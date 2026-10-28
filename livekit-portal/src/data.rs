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

//! Data packets out. Each Portal topic gets its own bounded queue and send
//! task, so the synchronous send path never blocks on the network and a
//! backlog on one topic can't delay another (a clock pong stuck behind
//! state packets would skew the round trip).

use livekit::prelude::*;
use portal_core::transport::Stream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Bound on the in-flight publish queue. Sized for ~10s at 100Hz so normal
/// operation never hits it. The bound exists so a stalled publish loop
/// (slow SFU, lossy link) cannot grow memory without limit; on overflow we
/// drop and warn rather than block the synchronous send path.
const PUBLISH_QUEUE_CAP: usize = 1024;
/// Clock packets are periodic; a lost one only delays the estimate.
const CLOCK_QUEUE_CAP: usize = 64;

struct Lane {
    tx: mpsc::Sender<DataPacket>,
    task: JoinHandle<()>,
}

impl Lane {
    fn spawn(local_participant: LocalParticipant, capacity: usize) -> Self {
        let (tx, mut rx) = mpsc::channel::<DataPacket>(capacity);
        let task = tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                if let Err(e) = local_participant.publish_data(packet).await {
                    log::warn!("[publish-failed] data publish failed: {e}");
                }
            }
        });
        Self { tx, task }
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Per-connection publish queues.
pub(crate) struct Lanes {
    state: Lane,
    action: Lane,
    clock: Lane,
}

pub(crate) struct Publish {
    pub topic: &'static str,
    pub payload: Vec<u8>,
    pub reliable: bool,
    pub destinations: Vec<String>,
}

impl Lanes {
    pub fn new(local_participant: &LocalParticipant) -> Self {
        Self {
            state: Lane::spawn(local_participant.clone(), PUBLISH_QUEUE_CAP),
            action: Lane::spawn(local_participant.clone(), PUBLISH_QUEUE_CAP),
            clock: Lane::spawn(local_participant.clone(), CLOCK_QUEUE_CAP),
        }
    }

    /// Queue a packet without blocking. Streams without a lane (keypoints)
    /// are published by their caller, which awaits the result.
    pub fn send(&self, stream: Stream, publish: Publish) {
        let lane = match stream {
            Stream::State => &self.state,
            Stream::Action => &self.action,
            Stream::Clock => &self.clock,
            Stream::Keypoint | Stream::Control | Stream::Presence => {
                log::warn!("[publish-failed] no queue for stream '{}'", stream.name());
                return;
            }
        };
        let topic = publish.topic;
        let quiet = stream == Stream::Clock;
        match lane.tx.try_send(packet(publish)) {
            Ok(()) => {}
            // Clock packets are dropped quietly: the next ping replaces them.
            Err(mpsc::error::TrySendError::Full(_)) if !quiet => {
                log::warn!(
                    "[publish-full] topic '{topic}' queue full (cap={PUBLISH_QUEUE_CAP}), dropping packet"
                );
            }
            // Full clock lane, or the send task is gone (disconnect): the
            // caller is already in teardown.
            Err(_) => {}
        }
    }
}

pub(crate) fn packet(publish: Publish) -> DataPacket {
    let Publish { topic, payload, reliable, destinations } = publish;
    DataPacket {
        payload,
        topic: Some(topic.to_string()),
        reliable,
        destination_identities: destinations.into_iter().map(Into::into).collect(),
    }
}
