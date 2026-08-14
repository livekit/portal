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

//! RPC types and handler alias.
//!
//! These re-expose the subset of the LiveKit RPC surface that Portal callers
//! need, without forcing the `livekit::` prelude into their imports. Handlers
//! operate on `RpcInvocationData` and return `Result<String, RpcError>`;
//! Portal translates these to/from the SDK's own types at the room boundary.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Data passed to a method handler for an incoming RPC invocation.
#[derive(Debug, Clone)]
pub struct RpcInvocationData {
    /// Matches on both sides of the call. Useful for logs.
    pub request_id: String,
    /// Identity of the caller.
    pub caller_identity: String,
    /// User-defined payload. Typically JSON; opaque to Portal.
    pub payload: String,
    /// Upper bound on how long the caller will wait for a reply.
    pub response_timeout: Duration,
}

/// Error raised by an RPC handler or returned from `perform_rpc`.
///
/// Codes 1001–1999 are reserved for built-in transport errors (see the
/// `livekit` crate for the canonical list). Application code should pick
/// codes outside that range.
#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: u32,
    pub message: String,
    pub data: Option<String>,
}

impl RpcError {
    pub fn new(code: u32, message: impl Into<String>, data: Option<String>) -> Self {
        Self { code, message: message.into(), data }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rpc error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// Boxed future returned by an RPC handler.
pub type RpcHandlerFuture =
    Pin<Box<dyn Future<Output = Result<String, RpcError>> + Send + 'static>>;

/// RPC handler trait object stored on Portal. Cloned into the closure handed
/// to `LocalParticipant::register_rpc_method` at connect time.
pub type RpcHandler = Arc<dyn Fn(RpcInvocationData) -> RpcHandlerFuture + Send + Sync + 'static>;

// --- Conversions to/from the SDK's types, crate-local ---

impl From<livekit::prelude::RpcError> for RpcError {
    fn from(e: livekit::prelude::RpcError) -> Self {
        Self { code: e.code, message: e.message, data: e.data }
    }
}

impl From<RpcError> for livekit::prelude::RpcError {
    fn from(e: RpcError) -> Self {
        livekit::prelude::RpcError::new(e.code, e.message, e.data)
    }
}

impl From<livekit::prelude::RpcInvocationData> for RpcInvocationData {
    fn from(d: livekit::prelude::RpcInvocationData) -> Self {
        Self {
            request_id: d.request_id,
            caller_identity: d.caller_identity.as_str().to_string(),
            payload: d.payload,
            response_timeout: d.response_timeout,
        }
    }
}
