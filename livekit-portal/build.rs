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

fn main() {
    // The same libwebrtc build webrtc-sys links: one webrtc-sys-build version
    // in the graph means one prebuilt, so these headers match the symbols.
    let webrtc_dir = webrtc_sys_build::webrtc_dir();
    if !webrtc_dir.exists() {
        webrtc_sys_build::download_webrtc().expect("libwebrtc download failed");
    }
    let jpeg_include = webrtc_dir.join("include/third_party/libjpeg_turbo/src");
    println!("cargo:rerun-if-changed=csrc/jpeg_encode.c");
    cc::Build::new()
        .file("csrc/jpeg_encode.c")
        .include(&jpeg_include)
        .define("MANGLE_JPEG_NAMES", None)
        .warnings(true)
        .compile("portal_jpeg");
}
