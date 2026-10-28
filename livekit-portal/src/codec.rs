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

//! Frame-video codecs. Same as `portal_core::codec`, except MJPEG encodes
//! through the libjpeg-turbo inside libwebrtc (see `jpeg.rs`) rather than the
//! pure-Rust encoder the core uses.

pub use portal_core::codec::{
    Codec, CodecError, CodecResult, DecodedFrame, decode_frame, estimated_encoded_size,
};

/// See `portal_core::codec::encode_frame`.
pub fn encode_frame(
    rgb: &[u8],
    width: u32,
    height: u32,
    codec: Codec,
    quality: u8,
) -> CodecResult<Vec<u8>> {
    let mut out = Vec::with_capacity(estimated_encoded_size(width, height, codec));
    encode_frame_into(&mut out, rgb, width, height, codec, quality)?;
    Ok(out)
}

/// See `portal_core::codec::encode_frame_into`.
pub fn encode_frame_into(
    out: &mut Vec<u8>,
    rgb: &[u8],
    width: u32,
    height: u32,
    codec: Codec,
    quality: u8,
) -> CodecResult<()> {
    if codec != Codec::Mjpeg {
        return portal_core::codec::encode_frame_into(out, rgb, width, height, codec, quality);
    }
    portal_core::codec::check_rgb(rgb, width, height)?;
    crate::jpeg::encode_rgb_into(out, rgb, width, height, quality)
        .ok_or_else(|| CodecError::EncodeFailed("libjpeg rejected the frame".into()))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                out.push((x % 256) as u8);
                out.push((y % 256) as u8);
                out.push(((x + y) % 256) as u8);
            }
        }
        out
    }

    #[test]
    fn mjpeg_roundtrip_is_close() {
        let rgb = gradient(64, 48);
        let bytes = encode_frame(&rgb, 64, 48, Codec::Mjpeg, 95).unwrap();
        assert!(bytes.len() < rgb.len(), "jpeg should shrink the payload");
        let decoded = decode_frame(Bytes::from(bytes), Codec::Mjpeg, 64, 48).unwrap();
        assert_eq!((decoded.width, decoded.height), (64, 48));
        let total: u64 = rgb
            .iter()
            .zip(decoded.rgb.iter())
            .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64)
            .sum();
        let avg = total as f64 / rgb.len() as f64;
        assert!(avg < 5.0, "avg pixel error {avg} should be small at q=95");
    }

    /// Sampling factors of each component in the baseline frame header.
    fn jpeg_sampling(jpeg: &[u8]) -> Vec<u8> {
        let sof = jpeg.windows(2).position(|w| w == [0xFF, 0xC0]).expect("baseline SOF0 marker");
        let count = jpeg[sof + 9] as usize;
        (0..count).map(|i| jpeg[sof + 10 + i * 3 + 1]).collect()
    }

    #[test]
    fn mjpeg_keeps_full_chroma_resolution() {
        let rgb = gradient(64, 48);
        let bytes = encode_frame(&rgb, 64, 48, Codec::Mjpeg, 90).unwrap();
        assert_eq!(jpeg_sampling(&bytes), vec![0x11, 0x11, 0x11], "4:4:4, no chroma subsampling");
    }

    #[test]
    fn mjpeg_quality_trades_size() {
        let rgb = gradient(64, 48);
        let low = encode_frame(&rgb, 64, 48, Codec::Mjpeg, 30).unwrap();
        let high = encode_frame(&rgb, 64, 48, Codec::Mjpeg, 95).unwrap();
        assert!(
            low.len() < high.len(),
            "q=30 ({}) should be smaller than q=95 ({})",
            low.len(),
            high.len()
        );
    }

    #[test]
    fn mjpeg_roundtrip_keeps_odd_dimensions() {
        let rgb = gradient(33, 17);
        let bytes = encode_frame(&rgb, 33, 17, Codec::Mjpeg, 90).unwrap();
        let decoded = decode_frame(Bytes::from(bytes), Codec::Mjpeg, 33, 17).unwrap();
        assert_eq!((decoded.width, decoded.height), (33, 17));
    }

    #[test]
    fn mjpeg_rejects_wrong_rgb_size() {
        let err = encode_frame(&[0u8; 100], 32, 32, Codec::Mjpeg, 90).unwrap_err();
        assert!(matches!(err, CodecError::WrongRgbSize { .. }));
    }

    #[test]
    fn other_codecs_match_the_core() {
        let rgb = gradient(16, 8);
        for codec in [Codec::Raw, Codec::Png] {
            assert_eq!(
                encode_frame(&rgb, 16, 8, codec, 0).unwrap(),
                portal_core::codec::encode_frame(&rgb, 16, 8, codec, 0).unwrap()
            );
        }
    }
}
