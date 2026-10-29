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

mod clock;
pub mod codec;
mod data;
mod driver;
mod frame_video;
mod jpeg;
mod portal;
pub mod rpc;
mod sinks;
mod video;
mod wire;

pub use livekit_portal_core::{config, config_file, dtype, error, metrics, types};

pub use codec::Codec;
pub use livekit_portal_core::config::{
    DEFAULT_H264_MAX_BITRATE_KBPS, FieldSpec, PortalConfig, Resolution, VideoOptions,
    VideoTrackSpec,
};
pub use livekit_portal_core::config_file::ConfigFileError;
pub use livekit_portal_core::dtype::DType;
pub use livekit_portal_core::error::{PortalError, PortalResult};
pub use livekit_portal_core::keypoint::Keypoint;
pub use livekit_portal_core::metrics::{
    BufferMetrics, PolicyMetrics, PortalMetrics, RttMetrics, SyncMetrics, TimeSyncMetrics,
    TransportMetrics,
};
pub use livekit_portal_core::types::{
    Action, ActionSubscription, FrameSource, Observation, Role, StallBehavior, StallConfig, State,
    SyncConfig, TimeSyncSource, TypedValue, VideoFrameData,
};
pub use portal::{
    ACTIVE_OPERATOR_ATTR_KEY, PROTOCOL_VERSION, PROTOCOL_VERSION_ATTR_KEY, Portal, ROLE_ATTR_KEY,
    SET_ACTIVE_OPERATOR_RPC,
};
pub use rpc::{RpcError, RpcHandler, RpcInvocationData};
pub use wire::BYTE_STREAM_CHUNK_SIZE;
