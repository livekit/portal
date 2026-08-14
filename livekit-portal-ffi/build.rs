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

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    configure_linker();
}

// WebRTC from the LiveKit rust-sdks links in Objective-C categories for the
// macOS/iOS video codec factories. Without `-ObjC`, Apple's linker strips
// category symbols as dead code and calls like
// `+[NSString stringForAbslStringView:]` throw
// `unrecognized selector` at runtime when the PeerConnection factory starts.
// On Linux, webrtc ships as a static archive and must be force-linked.
fn configure_linker() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "macos" | "ios" => {
            println!("cargo:rustc-link-arg=-ObjC");
        }
        "linux" => {
            println!("cargo:rustc-link-lib=static=webrtc");
        }
        _ => {}
    }
}
