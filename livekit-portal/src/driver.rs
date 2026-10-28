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

//! The room event actor: maps LiveKit `RoomEvent`s to core inputs and
//! fires the core's wake-ups. Media stays off this loop: video frames are
//! converted on their own tasks, and byte streams are read on spawned ones.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use livekit::prelude::*;
use livekit::webrtc::video_stream::native::NativeVideoStream;
use parking_lot::Mutex;
use portal_core::Input;
use portal_core::config::PortalConfig;
use portal_core::types::Role;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::frame_video::spawn_receive;
use crate::portal::Shared;
use crate::video::{VideoReceiver, VideoReceiverOptions};
use crate::wire;

/// Floor on a wake-up's delay. The core's times are on the raw local clock
/// and the timer on tokio's; this keeps a rounding gap between the two
/// from spinning the loop.
const MIN_WAKE_DELAY: Duration = Duration::from_millis(1);

pub(crate) struct EventLoop {
    pub shared: Arc<Shared>,
    pub events: mpsc::UnboundedReceiver<RoomEvent>,
    pub wake: watch::Receiver<Option<u64>>,
    pub video_receivers: Arc<Mutex<HashMap<String, VideoReceiver>>>,
    pub config: PortalConfig,
}

impl EventLoop {
    pub async fn run(mut self) {
        let mut deadline = self.deadline();
        loop {
            tokio::select! {
                event = self.events.recv() => {
                    let Some(event) = event else { break };
                    self.handle(event);
                }
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    // The core asks again if it still needs waking.
                    self.wake.borrow_and_update();
                    self.shared.step(Input::Wake);
                    deadline = self.deadline();
                    continue;
                }
                changed = self.wake.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
            deadline = self.deadline();
        }
    }

    /// The timer instant for the core's latest `ScheduleWake`.
    fn deadline(&mut self) -> Option<Instant> {
        let at_us = (*self.wake.borrow_and_update())?;
        let delay = Duration::from_micros(at_us.saturating_sub(self.shared.now_us()));
        Some(Instant::now() + delay.max(MIN_WAKE_DELAY))
    }

    fn handle(&self, event: RoomEvent) {
        match event {
            RoomEvent::TrackSubscribed { track, publication, .. } => {
                self.track_subscribed(track, publication)
            }
            RoomEvent::DataReceived { payload, topic: Some(topic), participant, .. } => {
                // Other applications' topics are theirs.
                let Some(stream) = wire::stream(&topic) else {
                    return;
                };
                let from = participant.as_ref().map(|p| p.identity());
                self.shared.step(Input::Data {
                    stream,
                    from: from.as_ref().map(|id| id.as_str()),
                    payload: &payload,
                });
            }
            RoomEvent::ByteStreamOpened { reader, topic, .. } => {
                // Take only Portal's streams, so other applications using
                // byte streams on unrelated topics are left untouched.
                if topic != wire::FRAME_VIDEO_TOPIC || !self.shared.receives_frame_bytes() {
                    return;
                }
                if let Some(reader) = reader.take() {
                    spawn_receive(reader, self.shared.clone());
                }
            }
            // The attribute map may still be empty if the participant has
            // not finished its own `set_attributes`; the attributes event
            // that follows reclassifies it.
            RoomEvent::ParticipantConnected(participant) => {
                let identity = participant.identity();
                let peer = identity.as_str();
                self.shared.step(Input::PeerJoined { peer });
                let presence = wire::presence(&participant.attributes());
                self.shared.step(Input::PresenceChanged { peer, presence });
            }
            // Includes this peer's own updates, which the core ignores.
            RoomEvent::ParticipantAttributesChanged { participant, .. } => {
                let identity = participant.identity();
                let presence = wire::presence(&participant.attributes());
                self.shared.step(Input::PresenceChanged { peer: identity.as_str(), presence });
            }
            RoomEvent::ParticipantDisconnected(participant) => {
                let identity = participant.identity();
                self.shared.step(Input::PeerLeft { peer: identity.as_str() });
            }
            RoomEvent::Reconnected => {
                // Pre-reconnect data is stale by definition: getters read
                // `None` until fresh packets arrive, matching the core's
                // cleared sync buffer.
                self.shared.sinks.clear_latest();
                self.shared.step(Input::LinkRestored);
            }
            _ => {}
        }
    }

    fn track_subscribed(&self, track: RemoteTrack, publication: RemoteTrackPublication) {
        if self.config.role() == Role::Robot {
            return;
        }
        let RemoteTrack::Video(video_track) = track else {
            return;
        };
        let name = publication.name();
        if !self.config.video_tracks().iter().any(|s| s.name == name) {
            return;
        }
        log::info!("[{}] subscribed to video track '{name}'", self.config.session());
        let metrics =
            self.shared.metrics.track(&name).expect("track metrics registered at construction");
        let receiver = VideoReceiver::spawn(VideoReceiverOptions {
            name: name.to_string(),
            stream: NativeVideoStream::new(video_track.rtc_track()),
            shared: self.shared.clone(),
            metrics,
        });
        self.video_receivers.lock().insert(name.to_string(), receiver);
    }
}
