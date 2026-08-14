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

use crate::dtype::DType;
use crate::rpc::RpcError;
use crate::types::Role;
use thiserror::Error;

pub type PortalResult<T> = Result<T, PortalError>;

#[derive(Error, Debug)]
pub enum PortalError {
    #[error("room error: {0}")]
    Room(String),

    #[error("portal is already connected")]
    AlreadyConnected,

    #[error("portal is not connected")]
    NotConnected,

    #[error("no peer in the room")]
    NoPeer,

    #[error(
        "room has multiple remote participants and no peer has been identified yet; pass destination explicitly"
    )]
    AmbiguousPeer,

    #[error("unknown video track: {name}")]
    UnknownVideoTrack { name: String },

    #[error("unknown action chunk: {name}")]
    UnknownChunk { name: String },

    #[error("wrong frame size: expected {expected} bytes, got {got}")]
    WrongFrameSize { expected: usize, got: usize },

    #[error("invalid frame dimensions: width={width}, height={height} (must both be even)")]
    InvalidFrameDimensions { width: u32, height: u32 },

    #[error("deserialization error: {0}")]
    Deserialization(String),

    #[error("frame codec error: {0}")]
    Codec(String),

    #[error("operation not available for role {0:?}")]
    WrongRole(Role),

    #[error(
        "field '{field}' declared as {expected:?} but sent as {got}; use the matching TypedValue variant or redeclare the dtype"
    )]
    DtypeMismatch { field: String, expected: DType, got: &'static str },

    #[error("{0}")]
    Rpc(RpcError),
}
