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

//! Transport-free core of LiveKit Portal: config, wire format, observation
//! sync, clock math, presence and the active-operator gate. No sockets,
//! tasks or timers, so it builds for `wasm32-unknown-unknown` as well as
//! native. It speaks Portal's own transport model (`transport`), which a
//! driver maps onto its transport: `livekit-portal` drives it over the
//! LiveKit Rust SDK, and `loopback` runs it in-process for tests.

#[doc(hidden)]
pub mod clock;
pub mod codec;
pub mod config;
pub mod config_file;
#[doc(hidden)]
pub mod control;
mod core;
pub mod data;
pub mod dtype;
pub mod error;
pub mod frame;
pub mod frame_video;
#[doc(hidden)]
pub mod keypoint;
pub mod loopback;
pub mod metrics;
#[doc(hidden)]
pub mod placeholder;
pub mod presence;
pub mod rpc;
#[doc(hidden)]
pub mod serialization;
#[doc(hidden)]
pub mod sync_buffer;
pub mod transport;
pub mod types;

pub use crate::core::{
    ACTION_MAX_AGE_MS, CLOCK_MAX_AGE_MS, Connect, CoreInit, DEFAULT_RPC_TIMEOUT_MS, Event, Input,
    OpId, Output, PRESENCE_HEARTBEAT_US, PRESENCE_TIMEOUT_US, PerformRpc, PortalCore,
    RPC_CALLER_ROSTER_WAIT_US, RPC_NOT_AUTHORIZED, RPC_NOT_CONNECTED, RPC_PRESENCE_FAILED,
    RPC_RECIPIENT_DISCONNECTED, RPC_RESPONSE_TIMEOUT, RPC_UNSUPPORTED_METHOD, ReplyRpc, RpcId,
    RpcRequest, SET_ACTIVE_OPERATOR_RPC, SendAction, SendKeypoint, SendState, SendVideoFrame,
    SetActiveOperator, Step, VideoRoute, VideoTransport,
};
pub use codec::Codec;
pub use config::{
    DEFAULT_H264_MAX_BITRATE_KBPS, FieldSpec, PortalConfig, Resolution, VideoOptions,
    VideoTrackSpec,
};
pub use config_file::ConfigFileError;
pub use dtype::DType;
pub use error::{PortalError, PortalResult};
pub use frame::FramePayload;
pub use keypoint::Keypoint;
pub use metrics::{
    BufferMetrics, PolicyMetrics, PortalMetrics, RttMetrics, SyncMetrics, TimeSyncMetrics,
    TransportMetrics,
};
pub use rpc::{RpcError, RpcHandler, RpcInvocationData};
pub use transport::{
    Delivery, PROTOCOL_VERSION, PeerId, Presence, Stream, TransportCaps, VideoCaps, stream_path,
};
pub use types::{
    Action, ActionSubscription, FrameSource, Observation, Role, StallBehavior, StallConfig, State,
    SyncConfig, TimeSyncSource, TypedValue, VideoFrameData,
};
