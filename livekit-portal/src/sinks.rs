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

//! Where core events land: user callbacks plus the latest-wins slots behind
//! the `get_*` getters.
//!
//! Callbacks run on tokio workers. A panic in user code would kill the
//! task delivering events, so every call is caught and logged.

use std::collections::HashMap;
use std::fmt::Display;
use std::panic::{AssertUnwindSafe, catch_unwind};

use livekit_portal_core::Event;
use livekit_portal_core::keypoint::Keypoint;
use livekit_portal_core::types::{
    Action, Observation, State, Telemetry, TypedValue, VideoFrameData,
};
use parking_lot::Mutex;

type Callback<A> = Mutex<Option<Box<A>>>;
type DropCb = dyn Fn(Vec<HashMap<String, TypedValue>>) + Send + Sync;
type IdentityCb = dyn Fn(&str) + Send + Sync;
type OptIdentityCb = dyn Fn(Option<&str>) + Send + Sync;
type KeypointCb = dyn Fn(&Keypoint) + Send + Sync;
type TimeSyncedCb = dyn Fn() + Send + Sync;
type TelemetryCb = dyn Fn(&Telemetry) + Send + Sync;
type VideoCb = dyn Fn(&str, &VideoFrameData) + Send + Sync;

/// Run the registered callback, if any, logging a panic instead of letting
/// it take down the delivering task.
fn call<C: ?Sized>(slot: &Callback<C>, what: impl Display, run: impl FnOnce(&C)) {
    let cb = slot.lock();
    let Some(cb) = cb.as_deref() else {
        return;
    };
    if catch_unwind(AssertUnwindSafe(|| run(cb))).is_err() {
        log::error!("[callback-panic] {what} callback panicked, event loop continues");
    }
}

/// Push callback + latest-wins slot for one record type.
pub(crate) struct Slot<R> {
    pub cb: Callback<dyn Fn(&R) + Send + Sync>,
    latest: Mutex<Option<R>>,
}

impl<R> Default for Slot<R> {
    fn default() -> Self {
        Self { cb: Mutex::new(None), latest: Mutex::new(None) }
    }
}

impl<R: Clone> Slot<R> {
    /// Fire the callback by reference, then hand ownership to the
    /// latest-wins slot, which is updated even if the callback panics.
    fn deliver(&self, record: R, what: &str) {
        call(&self.cb, what, |cb| cb(&record));
        *self.latest.lock() = Some(record);
    }

    pub fn get(&self) -> Option<R> {
        self.latest.lock().clone()
    }

    fn clear(&self) {
        *self.latest.lock() = None;
    }
}

/// Push callback + latest-wins slot for one video track.
#[derive(Default)]
pub(crate) struct VideoTrackSlots {
    pub cb: Callback<VideoCb>,
    pub latest: Mutex<Option<VideoFrameData>>,
}

/// Push callback + latest-wins slot per sender, since any peer publishes
/// telemetry.
#[derive(Default)]
pub(crate) struct TelemetrySlots {
    pub cb: Callback<TelemetryCb>,
    latest: Mutex<HashMap<String, Telemetry>>,
}

impl TelemetrySlots {
    pub fn get(&self, sender: &str) -> Option<Telemetry> {
        self.latest.lock().get(sender).cloned()
    }
}

#[derive(Default)]
pub(crate) struct Sinks {
    pub action: Slot<Action>,
    pub state: Slot<State>,
    pub telemetry: TelemetrySlots,
    /// Latest-wins. Consumers that want history register `on_observation`
    /// and buffer on their own side.
    pub observation: Slot<Observation>,
    pub drop_cb: Callback<DropCb>,
    /// Fixed at construction, keyed by declared track name.
    pub video: HashMap<String, VideoTrackSlots>,
    pub keypoint_cb: Callback<KeypointCb>,
    pub time_synced_cb: Callback<TimeSyncedCb>,
    pub operator_joined_cb: Callback<IdentityCb>,
    pub operator_left_cb: Callback<IdentityCb>,
    pub active_operator_changed_cb: Callback<OptIdentityCb>,
}

