//! Nearest-neighbour downscale of captured RGBA frames — the cheapest
//! resampler wins at streaming sizes (the codec dominates visually), but
//! it touches every output pixel every frame, so the source column for
//! each output column is computed once and the inner loops are pure
//! row-sliced copies.

/// Scale RGBA to RGBA. Never upscales in practice (`dw <= sw`, `dh <= sh`
/// from the callers' fit), but growth is handled rather than guarded.
pub fn scale_rgba(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    let xmap: Vec<usize> = (0..dw).map(|x| (x * sw / dw) * 4).collect();
    let mut out = vec![0u8; dw * dh * 4];
    for (y, drow) in out.chunks_exact_mut(dw * 4).enumerate() {
        let sy = y * sh / dh;
        let srow = &src[sy * sw * 4..][..sw * 4];
        let (pixels, _) = drow.as_chunks_mut::<4>();
        for (dst, &sx) in pixels.iter_mut().zip(&xmap) {
            dst.copy_from_slice(&srow[sx..sx + 4]);
        }
    }
    out
}

/// Scale RGBA straight to tightly packed RGB (alpha dropped) — the H.264
/// path's shape: the encoder's fast RGB→YUV conversion wants 3-byte
/// pixels, and the unchanged-frame compare gets 25% cheaper. Also serves
/// as the plain strip pass when no scaling is needed.
pub fn scale_rgba_to_rgb(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    let xmap: Vec<usize> = (0..dw).map(|x| (x * sw / dw) * 4).collect();
    let mut out = vec![0u8; dw * dh * 3];
    for (y, drow) in out.chunks_exact_mut(dw * 3).enumerate() {
        let sy = y * sh / dh;
        let srow = &src[sy * sw * 4..][..sw * 4];
        let (pixels, _) = drow.as_chunks_mut::<3>();
        for (dst, &sx) in pixels.iter_mut().zip(&xmap) {
            dst.copy_from_slice(&srow[sx..sx + 3]);
        }
    }
    out
}

/// Scale RGBA straight to a tightly packed **I420** (YUV 4:2:0) buffer in a
/// single pass — the H.264 path's real shape. Replaces the old two-step
/// `scale_rgba_to_rgb` → openh264 RGB→YUV, which materialised a full RGB
/// intermediate and then walked it again: this fuses the downscale and the
/// colour conversion so every output pixel is touched once. The layout is
/// the contiguous I420 openh264's `YUVBuffer::from_vec` expects — Y plane
/// (`dw*dh`), then U, then V (each `dw/2 * dh/2`) — so the caller can feed
/// it to the encoder with a borrowing `YUVSource` and never copy.
///
/// Both output edges must be even (4:2:0 needs it; callers already force it
/// via `fit_within_even`). Conversion is the standard integer BT.601
/// limited-range transform; chroma is the average of each 2×2 luma block.
/// The common no-scale case takes a contiguous fast path — the column map's
/// gather defeats vectorization, and native-resolution streaming is the
/// default.
pub fn scale_rgba_to_i420(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut out = Vec::new();
    scale_rgba_to_i420_into(src, sw, sh, dw, dh, &mut out);
    out
}

/// [`scale_rgba_to_i420`] into a reused buffer — the streaming shape. A
/// multi-megabyte output allocated fresh per frame costs far more than the
/// arithmetic (large allocations bypass the heap to the OS, and every page
/// is demand-zeroed on first touch — per frame, per thread); `out` is
/// resized (a no-op at steady state) and every byte is overwritten.
pub fn scale_rgba_to_i420_into(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32, out: &mut Vec<u8>) {
    if (dw, dh) == (sw, sh) {
        convert_native::<false>(src, sw as usize, sh as usize, out)
    } else {
        convert_scaled::<false>(src, sw as usize, sh as usize, dw as usize, dh as usize, out)
    }
}

