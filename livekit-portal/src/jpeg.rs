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

//! MJPEG encode through the libjpeg-turbo inside libwebrtc. The C side lives
//! in `csrc/jpeg_encode.c`.

use std::ffi::{c_int, c_uchar, c_uint, c_ulong};

unsafe extern "C" {
    fn portal_jpeg_encode_rgb(
        rgb: *const c_uchar,
        width: c_uint,
        height: c_uint,
        quality: c_int,
        out: *mut *mut c_uchar,
        out_len: *mut c_ulong,
    ) -> c_int;
    fn portal_jpeg_free(buf: *mut c_uchar);
}

/// Append the 4:4:4 JPEG encoding of packed RGB24 `rgb` to `out`. `None` if
/// `rgb` isn't `width * height * 3` bytes or libjpeg rejects the frame.
pub(crate) fn encode_rgb_into(
    out: &mut Vec<u8>,
    rgb: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Option<()> {
    if rgb.len() != (width as usize) * (height as usize) * 3 {
        return None;
    }
    let mut buf: *mut c_uchar = std::ptr::null_mut();
    let mut len: c_ulong = 0;
    // SAFETY: `rgb` holds exactly the `width * height * 3` bytes the shim
    // reads, and `buf` / `len` are valid out-pointers for the call.
    let status = unsafe {
        portal_jpeg_encode_rgb(rgb.as_ptr(), width, height, quality.into(), &mut buf, &mut len)
    };
    if status != 0 || buf.is_null() {
        return None;
    }
    // SAFETY: on success the shim hands over a buffer of `len` initialized
    // bytes that nothing else references until it is freed below.
    out.extend_from_slice(unsafe { std::slice::from_raw_parts(buf, len as usize) });
    // SAFETY: `buf` came from the shim, is freed exactly once, and the slice
    // borrowed from it ended with the copy above.
    unsafe { portal_jpeg_free(buf) };
    Some(())
}
