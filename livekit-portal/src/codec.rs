//! Frame-video codecs: encode RGB24 to a payload byte string for byte-stream
//! transport, and decode a payload back to RGB24.
//!
//! The user-facing API takes and returns RGB regardless of codec or transport.
//! `Raw` is byte-for-byte RGB24, `Png` is RFC 2083, `Mjpeg` is one JPEG per
//! frame. PNG and JPEG carry their own dimensions so decode is self-describing;
//! `Raw` requires the caller to provide dimensions out-of-band.
//!
//! Quality is honored for `Mjpeg` (1..=100) and ignored for `Raw` and `Png`.

use std::io::Cursor;

use bytes::Bytes;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType as PngFilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder};

/// Codec used by a video track.
///
/// Selected per-track at config time via `PortalConfig::add_video`. The codec
/// also picks the wire transport: the WebRTC codecs (`H264` / `Vp8` / `Vp9` /
/// `Av1`) ride the WebRTC media path and are encoded by libwebrtc, every other
/// variant rides a reliable byte-stream channel and is encoded by this module.
/// The user-facing payload is RGB in every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// WebRTC H.264, lossy. Real-time RTP/SRTP transport with libwebrtc's
    /// adaptive bitrate. Best-effort delivery — frames may drop or arrive
    /// late. Lowest end-to-end latency at scale. Encoded by libwebrtc,
    /// not this module — the byte-stream encode/decode helpers below panic
    /// for the WebRTC codecs.
    H264,
    /// WebRTC VP8, lossy. Same media path and trade-offs as `H264`. Widely
    /// supported software codec.
    Vp8,
    /// WebRTC VP9, lossy. Same media path as `H264`. Better compression than
    /// VP8/H264 at equal quality, higher CPU cost.
    Vp9,
    /// WebRTC AV1, lossy. Same media path as `H264`. Best compression of the
    /// set, highest CPU cost. Newest codec — confirm both peers support it.
    Av1,
    /// WebRTC H.265 / HEVC, lossy. Same media path as `H264`. Support is
    /// platform- and build-dependent in libwebrtc — confirm both peers
    /// negotiate it before relying on it.
    H265,
    /// Uncompressed RGB24. Largest payload, zero encode cost. Use when CPU is
    /// scarce or you want bit-exact frames with no codec dependency.
    Raw,
    /// PNG, lossless. ~2-3x compression on natural images. ~10-30 ms encode at
    /// 480p.
    Png,
    /// Motion JPEG, lossy. ~10-20x compression at quality 90. Sub-millisecond
    /// decode. Each frame is an independent JPEG (no temporal coding), so
    /// frame loss is contained.
    Mjpeg,
}

impl Codec {
    /// Whether this codec rides the WebRTC media path. The remaining codecs
    /// ride the per-frame byte-stream path.
    pub fn is_webrtc(self) -> bool {
        matches!(self, Codec::H264 | Codec::Vp8 | Codec::Vp9 | Codec::Av1 | Codec::H265)
    }
}

/// Decoded frame: RGB24 bytes plus the dimensions parsed from the payload
/// (or echoed back, in `Raw`'s case). `rgb` is `Bytes` so the `Raw` decode
/// can hand back a zero-copy slice of the wire payload — refcount bump
/// only, no memcpy. PNG and MJPEG own their decoded buffer.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub rgb: Bytes,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("invalid frame dimensions: {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error(
        "wrong RGB buffer size for {width}x{height}: expected {expected} bytes, got {got}"
    )]
    WrongRgbSize { width: u32, height: u32, expected: usize, got: usize },
    #[error("encode failed: {0}")]
    EncodeFailed(String),
    #[error("decode failed: {0}")]
    DecodeFailed(String),
    #[error(
        "decoded dimensions {decoded_width}x{decoded_height} disagree with declared {declared_width}x{declared_height}"
    )]
    DimensionMismatch {
        decoded_width: u32,
        decoded_height: u32,
        declared_width: u32,
        declared_height: u32,
    },
}

pub type CodecResult<T> = Result<T, CodecError>;

/// Validate that `rgb` matches `width × height × 3` and dimensions are
/// non-zero / non-overflowing.
fn check_rgb(rgb: &[u8], width: u32, height: u32) -> CodecResult<()> {
    if width == 0 || height == 0 {
        return Err(CodecError::InvalidDimensions { width, height });
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(3))
        .ok_or(CodecError::InvalidDimensions { width, height })?;
    if rgb.len() != expected {
        return Err(CodecError::WrongRgbSize {
            width,
            height,
            expected,
            got: rgb.len(),
        });
    }
    Ok(())
}