/// [`scale_rgba_to_i420`]'s **NV12** sibling — Y plane, then interleaved
/// U/V — the layout every hardware H.264 MFT ingests. Producing it directly
/// deletes the encoder-side I420→NV12 interleave pass (a full extra chroma
/// walk per frame). Same sampling, same BT.601 math, same fast path.
pub fn scale_rgba_to_nv12(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut out = Vec::new();
    scale_rgba_to_nv12_into(src, sw, sh, dw, dh, &mut out);
    out
}

/// [`scale_rgba_to_nv12`] into a reused buffer — see
/// [`scale_rgba_to_i420_into`] for why the reuse matters.
pub fn scale_rgba_to_nv12_into(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32, out: &mut Vec<u8>) {
    if (dw, dh) == (sw, sh) {
        convert_native::<true>(src, sw as usize, sh as usize, out)
    } else {
        convert_scaled::<true>(src, sw as usize, sh as usize, dw as usize, dh as usize, out)
    }
}

/// Write one 2×2 block's chroma into `chroma` for either layout: NV12
/// interleaves U,V; I420 lays the U plane before the V plane (`csize`
/// apart).
#[inline(always)]
fn put_chroma<const NV12: bool>(chroma: &mut [u8], csize: usize, ci: usize, u: u8, v: u8) {
    if NV12 {
        chroma[ci * 2] = u;
        chroma[ci * 2 + 1] = v;
    } else {
        chroma[ci] = u;
        chroma[csize + ci] = v;
    }
}

/// The scaled path: nearest-neighbour via a precomputed column map.
fn convert_scaled<const NV12: bool>(
    src: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    out: &mut Vec<u8>,
) {
    debug_assert!(
        dw.is_multiple_of(2) && dh.is_multiple_of(2),
        "4:2:0 needs even output edges"
    );
    let xmap: Vec<usize> = (0..dw).map(|x| (x * sw / dw) * 4).collect();
    let ysize = dw * dh;
    let csize = (dw / 2) * (dh / 2);
    out.resize(ysize + 2 * csize, 0);
    let (y_plane, chroma) = out.split_at_mut(ysize);
    let cw = dw / 2;
    // Two output rows at a time: 4:2:0 chroma is one sample per 2×2 luma block.
    for by in 0..dh / 2 {
        let y0 = 2 * by;
        let y1 = y0 + 1;
        let srow0 = &src[(y0 * sh / dh) * sw * 4..][..sw * 4];
        let srow1 = &src[(y1 * sh / dh) * sw * 4..][..sw * 4];
        for bx in 0..cw {
            let x0 = 2 * bx;
            let (sx0, sx1) = (xmap[x0], xmap[x0 + 1]);
            let p00 = &srow0[sx0..sx0 + 3];
            let p10 = &srow0[sx1..sx1 + 3];
            let p01 = &srow1[sx0..sx0 + 3];
            let p11 = &srow1[sx1..sx1 + 3];
            y_plane[y0 * dw + x0] = rgb_to_y(p00);
            y_plane[y0 * dw + x0 + 1] = rgb_to_y(p10);
            y_plane[y1 * dw + x0] = rgb_to_y(p01);
            y_plane[y1 * dw + x0 + 1] = rgb_to_y(p11);
            // Average the 2×2 RGB block, then convert once for U and V.
            let avg =
                |i: usize| (p00[i] as u32 + p10[i] as u32 + p01[i] as u32 + p11[i] as u32 + 2) / 4;
            let (r, g, b) = (avg(0), avg(1), avg(2));
            put_chroma::<NV12>(
                chroma,
                csize,
                by * cw + bx,
                rgb_to_u(r, g, b),
                rgb_to_v(r, g, b),
            );
        }
    }
}

