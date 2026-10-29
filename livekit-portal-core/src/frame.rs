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

//! The video frame payload the core buffers and matches. The core reads
//! only timestamps, so a driver can hand it decoded pixels (native) or an
//! opaque handle (a browser `VideoFrame`).

use crate::placeholder;
use crate::types::{FrameSource, VideoFrameData};

pub trait FramePayload: Clone {
    /// Sender timestamp, from the track's frame metadata.
    fn timestamp_us(&self) -> u64;

    /// Tag how the frame is used in an observation.
    fn set_source(&mut self, source: FrameSource);

    /// A stand-in for a silent track under `StallBehavior::Omit`, shaped
    /// like `self` (the track's last real frame) and carrying its
    /// timestamp, tagged `FrameSource::Omitted`. `cached` is the previous
    /// placeholder for the same track, to reuse when it still fits.
    fn placeholder(&self, track_name: &str, cached: Option<&Self>) -> Self;
}

impl FramePayload for VideoFrameData {
    fn timestamp_us(&self) -> u64 {
        self.timestamp_us
    }

    fn set_source(&mut self, source: FrameSource) {
        self.source = source;
    }

    fn placeholder(&self, track_name: &str, cached: Option<&Self>) -> Self {
        let data = match cached {
            Some(c) if c.width == self.width && c.height == self.height => c.data.clone(),
            _ => placeholder::render(self.width, self.height, track_name),
        };
        VideoFrameData {
            width: self.width,
            height: self.height,
            data,
            timestamp_us: self.timestamp_us,
            source: FrameSource::Omitted,
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn frame(width: u32, height: u32, ts: u64) -> VideoFrameData {
        VideoFrameData {
            width,
            height,
            data: Bytes::from(vec![0u8; (width * height * 3) as usize]),
            timestamp_us: ts,
            source: FrameSource::Live,
        }
    }

    #[test]
    fn placeholder_keeps_geometry_and_timestamp() {
        let p = frame(8, 6, 42).placeholder("cam", None);
        assert_eq!((p.width, p.height, p.timestamp_us), (8, 6, 42));
        assert_eq!(p.source, FrameSource::Omitted);
        assert_eq!(p.data, placeholder::render(8, 6, "cam"));
    }

    #[test]
    fn placeholder_reuses_pixels_only_at_the_same_size() {
        let first = frame(8, 6, 1).placeholder("cam", None);
        let again = frame(8, 6, 2).placeholder("cam", Some(&first));
        assert_eq!(again.data.as_ptr(), first.data.as_ptr());
        assert_eq!(again.timestamp_us, 2);
        let resized = frame(4, 4, 3).placeholder("cam", Some(&first));
        assert_eq!(resized.data.len(), 4 * 4 * 3);
    }
}
