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

pub mod codec;
pub mod config;
pub mod config_file;
mod data;
pub mod dtype;
pub mod error;
mod frame_video;
pub mod metrics;
mod placeholder;
mod portal;
pub mod rpc;
mod rtt;
mod serialization;
mod sync_buffer;
pub mod types;
mod video;

pub use codec::Codec;
pub use config::{
    ChunkSpec, DEFAULT_H264_MAX_BITRATE_KBPS, FieldSpec, FrameVideoSpec, PortalConfig,
    VideoTrackSpec,
};
pub use config_file::ConfigFileError;
pub use dtype::DType;
pub use error::{PortalError, PortalResult};
pub use frame_video::BYTE_STREAM_CHUNK_SIZE;
pub use metrics::{
    BufferMetrics, PolicyMetrics, PortalMetrics, RttMetrics, SyncMetrics, TransportMetrics,
};
pub use portal::{ACTIVE_OPERATOR_ATTR_KEY, Portal, ROLE_ATTR_KEY, SET_ACTIVE_OPERATOR_RPC};
pub use rpc::{RpcError, RpcHandler, RpcInvocationData};
pub use types::{
    Action, ActionChunk, ChunkColumn, FrameSource, Observation, Role, StallBehavior, StallConfig,
    State, SyncConfig, TypedValue, VideoFrameData,
};