/// The no-scale path: contiguous reads, no column map — identical sampling
/// and math to [`convert_scaled`] at 1:1 (the equality is tested), in the
/// shape the vectorizer can actually chew on.
fn convert_native<const NV12: bool>(src: &[u8], w: usize, h: usize, out: &mut Vec<u8>) {
    debug_assert!(
        w.is_multiple_of(2) && h.is_multiple_of(2),
        "4:2:0 needs even edges"
    );
    let ysize = w * h;
    let csize = (w / 2) * (h / 2);
    out.resize(ysize + 2 * csize, 0);
    let (y_plane, chroma) = out.split_at_mut(ysize);
    let cw = w / 2;
    for by in 0..h / 2 {
        let y0 = 2 * by;
        let srow0 = &src[y0 * w * 4..][..w * 4];
        let srow1 = &src[(y0 + 1) * w * 4..][..w * 4];
        let (yrow0, rest) = y_plane[y0 * w..].split_at_mut(w);
        let yrow1 = &mut rest[..w];
        for bx in 0..cw {
            let sx = bx * 8;
            let p00 = &srow0[sx..sx + 3];
            let p10 = &srow0[sx + 4..sx + 7];
            let p01 = &srow1[sx..sx + 3];
            let p11 = &srow1[sx + 4..sx + 7];
            yrow0[2 * bx] = rgb_to_y(p00);
            yrow0[2 * bx + 1] = rgb_to_y(p10);
            yrow1[2 * bx] = rgb_to_y(p01);
            yrow1[2 * bx + 1] = rgb_to_y(p11);
            let avg =
                |i: usize| (p00[i] as u32 + p10[i] as u32 + p01[i] as u32 + p11[i] as u32 + 2) / 4;
            let (r, g, b) = (avg(0), avg(1), avg(2));
            put_chroma::<NV12>(
                chroma,
                csize,
                by * cw + bx,
                rgb_to_u(r, g, b),
                rgb_to_v(r, g, b),
            );
        }
    }
}

#[inline]
fn rgb_to_y(p: &[u8]) -> u8 {
    let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8
}

#[inline]
fn rgb_to_u(r: u32, g: u32, b: u32) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

#[inline]
fn rgb_to_v(r: u32, g: u32, b: u32) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

/// BGRA rows laid `src_pitch` bytes apart (a D3D mapping's RowPitch) →
/// tightly packed RGBA with alpha forced opaque, into a reused buffer.
/// `dst` is resized to exactly `w*h*4` — a no-op after the first call at a
/// given size, so a persistent buffer never re-zeroes.
///
/// This is the DXGI readback's per-frame swizzle. It lives in this crate so
/// release builds run it at opt-level 3: in the node crate's size-optimized
/// profile the identical loop measured markedly slower per 4K frame (see
/// the win_capture bench, which keeps the old-home shape for comparison).
pub fn bgra_to_rgba_into(src: &[u8], src_pitch: usize, w: usize, h: usize, dst: &mut Vec<u8>) {
    dst.resize(w * h * 4, 0);
    for row in 0..h {
        let s = &src[row * src_pitch..][..w * 4];
        let d = &mut dst[row * w * 4..][..w * 4];
        let (dst_pixels, _) = d.as_chunks_mut::<4>();
        let (src_pixels, _) = s.as_chunks::<4>();
        for (dp, sp) in dst_pixels.iter_mut().zip(src_pixels) {
            dp[0] = sp[2];
            dp[1] = sp[1];
            dp[2] = sp[0];
            dp[3] = 255;
        }
    }
}

