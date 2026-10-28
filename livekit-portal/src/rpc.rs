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

//! RPC types (see `portal_core::rpc`) plus their conversions to and from the
//! LiveKit SDK's own types, used at the room boundary.

pub use portal_core::rpc::*;

pub(crate) fn rpc_error_from_sdk(e: livekit::prelude::RpcError) -> RpcError {
    RpcError { code: e.code, message: e.message, data: e.data }
}

pub(crate) fn rpc_error_to_sdk(e: RpcError) -> livekit::prelude::RpcError {
    livekit::prelude::RpcError::new(e.code, e.message, e.data)
}

pub(crate) fn invocation_from_sdk(d: livekit::prelude::RpcInvocationData) -> RpcInvocationData {
    RpcInvocationData {
        request_id: d.request_id,
        caller_identity: d.caller_identity.as_str().to_string(),
        payload: d.payload,
        response_timeout: d.response_timeout,
    }
}