/// Rough encoded-size estimate for capacity hints. Picked to over-allocate
/// slightly more often than under-allocate, so the buffer doesn't grow
/// during encode. Only used as a `Vec::with_capacity` hint — the actual
/// payload size is whatever the encoder produces.
///
///   * Raw   = exact (`W*H*3`)
///   * Png   = same as raw (high-entropy frames sit near the raw size)
///   * Mjpeg = raw / 8 (≈ q90 ratio on natural images; loose, but avoids
///     the common case of `Vec` doubling-from-zero during encode)
pub fn estimated_encoded_size(width: u32, height: u32, codec: Codec) -> usize {
    let raw = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(3);
    match codec {
        Codec::Raw => raw,
        Codec::Png => raw,
        Codec::Mjpeg => (raw / 8).max(1024),
        Codec::H264 | Codec::Vp8 | Codec::Vp9 | Codec::Av1 | Codec::H265 => unreachable!(
            "WebRTC codecs ride the WebRTC media path, not the byte-stream encode path"
        ),
    }
}

/// Encode an RGB24 frame to the wire payload for `codec`. `quality` is in
/// `1..=100` for `Mjpeg` and is ignored for `Raw` and `Png`.
///
/// Quality range is enforced at config time by `PortalConfig::add_video`,
/// so this hot-path encode skips re-validation.
///
/// Allocates a fresh `Vec`. Prefer `encode_frame_into` when you already
/// own the destination buffer (publishers do, to bake the framing header
/// and the codec output into a single allocation).
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

/// Encode an RGB24 frame into `out`, appending to whatever's already
/// there. Used by the frame-video publisher to fold the wire-framing
/// header and the codec payload into one buffer — saves a Vec
/// allocation and a full payload memcpy versus encoding into a
/// temporary and copying.
pub fn encode_frame_into(
    out: &mut Vec<u8>,
    rgb: &[u8],
    width: u32,
    height: u32,
    codec: Codec,
    quality: u8,
) -> CodecResult<()> {
    check_rgb(rgb, width, height)?;
    match codec {
        Codec::Raw => {
            out.extend_from_slice(rgb);
            Ok(())
        }
        Codec::Png => {
            // `Fast` instead of `Default` — for streaming inference video,
            // encode latency matters more than the last 10% of compression.
            // Bitstream-level losslessness is unaffected. Bumping the
            // compression dial would add ~30% encode time per 480p frame
            // for ~10% smaller payloads — wrong trade for a 5-30 Hz
            // realtime path. `Adaptive` filter still picks the best filter
            // per scanline.
            let encoder = PngEncoder::new_with_quality(
                &mut *out,
                CompressionType::Fast,
                PngFilterType::Adaptive,
            );
            encoder
                .write_image(rgb, width, height, ExtendedColorType::Rgb8)
                .map_err(|e| CodecError::EncodeFailed(e.to_string()))?;
            Ok(())
        }
        Codec::Mjpeg => {
            let mut encoder = JpegEncoder::new_with_quality(&mut *out, quality);
            encoder
                .encode(rgb, width, height, ExtendedColorType::Rgb8)
                .map_err(|e| CodecError::EncodeFailed(e.to_string()))?;
            Ok(())
        }
        Codec::H264 | Codec::Vp8 | Codec::Vp9 | Codec::Av1 | Codec::H265 => unreachable!(
            "WebRTC codecs ride the WebRTC media path, not the byte-stream encode path"
        ),
    }
}

/// Decode a wire payload to RGB24 plus its dimensions. For `Raw`, dimensions
/// must be supplied by the caller via `declared_width` / `declared_height`
/// (the byte stream carries them in the framing header) — the `Bytes` is
/// returned untouched (zero-copy refcount bump). For `Png` / `Mjpeg`, the
/// encoded bitstream carries its own dimensions; the declared values, when
/// non-zero, are checked against the decoded values and a mismatch returns
/// `DimensionMismatch`.
pub fn decode_frame(
    bytes: Bytes,
    codec: Codec,
    declared_width: u32,
    declared_height: u32,
) -> CodecResult<DecodedFrame> {
    match codec {
        Codec::Raw => {
            check_rgb(&bytes, declared_width, declared_height)?;
            Ok(DecodedFrame {
                rgb: bytes,
                width: declared_width,
                height: declared_height,
            })
        }
        Codec::Png => decode_with_image_crate(
            &bytes,
            image::ImageFormat::Png,
            declared_width,
            declared_height,
        ),
        Codec::Mjpeg => decode_with_image_crate(
            &bytes,
            image::ImageFormat::Jpeg,
            declared_width,
            declared_height,
        ),
        Codec::H264 | Codec::Vp8 | Codec::Vp9 | Codec::Av1 | Codec::H265 => unreachable!(
            "WebRTC codecs ride the WebRTC media path, not the byte-stream decode path"
        ),
    }
}

