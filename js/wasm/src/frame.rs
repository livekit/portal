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

use livekit_portal_core::FramePayload;
use livekit_portal_core::placeholder;
use livekit_portal_core::types::{FrameSource, VideoFrameData};

/// A frame the core buffers and matches. WebRTC frames stay in the browser
/// as `VideoFrame`s and the core only holds their handle; byte frames
/// (RAW/PNG/MJPEG) are decoded here, so the core holds their pixels.
#[derive(Clone)]
pub enum WebFrame {
    Pixels(VideoFrameData),
    Handle(FrameHandle),
}

#[derive(Clone, Copy)]
pub struct FrameHandle {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub timestamp_us: u64,
    pub source: FrameSource,
}

impl From<VideoFrameData> for WebFrame {
    fn from(frame: VideoFrameData) -> Self {
        WebFrame::Pixels(frame)
    }
}

impl FramePayload for WebFrame {
    fn timestamp_us(&self) -> u64 {
        match self {
            WebFrame::Pixels(f) => f.timestamp_us,
            WebFrame::Handle(h) => h.timestamp_us,
        }
    }

    fn set_source(&mut self, source: FrameSource) {
        match self {
            WebFrame::Pixels(f) => f.source = source,
            WebFrame::Handle(h) => h.source = source,
        }
    }

    /// A handle's stand-in is rendered pixels: there is no browser frame
    /// to point at.
    fn placeholder(&self, track_name: &str, cached: Option<&Self>) -> Self {
        let shape = match self {
            WebFrame::Pixels(f) => f.clone(),
            WebFrame::Handle(h) => VideoFrameData {
                width: h.width,
                height: h.height,
                data: placeholder::render(h.width, h.height, track_name),
                timestamp_us: h.timestamp_us,
                source: FrameSource::Live,
            },
        };
        let cached = match cached {
            Some(WebFrame::Pixels(c)) => Some(c),
            _ => None,
        };
        WebFrame::Pixels(shape.placeholder(track_name, cached))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_placeholder_is_rendered_at_the_handle_size() {
        let handle = WebFrame::Handle(FrameHandle {
            id: 7,
            width: 8,
            height: 6,
            timestamp_us: 42,
            source: FrameSource::Live,
        });
        let WebFrame::Pixels(p) = handle.placeholder("cam", None) else {
            panic!("expected pixels");
        };
        assert_eq!((p.width, p.height, p.timestamp_us), (8, 6, 42));
        assert_eq!(p.source, FrameSource::Omitted);
        assert_eq!(p.data.len(), 8 * 6 * 3);
    }
}
