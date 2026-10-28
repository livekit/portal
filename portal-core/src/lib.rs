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
pub mod dtype;
pub mod error;
#[doc(hidden)]
pub mod keypoint;
pub mod metrics;
#[doc(hidden)]
pub mod placeholder;
pub mod rpc;
#[doc(hidden)]
pub mod serialization;
#[doc(hidden)]
pub mod sync_buffer;
pub mod types;

pub use codec::Codec;
pub use config::{
    DEFAULT_H264_MAX_BITRATE_KBPS, FieldSpec, PortalConfig, Resolution, VideoOptions,
    VideoTrackSpec,
};
pub use config_file::ConfigFileError;
pub use dtype::DType;
pub use error::{PortalError, PortalResult};
pub use keypoint::Keypoint;
pub use metrics::{
    BufferMetrics, PolicyMetrics, PortalMetrics, RttMetrics, SyncMetrics, TimeSyncMetrics,
    TransportMetrics,
};
pub use rpc::{RpcError, RpcHandler, RpcInvocationData};
pub use types::{
    Action, ActionSubscription, FrameSource, Observation, Role, StallBehavior, StallConfig, State,
    SyncConfig, TimeSyncSource, TypedValue, VideoFrameData,
};
