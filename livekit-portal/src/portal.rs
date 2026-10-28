// Copyright 2026 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The native driver. Every decision lives in `PortalCore`; this module
//! connects the room, maps LiveKit events to core inputs (see `wire` for
//! the mapping) with the local time, and carries out the outputs as
//! LiveKit calls.
//!
//! The core trusts the identities it is handed. Here they are LiveKit
//! participant identities, which the SFU takes from each peer's JWT, so a
//! peer cannot speak for another.
//!
//! The core sits behind a mutex shared by the room event actor
//! (`driver.rs`), the video receive tasks and the synchronous send API.
//! Each holder takes the lock only to step the core and runs callbacks and
//! IO after releasing it, so a user callback may call back into `Portal`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use livekit::prelude::*;
use parking_lot::Mutex;
use portal_core::config::{FieldSpec, PortalConfig};
use portal_core::error::{PortalError, PortalResult};
use portal_core::frame_video::FrameVideoTracks;
use portal_core::keypoint::Keypoint;
use portal_core::metrics::{MetricsRegistry, PortalMetrics};
use portal_core::transport::{Delivery, Presence, Stream};
use portal_core::types::*;
use portal_core::{
    Connect, CoreInit, Input, OpId, Output, PortalCore, RpcId, RpcRequest, SendAction,
    SendKeypoint, SendState, SendVideoFrame, SetActiveOperator, Step, VideoTransport,
};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::clock::RawClock;
use crate::data::{Lanes, Publish, packet};
use crate::driver::EventLoop;
use crate::frame_video::{FrameVideoPublisher, FrameVideoPublisherOptions};
use crate::rpc::{RpcError, RpcHandler, RpcInvocationData};
use crate::sinks::Sinks;
use crate::video::{VideoPublisher, VideoPublisherOptions, VideoReceiver};
use crate::wire;

pub use crate::wire::{ACTIVE_OPERATOR_ATTR_KEY, PROTOCOL_VERSION_ATTR_KEY, ROLE_ATTR_KEY};
pub use portal_core::SET_ACTIVE_OPERATOR_RPC;
pub use portal_core::transport::PROTOCOL_VERSION;

/// State shared by `Portal`, the room event actor and the media tasks.
pub(crate) struct Shared {
    core: Mutex<PortalCore<VideoFrameData>>,
    pub sinks: Sinks,
    pub metrics: Arc<MetricsRegistry>,
    /// Declared frame-video tracks, to decode received frames off the lock.
    pub frame_video: FrameVideoTracks,
    clock: RawClock,
    /// Per-connection publish queues.
    lanes: Mutex<Option<Lanes>>,
    /// The latest `ScheduleWake`, read by the event actor.
    wake: watch::Sender<Option<u64>>,
    /// `set_active_operator` RPCs waiting on the core's reply.
    rpc_waiters: Mutex<HashMap<RpcId, oneshot::Sender<Result<String, RpcError>>>>,
    next_rpc: AtomicU64,
    local_participant: Mutex<Option<LocalParticipant>>,
    // Handle to the tokio runtime `connect()` ran on. Captured because
    // `register_rpc_method` can be called from a foreign thread with no
    // runtime context (e.g. a binding's asyncio loop); registering an RPC
    // method on a live LocalParticipant triggers publisher negotiation,
    // which `tokio::spawn`s and would otherwise panic with "no reactor
    // running". `None` before the first connect.
    runtime_handle: Mutex<Option<tokio::runtime::Handle>>,
    this: Weak<Shared>,
}

impl Shared {
    pub fn now_us(&self) -> u64 {
        self.clock.now_us()
    }

    pub fn receives_frame_bytes(&self) -> bool {
        self.core.lock().receives_frame_bytes()
    }

    /// Step the core and carry out what it returns, outside the lock.
    pub fn step(&self, input: Input<'_, VideoFrameData>) {
        let out = self.core.lock().handle(Step { input, now_us: self.now_us() });
        self.execute(out);
    }

    pub fn execute(&self, out: Vec<Output<VideoFrameData>>) {
        for output in out {
            match output {
                Output::Publish { stream, payload, delivery, to } => {
                    let Some(publish) = publish(Outgoing { stream, payload, delivery, to }) else {
                        continue;
                    };
                    if let Some(lanes) = self.lanes.lock().as_ref() {
                        lanes.send(stream, publish);
                    }
                }
                Output::SetPresence { id, presence } => self.spawn_set_presence(id, presence),
                // Only `Portal::set_active_operator` starts one, and it
                // performs the call itself.
                Output::Rpc { id, .. } => {
                    let result = Err(RpcError::new(1500, "unexpected rpc", None));
                    self.step(Input::RpcResponse { id, result });
                }
                Output::RpcReply { id, result } => {
                    if let Some(tx) = self.rpc_waiters.lock().remove(&id) {
                        let _ = tx.send(result);
                    }
                }
                Output::Emit(event) => self.sinks.dispatch(event),
                Output::ScheduleWake { at_us } => {
                    self.wake.send_replace(at_us);
                }
            }
        }
    }