impl Sinks {
    pub fn new<'a>(track_names: impl Iterator<Item = &'a str>) -> Self {
        let video = track_names.map(|n| (n.to_string(), VideoTrackSlots::default())).collect();
        Self { video, ..Self::default() }
    }

    pub fn dispatch(&self, event: Event<VideoFrameData>) {
        match event {
            Event::Action(action) => self.action.deliver(action, "action"),
            Event::State(state) => self.state.deliver(state, "state"),
            Event::Telemetry(telemetry) => {
                call(&self.telemetry.cb, "telemetry", |cb| cb(&telemetry));
                self.telemetry.latest.lock().insert(telemetry.sender.clone(), telemetry);
            }
            Event::Observation(obs) => self.observation.deliver(obs, "observation"),
            Event::VideoFrame { track, frame } => {
                let Some(slots) = self.video.get(&track) else {
                    return;
                };
                call(&slots.cb, format_args!("video frame (track '{track}')"), |cb| {
                    cb(&track, &frame)
                });
            }
            Event::Drops(drops) => call(&self.drop_cb, "drop", |cb| cb(drops)),
            Event::Keypoint(kp) => call(&self.keypoint_cb, "keypoint", |cb| cb(&kp)),
            Event::TimeSynced => call(&self.time_synced_cb, "on_time_synced", |cb| cb()),
            Event::OperatorJoined(id) => {
                call(&self.operator_joined_cb, "on_operator_joined", |cb| cb(&id))
            }
            Event::OperatorLeft(id) => {
                call(&self.operator_left_cb, "on_operator_left", |cb| cb(&id))
            }
            Event::ActiveOperatorChanged(id) => {
                let cb = &self.active_operator_changed_cb;
                call(cb, "on_active_operator_changed", |cb| cb(id.as_deref()))
            }
            // The awaiting `set_active_operator` reads the call's outcome
            // itself, and with native RPC the core hands application
            // requests to no one: LiveKit delivers them to their handlers.
            Event::RpcResult { .. } | Event::RpcRequest { .. } => {}
        }
    }

    /// The freshest frame for `get_video_frame`, set as frames arrive so
    /// polling consumers see the newest one even when callbacks fall behind.
    pub fn set_latest_frame(&self, track: &str, frame: &VideoFrameData) {
        if let Some(slots) = self.video.get(track) {
            *slots.latest.lock() = Some(frame.clone());
        }
    }

    /// Pre-reconnect or pre-disconnect data is stale: getters read `None`
    /// until fresh packets arrive.
    pub fn clear_latest(&self) {
        self.observation.clear();
        self.action.clear();
        self.state.clear();
        self.telemetry.latest.lock().clear();
        for slots in self.video.values() {
            *slots.latest.lock() = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::Map;

    use super::*;

    fn keypoint(kind: &str) -> Keypoint {
        Keypoint {
            kind: kind.into(),
            payload: Map::new(),
            timestamp_us: 1,
            sender: "teleop".into(),
        }
    }

    #[test]
    fn a_panicking_callback_does_not_propagate() {
        let sinks = Sinks::new(std::iter::empty());
        *sinks.keypoint_cb.lock() = Some(Box::new(|_| panic!("boom")));
        sinks.dispatch(Event::Keypoint(keypoint("idle")));
    }

    #[test]
    fn latest_slot_updates_even_if_the_callback_panics() {
        let sinks = Sinks::new(std::iter::empty());
        *sinks.state.cb.lock() = Some(Box::new(|_| panic!("boom")));
        let state = State { values: HashMap::new(), raw_values: HashMap::new(), timestamp_us: 7 };
        sinks.dispatch(Event::State(state));
        assert_eq!(sinks.state.get().map(|s| s.timestamp_us), Some(7));
        sinks.clear_latest();
        assert!(sinks.state.get().is_none());
    }

    #[test]
    fn latest_telemetry_is_kept_per_sender() {
        let sinks = Sinks::new(std::iter::empty());
        let telemetry = |sender: &str, ts| Telemetry {
            values: HashMap::new(),
            raw_values: HashMap::new(),
            timestamp_us: ts,
            sender: sender.into(),
        };
        sinks.dispatch(Event::Telemetry(telemetry("robot", 1)));
        sinks.dispatch(Event::Telemetry(telemetry("teleop", 2)));
        sinks.dispatch(Event::Telemetry(telemetry("robot", 3)));
        assert_eq!(sinks.telemetry.get("robot").map(|t| t.timestamp_us), Some(3));
        assert_eq!(sinks.telemetry.get("teleop").map(|t| t.timestamp_us), Some(2));
        sinks.clear_latest();
        assert!(sinks.telemetry.get("robot").is_none());
    }

    #[test]
    fn keypoints_reach_the_callback() {
        let sinks = Sinks::new(std::iter::empty());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        *sinks.keypoint_cb.lock() = Some(Box::new(move |kp| sink.lock().push(kp.kind.clone())));
        sinks.dispatch(Event::Keypoint(keypoint("idle")));
        assert_eq!(*seen.lock(), vec!["idle".to_string()]);
    }
}
