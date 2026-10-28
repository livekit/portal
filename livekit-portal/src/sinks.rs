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
use std::panic::{AssertUnwindSafe, catch_unwind};

use parking_lot::Mutex;
use portal_core::Event;
use portal_core::keypoint::Keypoint;
use portal_core::types::{Action, Observation, State, TypedValue, VideoFrameData};

type DropCb = Box<dyn Fn(Vec<HashMap<String, TypedValue>>) + Send + Sync>;
type IdentityCb = Box<dyn Fn(&str) + Send + Sync>;
type OptIdentityCb = Box<dyn Fn(Option<&str>) + Send + Sync>;
type KeypointCb = Box<dyn Fn(&Keypoint) + Send + Sync>;
type TimeSyncedCb = Box<dyn Fn() + Send + Sync>;
pub(crate) type VideoCb = Box<dyn Fn(&str, &VideoFrameData) + Send + Sync>;

/// Push callback + latest-wins slot for one record type.
pub(crate) struct Slot<R> {
    #[allow(clippy::type_complexity)]
    pub cb: Mutex<Option<Box<dyn Fn(&R) + Send + Sync>>>,
    pub latest: Mutex<Option<R>>,
}

impl<R: Clone> Slot<R> {
    fn new() -> Self {
        Self { cb: Mutex::new(None), latest: Mutex::new(None) }
    }

    /// Fire the callback by reference, then hand ownership to the
    /// latest-wins slot, which is updated even if the callback panics.
    fn deliver(&self, record: R, what: &str) {
        if let Some(cb) = self.cb.lock().as_ref()
            && catch_unwind(AssertUnwindSafe(|| cb(&record))).is_err()
        {
            log::error!("[callback-panic] {what} callback panicked, receive loop continues");
        }
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
pub(crate) struct VideoTrackSlots {
    pub cb: Mutex<Option<VideoCb>>,
    pub latest: Mutex<Option<VideoFrameData>>,
}

impl VideoTrackSlots {
    fn new() -> Self {
        Self { cb: Mutex::new(None), latest: Mutex::new(None) }
    }
}

pub(crate) struct Sinks {
    pub action: Slot<Action>,
    pub state: Slot<State>,
    /// Latest-wins. Consumers that want history register `on_observation`
    /// and buffer on their own side.
    pub observation: Slot<Observation>,
    pub drop_cb: Mutex<Option<DropCb>>,
    /// Fixed at construction, keyed by declared track name.
    pub video: HashMap<String, VideoTrackSlots>,
    pub keypoint_cb: Mutex<Option<KeypointCb>>,
    pub time_synced_cb: Mutex<Option<TimeSyncedCb>>,
    pub operator_joined_cb: Mutex<Option<IdentityCb>>,
    pub operator_left_cb: Mutex<Option<IdentityCb>>,
    pub active_operator_changed_cb: Mutex<Option<OptIdentityCb>>,
}

impl Sinks {
    pub fn new<'a>(track_names: impl Iterator<Item = &'a str>) -> Self {
        Self {
            action: Slot::new(),
            state: Slot::new(),
            observation: Slot::new(),
            drop_cb: Mutex::new(None),
            video: track_names.map(|n| (n.to_string(), VideoTrackSlots::new())).collect(),
            keypoint_cb: Mutex::new(None),
            time_synced_cb: Mutex::new(None),
            operator_joined_cb: Mutex::new(None),
            operator_left_cb: Mutex::new(None),
            active_operator_changed_cb: Mutex::new(None),
        }
    }

    pub fn dispatch(&self, event: Event<VideoFrameData>) {
        match event {
            Event::Action(action) => self.action.deliver(action, "action"),
            Event::State(state) => self.state.deliver(state, "state"),
            Event::Observation(obs) => self.observation.deliver(obs, "observation"),
            Event::VideoFrame { track, frame } => self.video_frame(&track, &frame),
            Event::Drops(drops) => {
                if let Some(cb) = self.drop_cb.lock().as_ref()
                    && catch_unwind(AssertUnwindSafe(|| cb(drops))).is_err()
                {
                    log::error!("[callback-panic] drop callback panicked, event loop continues");
                }
            }
            Event::Keypoint(kp) => self.keypoint(&kp),
            Event::TimeSynced => {
                if let Some(cb) = self.time_synced_cb.lock().as_ref()
                    && catch_unwind(AssertUnwindSafe(cb)).is_err()
                {
                    log::error!("[callback-panic] on_time_synced callback panicked");
                }
            }
            Event::OperatorJoined(id) => fire(&self.operator_joined_cb, &id, "on_operator_joined"),
            Event::OperatorLeft(id) => fire(&self.operator_left_cb, &id, "on_operator_left"),
            Event::ActiveOperatorChanged(id) => {
                if let Some(cb) = self.active_operator_changed_cb.lock().as_ref()
                    && catch_unwind(AssertUnwindSafe(|| cb(id.as_deref()))).is_err()
                {
                    log::error!("[callback-panic] on_active_operator_changed callback panicked");
                }
            }
        }
    }

    fn video_frame(&self, track: &str, frame: &VideoFrameData) {
        let Some(slots) = self.video.get(track) else {
            return;
        };
        if let Some(cb) = slots.cb.lock().as_ref()
            && catch_unwind(AssertUnwindSafe(|| cb(track, frame))).is_err()
        {
            log::error!(
                "[callback-panic] video frame callback panicked on track '{track}', receive loop continues"
            );
        }
    }

    fn keypoint(&self, keypoint: &Keypoint) {
        if let Some(cb) = self.keypoint_cb.lock().as_ref()
            && catch_unwind(AssertUnwindSafe(|| cb(keypoint))).is_err()
        {
            log::error!("[callback-panic] keypoint callback panicked, event loop continues");
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
        for slots in self.video.values() {
            *slots.latest.lock() = None;
        }
    }
}

fn fire(slot: &Mutex<Option<IdentityCb>>, identity: &str, what: &str) {
    if let Some(cb) = slot.lock().as_ref()
        && catch_unwind(AssertUnwindSafe(|| cb(identity))).is_err()
    {
        log::error!("[callback-panic] {what} callback panicked");
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
    fn keypoints_reach_the_callback() {
        let sinks = Sinks::new(std::iter::empty());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        *sinks.keypoint_cb.lock() = Some(Box::new(move |kp| sink.lock().push(kp.kind.clone())));
        sinks.dispatch(Event::Keypoint(keypoint("idle")));
        assert_eq!(*seen.lock(), vec!["idle".to_string()]);
    }
}