    /// Presence updates the core starts on its own (an RPC-requested
    /// handoff) report back once LiveKit confirms the attribute write.
    fn spawn_set_presence(&self, id: Option<OpId>, presence: Presence) {
        let lp = self.local_participant.lock().clone();
        let runtime = self.runtime_handle.lock().clone();
        let (Some(lp), Some(runtime)) = (lp, runtime) else {
            if let Some(id) = id {
                self.step(Input::PresenceSet { id, result: Err("not connected".into()) });
            }
            return;
        };
        let shared = self.this.clone();
        let attributes = wire::active_operator_attributes(&presence);
        runtime.spawn(async move {
            let result = lp.set_attributes(attributes).await.map_err(|e| e.to_string());
            if let (Some(id), Some(shared)) = (id, shared.upgrade()) {
                shared.step(Input::PresenceSet { id, result });
            }
        });
    }
}

struct ConnectionState {
    room: Option<Room>,
    event_task: Option<JoinHandle<()>>,
}

pub struct Portal {
    config: PortalConfig,

    // Serializes connect()/disconnect() so a disconnect() yielding on
    // room.close().await can't be overtaken by a concurrent connect()
    // whose newly-populated state would then be clobbered by the
    // disconnect's cleanup path.
    lifecycle: tokio::sync::Mutex<()>,

    conn: Mutex<ConnectionState>,
    shared: Arc<Shared>,

    // Video receivers are spawned by the event actor (on TrackSubscribed)
    // and torn down by `disconnect`, so they live in an Arc shared with both.
    video_receivers: Arc<Mutex<HashMap<String, VideoReceiver>>>,
    video_publishers: Mutex<HashMap<String, Arc<VideoPublisher>>>,
    /// Robot-side: one publisher per declared frame-video track.
    frame_video_publishers: Mutex<HashMap<String, Arc<FrameVideoPublisher>>>,

    // RPC methods the caller has registered. Applied to the LocalParticipant
    // on connect(); survives disconnect so reconnects reapply them.
    rpc_handlers: Arc<Mutex<HashMap<String, RpcHandler>>>,
}

