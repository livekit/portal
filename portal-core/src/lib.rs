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
//! sync and clock math. No sockets, tasks or timers, so it builds for
//! `wasm32-unknown-unknown` as well as native. `livekit-portal` drives it
//! over the LiveKit Rust SDK.

#[doc(hidden)]
pub mod clock;
pub mod codec;
pub mod config;
pub mod config_file;
mod core;
pub mod data;
pub mod dtype;
pub mod error;
pub mod frame;
pub mod frame_video;
#[doc(hidden)]
pub mod keypoint;
pub mod metrics;
#[doc(hidden)]
pub mod placeholder;
pub mod presence;
pub mod rpc;
#[doc(hidden)]
pub mod serialization;
#[doc(hidden)]
pub mod sync_buffer;
pub mod types;

pub use crate::core::{
    Connected, DataReceived, Event, Input, OpId, Output, ParticipantInfo, PortalCore,
    RPC_CALLER_ROSTER_WAIT_US, RPC_NOT_AUTHORIZED, RPC_NOT_CONNECTED, RPC_SET_ATTRIBUTES_FAILED,
    RpcId, RpcRequest, SET_ACTIVE_OPERATOR_RPC, SendAction, SendKeypoint, SendState,
    SendVideoFrame, SetActiveOperator, Step, VideoRoute, VideoTransport,
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
pub use frame_video::{BYTE_STREAM_CHUNK_SIZE, FRAME_VIDEO_TOPIC};
pub use keypoint::Keypoint;
pub use metrics::{
    BufferMetrics, PolicyMetrics, PortalMetrics, RttMetrics, SyncMetrics, TimeSyncMetrics,
    TransportMetrics,
};
pub use presence::{
    ACTIVE_OPERATOR_ATTR_KEY, PROTOCOL_VERSION, PROTOCOL_VERSION_ATTR_KEY, ROLE_ATTR_KEY,
};
pub use rpc::{RpcError, RpcHandler, RpcInvocationData};
pub use types::{
    Action, ActionSubscription, FrameSource, Observation, Role, StallBehavior, StallConfig, State,
    SyncConfig, TimeSyncSource, TypedValue, VideoFrameData,
};
