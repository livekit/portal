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

//! Synthesized stand-in frames for `StallBehavior::Omit`.
//!
//! When a track goes silent past its lag budget, `Omit` keeps the moment
//! alive by substituting a frame that is obviously not camera output: magenta
//! diagonals with the track's name written across it. The alternative —
//! plain black — is indistinguishable from a dark room or a capped lens, so a
//! recording made through a camera failure could never be told apart from one
//! made in the dark.
//!
//! The pixels are only half the signal. Every synthesized frame is also
//! tagged [`FrameSource::Omitted`](crate::FrameSource::Omitted), which is
//! what a policy or a dataset writer should branch on; the pattern is for
//! whoever is watching the screen.
//!
//! Rendering is deliberately dependency-free — a 5x7 bitmap font rather than
//! a font rasterizer crate, which would be a real dependency taken on for a
//! failure path. Callers are expected to cache the result per track and
//! geometry (see `SyncBuffer::placeholder_pixels`); nothing here memoizes.

use bytes::Bytes;

/// Stripe colors. Magenta essentially never occurs in a robot scene, and is
/// the established "invalid" convention in video and graphics tooling.
const MAGENTA: [u8; 3] = [0xC0, 0x00, 0xC0];
const DARK: [u8; 3] = [0x20, 0x00, 0x28];
/// Backing band behind the text, so the label stays legible over stripes.
const BAND: [u8; 3] = [0x0A, 0x00, 0x0C];
const TEXT: [u8; 3] = [0xFF, 0xFF, 0xFF];

/// Diagonal stripe half-period, in unscaled pixels.
const STRIPE: usize = 8;

const GLYPH_W: usize = 5;
const GLYPH_H: usize = 7;
/// One blank column between glyphs.
const ADVANCE: usize = GLYPH_W + 1;

/// Rows of a 5x7 glyph, most significant of the low 5 bits leftmost.
type Glyph = [u8; GLYPH_H];

const UNKNOWN: Glyph = [0x0E, 0x11, 0x01, 0x02, 0x04, 0x00, 0x04];

/// 5x7 bitmap font, uppercase-only. Track names are upcased before drawing,
/// so lowercase never needs its own glyphs.
fn glyph(c: char) -> Glyph {
    match c {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        ' ' => [0x00; GLYPH_H],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        ':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        '/' => [0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10],
        _ => UNKNOWN,
    }
}

/// Longest track name drawn. Longer names are truncated with an ellipsis so
/// one pathological name cannot push the label off both edges.
const MAX_NAME: usize = 20;