impl Portal {
    pub fn new(config: PortalConfig) -> Self {
        let core = PortalCore::new(CoreInit { config: config.clone(), caps: wire::CAPS });
        let metrics = core.metrics_registry().clone();
        let (wake, _) = watch::channel(None);
        let shared = Arc::new_cyclic(|this| Shared {
            sinks: Sinks::new(config.video_track_names()),
            metrics,
            frame_video: FrameVideoTracks::new(&config),
            clock: RawClock { source: config.time_sync_source(), skew_us: config.clock_skew_us() },
            core: Mutex::new(core),
            lanes: Mutex::new(None),
            wake,
            rpc_waiters: Mutex::new(HashMap::new()),
            next_rpc: AtomicU64::new(0),
            local_participant: Mutex::new(None),
            runtime_handle: Mutex::new(None),
            this: this.clone(),
        });
        Self {
            config,
            lifecycle: tokio::sync::Mutex::new(()),
            conn: Mutex::new(ConnectionState { room: None, event_task: None }),
            shared,
            video_receivers: Arc::new(Mutex::new(HashMap::new())),
            video_publishers: Mutex::new(HashMap::new()),
            frame_video_publishers: Mutex::new(HashMap::new()),
            rpc_handlers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn connect(&self, url: &str, token: &str) -> PortalResult<()> {
        let _lifecycle = self.lifecycle.lock().await;
        // Capture the runtime we're running on so `register_rpc_method` can
        // enter it when called later from a non-runtime (foreign) thread.
        *self.shared.runtime_handle.lock() = Some(tokio::runtime::Handle::current());
        if self.conn.lock().room.is_some() {
            return Err(PortalError::AlreadyConnected);
        }

        static OTHER_SDKS_VALUE: &str = concat!("portal:", env!("CARGO_PKG_VERSION"));

        let mut options = RoomOptions::default();
        options.sdk_options.other_sdks = Some(OTHER_SDKS_VALUE.to_string());
        options.auto_subscribe = true;
        if let Some(key) = self.config.e2ee_key() {
            use livekit::E2eeOptions;
            use livekit::e2ee::{
                EncryptionType,
                key_provider::{KeyProvider, KeyProviderOptions},
            };
            let key_provider =
                KeyProvider::with_shared_key(KeyProviderOptions::default(), key.to_vec());
            options.encryption =
                Some(E2eeOptions { key_provider, encryption_type: EncryptionType::Gcm });
        }

        let session = self.config.session();
        let role = self.config.role();
        log::info!("[{session}] connecting as {role:?} to {url}");

        // Hand decoded frames over as soon as they are ready instead of
        // holding them for smooth playback. Process-wide, and only possible
        // before the first WebRTC runtime starts.
        if let Err(e) = livekit::webrtc::enable_zero_playout_delay() {
            log::warn!("[playout-delay] zero playout delay not enabled: {e}");
        }

        let (room, events) = Room::connect(url, token, options)
            .await
            .map_err(|e| PortalError::Room(e.to_string()))?;

        // Store the LocalParticipant before applying handlers so a concurrent
        // `register_rpc_method` either (a) inserts before we iterate and gets
        // picked up, or (b) inserts after we've stored LP and forwards the
        // handler itself. Overlap is idempotent — the SDK's rpc handler map
        // is last-writer-wins.
        let local_participant = room.local_participant();
        *self.shared.local_participant.lock() = Some(local_participant.clone());
        *self.shared.lanes.lock() = Some(Lanes::new(&local_participant));

        // Robot-side: the built-in `set_active_operator` RPC. The core
        // decides; the handler only relays the call and its answer.
        if role == Role::Robot {
            let shared = Arc::downgrade(&self.shared);
            let handler: RpcHandler = Arc::new(move |data: RpcInvocationData| {
                let shared = shared.clone();
                Box::pin(async move { set_active_operator_rpc(shared, data).await })
            });
            self.register_rpc_method(SET_ACTIVE_OPERATOR_RPC, handler);
        }

        self.apply_rpc_handlers(&local_participant);

        // Self-set the role attribute so other participants can discover us.
        // Token-mint may also have set this key; in that case `set_attributes`
        // is effectively a no-op for the same value.
        if let Err(e) = local_participant.set_attributes(wire::role_attributes(role)).await {
            // Most common cause: the token grant did not include
            // `canUpdateOwnMetadata`. Surface a clear error so callers fix
            // their token-mint script rather than silently leaving the
            // participant unidentified. Roll back the partial state we
            // already wrote so a retry starts from a clean slate.
            self.teardown();
            let _ = room.close().await;
            return Err(PortalError::Room(format!(
                "failed to publish role attribute (token may be missing canUpdateOwnMetadata): {e}"
            )));
        }

        let peers = room
            .remote_participants()
            .into_values()
            .map(|p| (p.identity().as_str().to_string(), wire::presence(&p.attributes())))
            .collect();
        let connected = self.shared.core.lock().connect(Connect {
            local_peer: local_participant.identity().as_str().to_string(),
            // A token can seed the pointer, so the gate holds before anyone
            // calls `set_active_operator`.
            active_operator: wire::active_operator(&local_participant.attributes()),
            peers,
            now_us: self.shared.now_us(),
        });
        let out = match connected {
            Ok(out) => out,
            Err(e) => {
                self.teardown();
                let _ = room.close().await;
                return Err(e);
            }
        };
        self.shared.execute(out);

        if role == Role::Robot
            && let Err(e) = self.setup_robot(&local_participant).await
        {
            // Publishing a video track can fail mid-way; undo everything
            // written above before bailing.
            self.teardown();
            let _ = room.close().await;
            return Err(e);
        }

        log::info!("[{session}] connected as {role:?}");

        let event_loop = EventLoop {
            shared: self.shared.clone(),
            events,
            wake: self.shared.wake.subscribe(),
            video_receivers: self.video_receivers.clone(),
            config: self.config.clone(),
        };
        let event_task = tokio::spawn(event_loop.run());

        let mut state = self.conn.lock();
        state.room = Some(room);
        state.event_task = Some(event_task);
        Ok(())
    }

    pub fn send_video_frame(
        &self,
        track_name: &str,
        rgb_data: &[u8],
        width: u32,
        height: u32,
        timestamp_us: Option<u64>,
    ) -> PortalResult<()> {
        let route = self.shared.core.lock().send_video_frame(SendVideoFrame {
            track: track_name,
            timestamp_us,
            now_us: self.shared.now_us(),
        })?;
        // Publishers exist only while connected; before that the track has
        // nowhere to go.
        let unknown = || PortalError::UnknownVideoTrack { name: track_name.to_string() };
        match route.transport {
            VideoTransport::TransportEncoded => {
                let publisher =
                    self.video_publishers.lock().get(track_name).cloned().ok_or_else(unknown)?;
                publisher.send_frame(rgb_data, width, height, route.timestamp_us)
            }
            VideoTransport::ByteFrames { .. } => {
                let publisher = self
                    .frame_video_publishers
                    .lock()
                    .get(track_name)
                    .cloned()
                    .ok_or_else(unknown)?;
                publisher.send_frame(rgb_data, width, height, route.timestamp_us)
            }
            // LiveKit encodes video itself (`wire::CAPS`), so the core never
            // routes a frame to a driver encoder here.
            VideoTransport::DriverEncoded { codec } => Err(PortalError::Unsupported(format!(
                "no driver-side {codec:?} encoder on LiveKit"
            ))),
        }
    }

    /// Publish a state sample (robot only). Values are typed — build the
    /// map with `TypedValue::Bool(true)`, `0.5f32.into()`, etc. The
    /// pipeline internally widens to `f64` for carry-forward and casts
    /// back to the declared dtype at the wire boundary. `timestamp_us`
    /// defaults to `now_us()`.
    pub fn send_state(
        &self,
        values: &HashMap<String, TypedValue>,
        timestamp_us: Option<u64>,
    ) -> PortalResult<()> {
        let now_us = self.shared.now_us();
        let out = self.shared.core.lock().send_state(SendState { values, timestamp_us, now_us })?;
        self.shared.execute(out);
        Ok(())
    }

    /// Publish an action (operator only).
    ///
    /// `in_reply_to_ts_us` is the timestamp of the observation this action
    /// was produced from — pass `Some(obs.timestamp_us)` to give the
    /// receiver the data it needs to compute true end-to-end policy
    /// latency (`metrics.policy.e2e_us_*`). Pass `None` for unsolicited
    /// publishes (teleop, idle commands).
    pub fn send_action(
        &self,
        values: &HashMap<String, TypedValue>,
        timestamp_us: Option<u64>,
        in_reply_to_ts_us: Option<u64>,
    ) -> PortalResult<()> {
        let now_us = self.shared.now_us();
        let out = self.shared.core.lock().send_action(SendAction {
            values,
            timestamp_us,
            in_reply_to_ts_us,
            now_us,
        })?;
        self.shared.execute(out);
        Ok(())
    }

    /// Declared state schema (field names + dtypes), in declaration order.
    /// Bindings mirror this snapshot internally; reading from the Portal
    /// keeps the snapshot single-sourced.
    pub fn state_schema(&self) -> &[FieldSpec] {
        self.config.state_schema()
    }

    /// Declared action schema, same semantics as `state_schema`.
    pub fn action_schema(&self) -> &[FieldSpec] {
        self.config.action_schema()
    }

    // --- Multi-controller surface (v0.2) ---

    /// This Portal's own LiveKit identity once connected. Reads from the
    /// stored `LocalParticipant`. `None` before `connect()` succeeds.
    pub fn local_identity(&self) -> Option<String> {
        self.shared.local_participant.lock().as_ref().map(|lp| lp.identity().as_str().to_string())
    }

    /// Identity of the operator the robot is currently listening to, or
    /// `None` if no operator is selected. On Robot side this is the local
    /// pointer (also broadcast as the `lk.portal.active_operator` attribute).
    /// On Operator side it is a mirror of the robot's attribute.
    pub fn active_operator(&self) -> Option<String> {
        self.shared.core.lock().active_operator()
    }

    /// Set the active operator. On Robot side this updates the local pointer
    /// and broadcasts via the robot's own attributes. On Operator side this
    /// dispatches a `portal.set_active_operator` RPC to the robot.
    ///
    /// Pass `None` to clear and drop all incoming actions.
    pub async fn set_active_operator(&self, identity: Option<String>) -> PortalResult<()> {
        if self.config.role() != Role::Robot {
            // The common pattern is to call this right after `connect()`,
            // before the robot's attributes have surfaced. Wait briefly for
            // it rather than forcing every caller to.
            self.resolve_robot_identity().await?;
        }
        let now_us = self.shared.now_us();
        let (_, out) =
            self.shared.core.lock().set_active_operator(SetActiveOperator { identity, now_us })?;
        for output in out {
            match output {
                Output::SetPresence { id: Some(id), presence } => {
                    let lp = self
                        .shared
                        .local_participant
                        .lock()
                        .clone()
                        .ok_or(PortalError::NotConnected)?;
                    let attributes = wire::active_operator_attributes(&presence);
                    let result = lp.set_attributes(attributes).await.map_err(|e| e.to_string());
                    self.shared.step(Input::PresenceSet { id, result: result.clone() });
                    result.map_err(PortalError::Room)?;
                }
                Output::Rpc { id, to, method, payload, timeout_ms } => {
                    let timeout = timeout_ms.map(|ms| Duration::from_millis(ms.into()));
                    let result = self.perform_rpc(Some(&to), &method, payload, timeout).await;
                    let core_result = match &result {
                        Ok(s) => Ok(s.clone()),
                        Err(PortalError::Rpc(e)) => Err(e.clone()),
                        Err(e) => Err(RpcError::new(1500, e.to_string(), None)),
                    };
                    self.shared.step(Input::RpcResponse { id, result: core_result });
                    result?;
                }
                other => self.shared.execute(vec![other]),
            }
        }
        Ok(())
    }

    /// Currently-connected operator identities (excluding self).
    pub fn operators(&self) -> Vec<String> {
        self.shared.core.lock().operators()
    }

    /// Currently-connected observer identities (excluding self). Observers
    /// never drive, so they are listed apart from `operators()`.
    pub fn observers(&self) -> Vec<String> {
        self.shared.core.lock().observers()
    }

    /// Identity of the robot in the room, or `None` if none has been seen.
    /// Operator-side helper, derived from the robot's `lk.portal.role`
    /// attribute.
    pub fn robot_identity(&self) -> Option<String> {
        self.shared.core.lock().robot_identity()
    }

    /// Fire when an operator joins the room. Identity is the new operator's
    /// participant identity. Only one callback is stored; subsequent calls
    /// overwrite.
    pub fn on_operator_joined(&self, callback: impl Fn(&str) + Send + Sync + 'static) {
        *self.shared.sinks.operator_joined_cb.lock() = Some(Box::new(callback));
    }

    /// Fire when an operator leaves the room. The robot's `active_operator`
    /// attribute is **not** auto-cleared on disconnect; the pointer stays
    /// pinned so a reconnect with the same identity resumes control.
    pub fn on_operator_left(&self, callback: impl Fn(&str) + Send + Sync + 'static) {
        *self.shared.sinks.operator_left_cb.lock() = Some(Box::new(callback));
    }

    /// Fire when the robot's `active_operator` attribute changes (or, on the
    /// Robot side, when the local pointer is updated via `set_active_operator`
    /// or the RPC handler). The argument is the new identity, or `None` if
    /// the pointer was cleared.
    pub fn on_active_operator_changed(
        &self,
        callback: impl Fn(Option<&str>) + Send + Sync + 'static,
    ) {
        *self.shared.sinks.active_operator_changed_cb.lock() = Some(Box::new(callback));
    }

    /// Register an RPC method handler. Handlers can be registered before or
    /// after `connect()`; stored handlers are (re)applied to the
    /// `LocalParticipant` on each connect.
    pub fn register_rpc_method(&self, method: &str, handler: RpcHandler) {
        self.rpc_handlers.lock().insert(method.to_string(), handler.clone());
        let Some(lp) = self.shared.local_participant.lock().clone() else {
            return;
        };
        // The SDK's `register_rpc_method` kicks off publisher negotiation,
        // which `tokio::spawn`s internally. If we were called from a
        // foreign thread with no runtime context (a binding's asyncio
        // loop), that spawn panics. Enter the runtime `connect()` ran on
        // so the spawn lands on it. The handle is always present here
        // (an LP exists only after a successful connect set it), but fall
        // back to a bare call rather than panicking if it somehow isn't.
        match self.shared.runtime_handle.lock().clone() {
            Some(handle) => {
                let _guard = handle.enter();
                register_handler_on(&lp, method.to_string(), handler);
            }
            None => register_handler_on(&lp, method.to_string(), handler),
        }
    }

    /// Remove a previously registered RPC method handler.
    pub fn unregister_rpc_method(&self, method: &str) {
        self.rpc_handlers.lock().remove(method);
        if let Some(lp) = self.shared.local_participant.lock().clone() {
            lp.unregister_rpc_method(method.to_string());
        }
    }

    /// Invoke a registered method on the peer. `destination` is optional;
    /// when omitted, the call is routed to the obvious counterpart — robot
    /// for an Operator, the active operator for a Robot — falling back to
    /// the single remote participant if neither pointer is set yet. Errors
    /// with `NoPeer` or `AmbiguousPeer` when no unique destination
    /// resolves.
    pub async fn perform_rpc(
        &self,
        destination: Option<&str>,
        method: &str,
        payload: String,
        response_timeout: Option<Duration>,
    ) -> PortalResult<String> {
        let destination = match destination {
            Some(id) => id.to_string(),
            None => self.resolve_peer()?,
        };
        let lp = self.shared.local_participant.lock().clone().ok_or(PortalError::NotConnected)?;

        let mut data = PerformRpcData {
            destination_identity: destination,
            method: method.to_string(),
            payload,
            ..Default::default()
        };
        if let Some(t) = response_timeout {
            data.response_timeout = t;
        }

        lp.perform_rpc(data).await.map_err(|e| PortalError::Rpc(crate::rpc::rpc_error_from_sdk(e)))
    }

    /// Walk `room.remote_participants()` looking for one whose attributes
    /// declare `role=robot`, and tell the core about it so later calls take
    /// the fast path.
    fn find_robot_in_room(&self) -> Option<String> {
        let (identity, presence) = {
            let conn = self.conn.lock();
            let room = conn.room.as_ref()?;
            room.remote_participants().into_values().find_map(|p| {
                let presence = wire::presence(&p.attributes());
                (presence.compatible_role() == Some(Role::Robot))
                    .then(|| (p.identity().as_str().to_string(), presence))
            })?
        };
        self.shared.step(Input::PresenceChanged { peer: &identity, presence });
        Some(identity)
    }

    /// Resolve the robot's identity for an operator-side RPC. Tries the
    /// core's roster first, then a synchronous scan of the room, then a
    /// short polling loop so `set_active_operator` works immediately after
    /// `connect()` without racing the initial attribute-propagation event.
    /// Returns `NoPeer` after the timeout if no robot ever appears.
    async fn resolve_robot_identity(&self) -> PortalResult<String> {
        let lookup = || self.robot_identity().or_else(|| self.find_robot_in_room());
        if let Some(id) = lookup() {
            return Ok(id);
        }
        // Poll for ~1.5s in 50ms ticks. On a healthy LAN the first
        // ParticipantAttributesChanged event lands well within this window.
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(id) = lookup() {
                return Ok(id);
            }
        }
        Err(PortalError::NoPeer)
    }

    /// Resolve a default destination for `perform_rpc(None, ...)`: the
    /// core's counterpart first, then a single-remote-participant snapshot
    /// for setups that haven't designated control yet.
    fn resolve_peer(&self) -> PortalResult<String> {
        if let Some(id) = self.shared.core.lock().default_peer() {
            return Ok(id);
        }
        let conn = self.conn.lock();
        let room = conn.room.as_ref().ok_or(PortalError::NotConnected)?;
        let remotes = room.remote_participants();
        match remotes.len() {
            0 => Err(PortalError::NoPeer),
            1 => {
                let (id, _) = remotes.into_iter().next().expect("remotes has one entry");
                Ok(id.as_str().to_string())
            }
            _ => Err(PortalError::AmbiguousPeer),
        }
    }

    /// Apply every stored handler to a freshly-connected LocalParticipant.
    fn apply_rpc_handlers(&self, lp: &LocalParticipant) {
        let handlers = self.rpc_handlers.lock().clone();
        for (method, handler) in handlers {
            register_handler_on(lp, method, handler);
        }
    }

    /// Per-connection cleanup shared by `disconnect()` and a failed
    /// `connect()`, which leaves the room for its caller to close.
    fn teardown(&self) {
        *self.shared.local_participant.lock() = None;
        // Rosters, the active-operator mirror, publishers and the sync
        // buffer are per-connection, so the next connect starts clean.
        let out = self.shared.core.lock().disconnect();
        self.shared.execute(out);
        *self.shared.lanes.lock() = None;
        {
            let mut receivers = self.video_receivers.lock();
            for receiver in receivers.values() {
                receiver.abort();
            }
            receivers.clear();
        }
        self.video_publishers.lock().clear();
        self.frame_video_publishers.lock().clear();
        self.shared.sinks.clear_latest();
    }

    pub async fn disconnect(&self) -> PortalResult<()> {
        let _lifecycle = self.lifecycle.lock().await;
        let room = self.conn.lock().room.take();
        log::info!("disconnecting");

        // close() is best-effort; cleanup must happen even if it errors,
        // otherwise the Portal would be half-disconnected (room=None but
        // tasks/publishers still running) and the next connect() would race.
        let close_result = match room {
            Some(room) => room.close().await.map_err(|e| PortalError::Room(e.to_string())),
            None => Ok(()),
        };

        if let Some(task) = self.conn.lock().event_task.take() {
            task.abort();
        }
        self.teardown();
        close_result
    }

    // --- Pull API (latest-wins, peek semantics) ---

    /// Latest observation, or `ObservationSyncDisabled` when observation
    /// sync is off. Consumers wanting a history of observations should
    /// register `on_observation` and buffer on their own side.
    pub fn get_observation(&self) -> PortalResult<Option<Observation>> {
        self.require_observation_sync()?;
        Ok(self.shared.sinks.observation.get())
    }

    fn require_observation_sync(&self) -> PortalResult<()> {
        match self.config.observation_sync() {
            true => Ok(()),
            false => Err(PortalError::ObservationSyncDisabled),
        }
    }

    /// Clone of the latest action received (Robot side), or `None`.
    /// `.values` holds typed values per the declared schema; `.raw_values`
    /// is the lossless `f64` view.
    pub fn get_action(&self) -> Option<Action> {
        self.shared.sinks.action.get()
    }

    /// Clone of the latest state received (Operator side), or `None`.
    /// Typed per the declared schema.
    pub fn get_state(&self) -> Option<State> {
        self.shared.sinks.state.get()
    }

    /// Clone of the latest frame received for `track_name`, or `None`.
    pub fn get_video_frame(&self, track_name: &str) -> Option<VideoFrameData> {
        self.shared.sinks.video.get(track_name).and_then(|s| s.latest.lock().clone())
    }

    // --- Callback registration (push API) ---

    /// Fire on every received action. The `Action` record exposes typed
    /// values per the declared schema plus `raw_values` for the lossless
    /// `f64` view.
    pub fn on_action(&self, callback: impl Fn(&Action) + Send + Sync + 'static) {
        *self.shared.sinks.action.cb.lock() = Some(Box::new(callback));
    }

    pub fn on_observation(
        &self,
        callback: impl Fn(&Observation) + Send + Sync + 'static,
    ) -> PortalResult<()> {
        self.require_observation_sync()?;
        *self.shared.sinks.observation.cb.lock() = Some(Box::new(callback));
        Ok(())
    }

    /// Whether this peer bundles observations. See
    /// `PortalConfig::set_observation_sync`.
    pub fn observation_sync(&self) -> bool {
        self.config.observation_sync()
    }

    /// Fire on every received state. Semantics mirror `on_action`.
    pub fn on_state(&self, callback: impl Fn(&State) + Send + Sync + 'static) {
        *self.shared.sinks.state.cb.lock() = Some(Box::new(callback));
    }

    pub fn on_video_frame(
        &self,
        track_name: &str,
        callback: impl Fn(&str, &VideoFrameData) + Send + Sync + 'static,
    ) {
        match self.shared.sinks.video.get(track_name) {
            Some(slots) => *slots.cb.lock() = Some(Box::new(callback)),
            None => log::warn!(
                "[unknown-track] on_video_frame: track '{track_name}' not registered, callback ignored"
            ),
        }
    }

    /// Fire on every batch of state samples that couldn't be matched to a
    /// video frame. Each entry is the typed state payload (same shape as
    /// `Observation.state`).
    pub fn on_drop(
        &self,
        callback: impl Fn(Vec<HashMap<String, TypedValue>>) + Send + Sync + 'static,
    ) {
        *self.shared.sinks.drop_cb.lock() = Some(Box::new(callback));
    }

    // --- Internal ---

    async fn setup_robot(&self, lp: &LocalParticipant) -> PortalResult<()> {
        let session = self.config.session();
        for spec in self.config.video_tracks().iter().filter(|s| s.is_webrtc()) {
            let track_name = &spec.name;
            let metrics = self
                .shared
                .metrics
                .track(track_name)
                .expect("track metrics registered at construction");
            let fps = self.config.fps();
            let publisher = VideoPublisher::new(VideoPublisherOptions { spec, metrics, fps });
            publisher.publish(lp).await?;
            log::info!("[{session}] published video track '{track_name}'");
            self.video_publishers.lock().insert(track_name.clone(), Arc::new(publisher));
        }

        // Frame-video publishers don't publish a track: they emit one byte
        // stream per frame, so there is only a drainer task to start.
        for spec in self.config.frame_video_tracks() {
            let metrics = self
                .shared
                .metrics
                .track(&spec.name)
                .expect("track metrics registered at construction");
            let publisher = FrameVideoPublisher::new(FrameVideoPublisherOptions {
                spec: spec.clone(),
                local_participant: lp.clone(),
                metrics,
            });
            self.frame_video_publishers.lock().insert(spec.name.clone(), Arc::new(publisher));
        }
        Ok(())
    }

    /// Snapshot of metrics since construction or the last `reset_metrics()`.
    pub fn metrics(&self) -> PortalMetrics {
        self.shared.core.lock().metrics()
    }

    /// Resets counters. Time sync state is kept: see `TimeSyncMetrics`.
    pub fn reset_metrics(&self) {
        self.shared.core.lock().reset_metrics();
    }

    /// Now, in microseconds on the robot's clock. On the robot this is its
    /// own clock. Before the first sync it is local time, and at the first
    /// sync it switches to the robot's clock in one step, possibly backwards.
    /// After that it never repeats and never goes backwards.
    pub fn now_us(&self) -> u64 {
        let raw = self.shared.now_us();
        self.shared.core.lock().now_us(raw)
    }

    /// Send an annotation to everyone in the room, including this peer's own
    /// `on_keypoint`. `timestamp_us` is where the mark belongs and defaults to
    /// `now_us()`; a teleoperator passes the timestamp of the frame on screen.
    ///
    /// Portal doesn't interpret `kind` or `payload`. Returns the observers
    /// that were present, so an empty list tells the sender no one recorded
    /// the mark. The encoded keypoint must fit in one data packet (about
    /// 64 KB on a default server); a larger one fails with the transport's error.
    pub async fn send_keypoint(
        &self,
        kind: &str,
        payload: serde_json::Map<String, serde_json::Value>,
        timestamp_us: Option<u64>,
    ) -> PortalResult<Vec<String>> {
        let lp = self.shared.local_participant.lock().clone().ok_or(PortalError::NotConnected)?;
        let now_us = self.shared.now_us();
        let out = self.shared.core.lock().send_keypoint(SendKeypoint {
            kind,
            payload,
            timestamp_us,
            now_us,
        })?;
        for output in out {
            match output {
                Output::Publish { stream: Stream::Keypoint, payload, delivery, to } => {
                    let stream = Stream::Keypoint;
                    let Some(publish) = publish(Outgoing { stream, payload, delivery, to }) else {
                        continue;
                    };
                    lp.publish_data(packet(publish))
                        .await
                        .map_err(|e| PortalError::Room(e.to_string()))?;
                }
                // LiveKit doesn't fan a publisher's own packets back, and a
                // recorder that marks its own takes still has to see them.
                other => self.shared.execute(vec![other]),
            }
        }
        Ok(self.observers())
    }

    /// Fire on every keypoint in the room, this peer's own included.
    pub fn on_keypoint(&self, callback: impl Fn(&Keypoint) + Send + Sync + 'static) {
        *self.shared.sinks.keypoint_cb.lock() = Some(Box::new(callback));
    }

    /// Fires on the first sync with the robot's clock, and after each resync.
    pub fn on_time_synced(&self, callback: impl Fn() + Send + Sync + 'static) {
        *self.shared.sinks.time_synced_cb.lock() = Some(Box::new(callback));
    }
}

/// The fields of an `Output::Publish`.
struct Outgoing {
    stream: Stream,
    payload: Vec<u8>,
    delivery: Delivery,
    to: Option<String>,
}

/// A core publish as a LiveKit data packet: the stream's topic, reliable
/// only for `Delivery::Reliable` (`LatestOnly` and `BestEffort` both ride
/// the lossy channel), and LiveKit's own addressing for a hinted peer.
fn publish(outgoing: Outgoing) -> Option<Publish> {
    let Outgoing { stream, payload, delivery, to } = outgoing;
    let Some(topic) = wire::topic(stream) else {
        log::warn!("[publish-failed] stream '{}' has no LiveKit topic", stream.name());
        return None;
    };
    let reliable = delivery == Delivery::Reliable;
    Some(Publish { topic, payload, reliable, destinations: to.into_iter().collect() })
}

/// Wrap a Portal `RpcHandler` in the signature the SDK expects and install
/// it on the given LocalParticipant. Payload types are converted at the
/// boundary — the SDK's `RpcInvocationData` / `RpcError` never leak into
/// caller-facing code.
fn register_handler_on(lp: &LocalParticipant, method: String, handler: RpcHandler) {
    lp.register_rpc_method(method, move |data| {
        let handler = handler.clone();
        Box::pin(async move {
            let core_data = crate::rpc::invocation_from_sdk(data);
            handler(core_data).await.map_err(crate::rpc::rpc_error_to_sdk)
        })
    });
}

/// Relay a `portal.set_active_operator` call to the core and wait for its
/// answer. The core may hold the call while an unknown caller's role
/// attribute catches up with it.
async fn set_active_operator_rpc(
    shared: Weak<Shared>,
    data: RpcInvocationData,
) -> Result<String, RpcError> {
    let not_connected =
        || RpcError::new(portal_core::RPC_NOT_CONNECTED, "robot not connected", None);
    let rx = {
        let shared = shared.upgrade().ok_or_else(not_connected)?;
        let id = RpcId(shared.next_rpc.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        shared.rpc_waiters.lock().insert(id, tx);
        shared.step(Input::RpcRequest(RpcRequest {
            id,
            method: SET_ACTIVE_OPERATOR_RPC,
            from: &data.caller_identity,
            payload: &data.payload,
        }));
        rx
    };
    rx.await.unwrap_or_else(|_| Err(not_connected()))
}