/// Rotate a packed RGBA buffer clockwise by `quarter_turns` × 90°
/// (`quarter_turns` is taken mod 4). For an odd number of turns the output
/// dimensions are swapped. Used to bring a capture backend's raw, *unrotated*
/// framebuffer (Windows DXGI hands one over) upright so the streamed video
/// matches a physically-rotated monitor — the orientation remote control
/// already targets. Index-mapped 4-byte copies, like the scalers above.
pub fn rotate_rgba(src: &[u8], w: u32, h: u32, quarter_turns: u8) -> (Vec<u8>, u32, u32) {
    let (w, h) = (w as usize, h as usize);
    match quarter_turns & 3 {
        // No rotation — hand the buffer straight back.
        0 => (src.to_vec(), w as u32, h as u32),
        // 90° clockwise: dest is h×w; src (x, y) → dest (h-1-y, x).
        1 => {
            let (ow, oh) = (h, w);
            let mut out = vec![0u8; ow * oh * 4];
            for y in 0..h {
                for x in 0..w {
                    let s = (y * w + x) * 4;
                    let d = (x * ow + (h - 1 - y)) * 4;
                    out[d..d + 4].copy_from_slice(&src[s..s + 4]);
                }
            }
            (out, ow as u32, oh as u32)
        }
        // 180°: same dims, reversed pixel order.
        2 => {
            let mut out = vec![0u8; w * h * 4];
            for y in 0..h {
                for x in 0..w {
                    let s = (y * w + x) * 4;
                    let d = ((h - 1 - y) * w + (w - 1 - x)) * 4;
                    out[d..d + 4].copy_from_slice(&src[s..s + 4]);
                }
            }
            (out, w as u32, h as u32)
        }
        // 270° clockwise (= 90° counter-clockwise): src (x, y) → dest (y, w-1-x).
        _ => {
            let (ow, oh) = (h, w);
            let mut out = vec![0u8; ow * oh * 4];
            for y in 0..h {
                for x in 0..w {
                    let s = (y * w + x) * 4;
                    let d = ((w - 1 - x) * ow + y) * 4;
                    out[d..d + 4].copy_from_slice(&src[s..s + 4]);
                }
            }
            (out, ow as u32, oh as u32)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_rgba_turns_and_swaps_dimensions() {
        // A 2×1 image: pixel A at (0,0), pixel B at (1,0).
        let a = [1, 2, 3, 4];
        let b = [5, 6, 7, 8];
        let src: Vec<u8> = a.iter().chain(&b).copied().collect();

        // 0 turns is the identity.
        assert_eq!(rotate_rgba(&src, 2, 1, 0), (src.clone(), 2, 1));

        // 90° CW: 2×1 → 1×2. A (left) goes to the top, B to the bottom.
        let (out, w, h) = rotate_rgba(&src, 2, 1, 1);
        assert_eq!((w, h), (1, 2));
        assert_eq!(out, [a, b].concat());

        // 270° CW: 2×1 → 1×2. A to the bottom, B to the top.
        let (out, w, h) = rotate_rgba(&src, 2, 1, 3);
        assert_eq!((w, h), (1, 2));
        assert_eq!(out, [b, a].concat());

        // 180°: dims unchanged, order reversed (A and B swap).
        let (out, w, h) = rotate_rgba(&src, 2, 1, 2);
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, [b, a].concat());

        // Four 90° turns return to the original (round-trip sanity).
        let mut buf = (src.clone(), 2u32, 1u32);
        for _ in 0..4 {
            buf = rotate_rgba(&buf.0, buf.1, buf.2, 1);
        }
        assert_eq!(buf, (src, 2, 1));
    }

    #[test]
    fn dxgi_rotation_enum_maps_to_clockwise_turns() {
        // DXGI_OUTDUPL_DESC.Rotation enum values: UNSPECIFIED=0, IDENTITY=1,
        // ROTATE90=2, ROTATE180=3, ROTATE270=4. To bring the raw native
        // scan-out upright, rotate clockwise by `max(0, enum-1)` turns — the
        // mapping win_capture encodes as degrees (90→1, 180→2, 270→3) and the
        // operator orient_to_monitor applies. Asserted here, in the shared
        // crate, since the live DXGI constants are Windows-only.
        let enum_to_turns = |e: i32| -> u8 { (e.saturating_sub(1).max(0) as u8) & 3 };
        assert_eq!(enum_to_turns(0), 0, "UNSPECIFIED → 0");
        assert_eq!(enum_to_turns(1), 0, "IDENTITY → 0");
        assert_eq!(enum_to_turns(2), 1, "ROTATE90 → 1 CW");
        assert_eq!(enum_to_turns(3), 2, "ROTATE180 → 2");
        assert_eq!(enum_to_turns(4), 3, "ROTATE270 → 3 CW");

        // And those turn counts produce the expected dim behavior on a 4×2.
        let buf: Vec<u8> = (0..(4 * 2 * 4)).map(|i| i as u8).collect();
        assert_eq!(rotate_rgba(&buf, 4, 2, enum_to_turns(2)).1, 2); // 90 → w becomes 2
        assert_eq!(rotate_rgba(&buf, 4, 2, enum_to_turns(3)).1, 4); // 180 → w stays 4
        assert_eq!(rotate_rgba(&buf, 4, 2, enum_to_turns(4)).1, 2); // 270 → w becomes 2
    }

    #[test]
    fn scale_rgba_samples_the_right_pixels() {
        // 2x1 image: red then blue. Downscale to 1x1 keeps the left pixel.
        let src = [255, 0, 0, 255, 0, 0, 255, 255];
        assert_eq!(scale_rgba(&src, 2, 1, 1, 1), vec![255, 0, 0, 255]);
        // Growth repeats pixels (callers never ask, the fn never errors).
        let one = [9, 8, 7, 255];
        assert_eq!(scale_rgba(&one, 1, 1, 2, 2), one.repeat(4));
    }

    #[test]
    fn i420_has_correct_layout_and_neutral_chroma() {
        // A 2×2 mid-grey image: every channel 128, alpha ignored.
        let grey = [128u8, 128, 128, 255];
        let src: Vec<u8> = grey.repeat(4);
        let out = scale_rgba_to_i420(&src, 2, 2, 2, 2);
        // Y plane (4) + U (1) + V (1) for a 2×2.
        assert_eq!(out.len(), 2 * 2 + 1 + 1);
        // Grey → Y≈126 (16 + 219*0.5), U=V=128 (no colour).
        for &y in &out[..4] {
            assert!((125..=127).contains(&y), "grey luma {y} out of range");
        }
        assert_eq!(out[4], 128, "neutral U");
        assert_eq!(out[5], 128, "neutral V");
    }

    #[test]
    fn i420_black_and_white_hit_limited_range_endpoints() {
        let black: Vec<u8> = [0u8, 0, 0, 255].repeat(4);
        let white: Vec<u8> = [255u8, 255, 255, 255].repeat(4);
        let kb = scale_rgba_to_i420(&black, 2, 2, 2, 2);
        let kw = scale_rgba_to_i420(&white, 2, 2, 2, 2);
        // BT.601 limited range: black luma 16, white luma 235.
        assert!(kb[..4].iter().all(|&y| y == 16), "black luma = 16");
        assert!(kw[..4].iter().all(|&y| y == 235), "white luma = 235");
    }

    #[test]
    fn i420_downscale_picks_the_same_columns_as_the_rgb_scaler() {
        // 4×2 → 2×2: nearest-neighbour keeps columns 0 and 2, rows 0 and 1.
        // Build a frame whose top-left 2×2 (after subsample) is pure red.
        let red = [255u8, 0, 0, 255];
        let blu = [0u8, 0, 255, 255];
        // row pattern: red blu red blu  (cols 0,2 = red; the scaler samples those)
        let row: Vec<u8> = [red, blu, red, blu].concat();
        let src: Vec<u8> = [row.clone(), row].concat();
        let out = scale_rgba_to_i420(&src, 4, 2, 2, 2);
        // Red in BT.601: Y≈81. All four luma should match the red sample.
        assert!(out[..4].iter().all(|&y| (80..=82).contains(&y)), "red luma");
    }

    #[test]
    fn rgb_variant_strips_alpha_and_samples_identically() {
        let src = [255, 0, 0, 255, 0, 0, 255, 255];
        assert_eq!(scale_rgba_to_rgb(&src, 2, 1, 1, 1), vec![255, 0, 0]);
        // The no-scale case is a pure strip pass.
        assert_eq!(
            scale_rgba_to_rgb(&src, 2, 1, 2, 1),
            vec![255, 0, 0, 0, 0, 255]
        );
    }

    /// A deterministic multi-tone frame — every 2×2 block differs, so any
    /// sampling or plane-layout mistake shows up as a byte mismatch.
    fn patterned_rgba(w: usize, h: usize) -> Vec<u8> {
        (0..w * h * 4).map(|i| (i * 37 % 251) as u8).collect()
    }

    #[test]
    fn native_fast_path_matches_the_scaled_path_exactly() {
        // The 1:1 fast path must be indistinguishable from the generic path
        // for both layouts — same sampling, same math, different loop shape.
        let (w, h) = (8usize, 6usize);
        let src = patterned_rgba(w, h);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        convert_native::<false>(&src, w, h, &mut a);
        convert_scaled::<false>(&src, w, h, w, h, &mut b);
        assert_eq!(a, b, "i420");
        convert_native::<true>(&src, w, h, &mut a);
        convert_scaled::<true>(&src, w, h, w, h, &mut b);
        assert_eq!(a, b, "nv12");
    }

    #[test]
    fn into_variants_reuse_the_buffer_and_match_the_allocating_path() {
        let (w, h) = (8u32, 6u32);
        let src = patterned_rgba(w as usize, h as usize);
        let mut reused = Vec::new();
        scale_rgba_to_nv12_into(&src, w, h, w, h, &mut reused);
        assert_eq!(reused, scale_rgba_to_nv12(&src, w, h, w, h));
        // A second convert at the same size must reuse the exact buffer and
        // fully overwrite it (poison first to prove every byte is written).
        let ptr = reused.as_ptr();
        reused.iter_mut().for_each(|b| *b = 0xAB);
        scale_rgba_to_nv12_into(&src, w, h, w, h, &mut reused);
        assert_eq!(reused.as_ptr(), ptr, "buffer reused in place");
        assert_eq!(
            reused,
            scale_rgba_to_nv12(&src, w, h, w, h),
            "no stale bytes"
        );
        // And a size change adjusts cleanly.
        scale_rgba_to_i420_into(&src, w, h, 4, 4, &mut reused);
        assert_eq!(reused, scale_rgba_to_i420(&src, w, h, 4, 4));
    }

    #[test]
    fn nv12_interleaves_the_same_chroma_i420_plans() {
        // NV12 must carry byte-identical Y and the same U/V values as I420,
        // just interleaved.
        let (w, h) = (8u32, 6u32);
        let src = patterned_rgba(w as usize, h as usize);
        let i420 = scale_rgba_to_i420(&src, w, h, w, h);
        let nv12 = scale_rgba_to_nv12(&src, w, h, w, h);
        let ysize = (w * h) as usize;
        let csize = ysize / 4;
        assert_eq!(i420[..ysize], nv12[..ysize], "identical luma");
        for ci in 0..csize {
            assert_eq!(nv12[ysize + ci * 2], i420[ysize + ci], "U {ci}");
            assert_eq!(nv12[ysize + ci * 2 + 1], i420[ysize + csize + ci], "V {ci}");
        }
        // The scaled path produces the same relationship.
        let i420s = scale_rgba_to_i420(&src, w, h, 4, 4);
        let nv12s = scale_rgba_to_nv12(&src, w, h, 4, 4);
        assert_eq!(i420s[..16], nv12s[..16]);
        assert_eq!(nv12s[16], i420s[16]);
        assert_eq!(nv12s[17], i420s[20]);
    }

    #[test]
    fn bgra_swizzle_honours_pitch_and_forces_opaque_alpha() {
        // Two BGRA pixels per row with 4 bytes of pitch padding; alpha bytes
        // deliberately garbage (DXGI leaves them undefined).
        #[rustfmt::skip]
        let src = [
            1u8, 2, 3, 9,   4, 5, 6, 9,   0xAA, 0xBB, 0xCC, 0xDD, // row 0 + pad
            7, 8, 9, 9,   10, 11, 12, 9,  0xAA, 0xBB, 0xCC, 0xDD, // row 1 + pad
        ];
        let mut dst = Vec::new();
        bgra_to_rgba_into(&src, 12, 2, 2, &mut dst);
        assert_eq!(
            dst,
            [
                3, 2, 1, 255, 6, 5, 4, 255, // row 0: B,G,R,A → R,G,B,255
                9, 8, 7, 255, 12, 11, 10, 255, // row 1
            ]
        );
        // Reuse at the same size must not re-zero or grow the buffer.
        let ptr = dst.as_ptr();
        bgra_to_rgba_into(&src, 12, 2, 2, &mut dst);
        assert_eq!(dst.as_ptr(), ptr, "buffer reused in place");
    }
}