/// Render an RGB24 placeholder of `width` x `height` for `track_name`.
///
/// Always returns exactly `width * height * 3` bytes, including for degenerate
/// sizes: text that does not fit is skipped rather than clipped mid-glyph, so
/// a tiny frame comes back as bare stripes.
#[doc(hidden)]
pub fn render(width: u32, height: u32, track_name: &str) -> Bytes {
    let w = width as usize;
    let h = height as usize;
    let mut px: Vec<[u8; 3]> = (0..w * h)
        .map(|i| if ((i % w + i / w) / STRIPE).is_multiple_of(2) { MAGENTA } else { DARK })
        .collect();

    // Integer scale keeps glyph edges crisp. ~120px of width per 20 columns
    // of text is the smallest that stays readable on a teleop grid tile.
    let scale = (w / 120).max(1);
    let mut name: String = track_name.to_uppercase();
    if name.chars().count() > MAX_NAME {
        name = name.chars().take(MAX_NAME - 1).collect::<String>() + ".";
    }
    let lines = ["NO SIGNAL", name.as_str()];

    let line_h = GLYPH_H * scale;
    let gap = scale * 3;
    let text_h = line_h * lines.len() + gap;
    let pad = scale * 4;
    if text_h + pad * 2 > h {
        // No room for a legible label; the stripes alone still read as
        // "synthetic", which is the property that matters most.
        return Bytes::from(px.into_flattened());
    }

    let band_top = (h - (text_h + pad * 2)) / 2;
    let band_bot = band_top + text_h + pad * 2;
    px[band_top * w..band_bot * w].fill(BAND);

    // `scale`x nearest-neighbour blit. A line that fits is centred, so every
    // glyph pixel lands inside the band.
    for (k, line) in lines.into_iter().enumerate() {
        let text_w = line.chars().count() * ADVANCE * scale;
        if text_w > w {
            continue;
        }
        let oy = band_top + pad + k * (line_h + gap);
        let ox = (w - text_w) / 2;
        for (n, ch) in line.chars().enumerate() {
            for (row, bits) in glyph(ch).iter().enumerate() {
                // Bit 4 is the leftmost column of the glyph.
                for col in (0..GLYPH_W).filter(|col| bits & (1 << (GLYPH_W - 1 - col)) != 0) {
                    let x = ox + n * ADVANCE * scale + col * scale;
                    for y in oy + row * scale..oy + (row + 1) * scale {
                        px[y * w + x..][..scale].fill(TEXT);
                    }
                }
            }
        }
    }

    Bytes::from(px.into_flattened())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(buf: &[u8], w: usize, x: usize, y: usize) -> [u8; 3] {
        let i = (y * w + x) * 3;
        [buf[i], buf[i + 1], buf[i + 2]]
    }

    /// The buffer is always exactly RGB24-sized for the requested geometry —
    /// consumers reshape it as `(h, w, 3)` and a short buffer would be a
    /// downstream panic rather than a visible defect.
    #[test]
    fn output_is_always_rgb24_sized() {
        for (w, h) in [(640u32, 480u32), (1, 1), (2, 2), (17, 33), (320, 240)] {
            let b = render(w, h, "cam1");
            assert_eq!(b.len(), (w as usize) * (h as usize) * 3, "{w}x{h}");
        }
    }

    /// Zero-sized geometry is degenerate but must not panic.
    #[test]
    fn zero_geometry_is_empty_not_a_panic() {
        assert!(render(0, 0, "cam1").is_empty());
        assert!(render(0, 10, "cam1").is_empty());
    }

    /// A placeholder must never be mistakable for a dark scene: the stripes
    /// guarantee saturated magenta somewhere in the frame.
    #[test]
    fn is_visibly_not_a_dark_frame() {
        let w = 320usize;
        let b = render(320, 240, "cam1");
        let has_magenta = (0..240).any(|y| (0..w).any(|x| px(&b, w, x, y) == MAGENTA));
        assert!(has_magenta, "placeholder must be obviously synthetic");
        assert!(b.iter().any(|&v| v > 0x80), "must not read as near-black");
    }

    /// Text is drawn for a normal-sized frame — the band and white pixels are
    /// what tell an operator *which* camera died.
    #[test]
    fn draws_a_label_when_there_is_room() {
        let w = 640usize;
        let b = render(640, 480, "wrist");
        let white = (0..480).any(|y| (0..w).any(|x| px(&b, w, x, y) == TEXT));
        assert!(white, "expected label pixels");
    }

    /// Frames too small for a legible label degrade to bare stripes rather
    /// than clipping glyphs into noise.
    #[test]
    fn tiny_frames_skip_the_label() {
        let w = 2usize;
        let b = render(2, 2, "cam1");
        let white = (0..2).any(|y| (0..w).any(|x| px(&b, w, x, y) == TEXT));
        assert!(!white, "no label should be attempted at 2x2");
    }

    /// Rendering is deterministic, which is what makes caching per (track,
    /// geometry) safe.
    #[test]
    fn render_is_deterministic() {
        assert_eq!(render(64, 64, "cam1"), render(64, 64, "cam1"));
        assert_ne!(render(64, 64, "cam1"), render(64, 64, "cam2"));
    }

    /// A pathological track name must not panic or overrun the buffer.
    #[test]
    fn long_and_odd_names_are_contained() {
        let b = render(320, 240, "a-very-long-track-name-that-keeps-going/…");
        assert_eq!(b.len(), 320 * 240 * 3);
    }
}