fn decode_with_image_crate(
    bytes: &[u8],
    format: image::ImageFormat,
    declared_width: u32,
    declared_height: u32,
) -> CodecResult<DecodedFrame> {
    let cursor = Cursor::new(bytes);
    let reader = image::ImageReader::with_format(cursor, format);
    let decoded = reader
        .decode()
        .map_err(|e| CodecError::DecodeFailed(e.to_string()))?
        .into_rgb8();
    let (w, h) = decoded.dimensions();
    if (declared_width != 0 || declared_height != 0)
        && (w != declared_width || h != declared_height)
    {
        return Err(CodecError::DimensionMismatch {
            decoded_width: w,
            decoded_height: h,
            declared_width,
            declared_height,
        });
    }
    Ok(DecodedFrame { rgb: Bytes::from(decoded.into_raw()), width: w, height: h })
}

#[cfg(test)]
mod tests {
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
    fn raw_roundtrip_is_byte_exact() {
        let rgb = gradient(64, 48);
        let bytes = encode_frame(&rgb, 64, 48, Codec::Raw, 0).unwrap();
        assert_eq!(bytes, rgb, "raw encode is identity");
        let decoded = decode_frame(Bytes::from(bytes), Codec::Raw, 64, 48).unwrap();
        assert_eq!(decoded.rgb.as_ref(), rgb.as_slice());
        assert_eq!((decoded.width, decoded.height), (64, 48));
    }

    #[test]
    fn raw_decode_is_zero_copy() {
        // Raw decode must hand back a `Bytes` that aliases the input — no
        // memcpy of the payload. We can't directly observe the heap
        // pointer, but `Bytes::ptr_eq` (via the addr of the slice) tells
        // us whether two views share the same allocation.
        let rgb = gradient(64, 48);
        let payload = Bytes::from(rgb.clone());
        let payload_ptr = payload.as_ptr();
        let decoded = decode_frame(payload, Codec::Raw, 64, 48).unwrap();
        assert_eq!(
            decoded.rgb.as_ptr(),
            payload_ptr,
            "Raw decode must reuse the input buffer (zero-copy)"
        );
    }

    #[test]
    fn png_roundtrip_is_byte_exact() {
        let rgb = gradient(64, 48);
        let bytes = encode_frame(&rgb, 64, 48, Codec::Png, 0).unwrap();
        assert!(bytes.len() < rgb.len() * 2, "png shouldn't grow much over raw");
        let decoded = decode_frame(Bytes::from(bytes), Codec::Png, 64, 48).unwrap();
        assert_eq!(decoded.rgb.as_ref(), rgb.as_slice(), "PNG is lossless");
        assert_eq!((decoded.width, decoded.height), (64, 48));
    }

    #[test]
    fn mjpeg_roundtrip_is_close() {
        let rgb = gradient(64, 48);
        let bytes = encode_frame(&rgb, 64, 48, Codec::Mjpeg, 95).unwrap();
        assert!(bytes.len() < rgb.len(), "jpeg should shrink the payload");
        let decoded = decode_frame(Bytes::from(bytes), Codec::Mjpeg, 64, 48).unwrap();
        assert_eq!((decoded.width, decoded.height), (64, 48));
        // JPEG is lossy; check the average per-pixel error is small.
        let total: u64 = rgb
            .iter()
            .zip(decoded.rgb.iter())
            .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64)
            .sum();
        let avg = total as f64 / rgb.len() as f64;
        assert!(avg < 5.0, "avg pixel error {avg} should be small at q=95");
    }

    #[test]
    fn declared_dims_zero_skips_check() {
        // PNG / MJPEG payloads carry their own dimensions; passing 0/0 means
        // "trust the bitstream" which is what receivers do when they have no
        // out-of-band hint.
        let rgb = gradient(32, 32);
        let bytes = encode_frame(&rgb, 32, 32, Codec::Png, 0).unwrap();
        let decoded = decode_frame(Bytes::from(bytes), Codec::Png, 0, 0).unwrap();
        assert_eq!((decoded.width, decoded.height), (32, 32));
    }

    #[test]
    fn dimension_mismatch_rejected() {
        let rgb = gradient(32, 32);
        let bytes = encode_frame(&rgb, 32, 32, Codec::Png, 0).unwrap();
        let err = decode_frame(Bytes::from(bytes), Codec::Png, 64, 64).unwrap_err();
        assert!(matches!(err, CodecError::DimensionMismatch { .. }));
    }

    #[test]
    fn wrong_rgb_size_rejected() {
        let rgb = vec![0u8; 100]; // not 32*32*3
        let err = encode_frame(&rgb, 32, 32, Codec::Raw, 0).unwrap_err();
        assert!(matches!(err, CodecError::WrongRgbSize { .. }));
    }
}
