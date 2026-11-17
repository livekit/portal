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

//! `livekit-portal-core` for the browser. The TypeScript driver in `js/src`
//! owns the LiveKit room; this crate owns every decision, exactly as the
//! native driver does. Inputs come in as plain JS values with the local time,
//! and the core's outputs go back as plain JS objects (see `js.rs`), already
//! mapped to LiveKit names by `livekit_portal_core::livekit`.
//!
//! Times cross the boundary as `number` microseconds: Unix-epoch µs stay
//! below 2^53 for the next two centuries, so no precision is lost.

mod config;
mod core;
mod frame;
mod js;
mod logger;

pub use config::Config;
pub use core::Core;
