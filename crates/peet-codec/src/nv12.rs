//! CPU colour conversion BGRA8 to NV12 (BT.709, limited range).
//!
//! This is the fallback for encoders that only take NV12 when no GPU path is available. The
//! GPU converter in the Windows backend produces the same colour space so streams look
//! identical whichever path produced them.

/// Size in bytes of an NV12 image of the given dimensions.
pub fn nv12_len(width: u32, height: u32) -> usize {
    let w = width as usize;
    let h = height as usize;
    w * h + 2 * w.div_ceil(2) * h.div_ceil(2)
}

/// Converts `bgra` (rows of `stride` bytes) into NV12 in `out`, resizing `out`.
///
/// Chroma is averaged over each 2x2 block. Odd widths or heights are handled by clamping.
#[allow(clippy::many_single_char_names)]
pub fn bgra_to_nv12(bgra: &[u8], stride: usize, width: u32, height: u32, out: &mut Vec<u8>) {
    let w = width as usize;
    let h = height as usize;
    assert!(stride >= w * 4, "stride shorter than a row");
    assert!(bgra.len() >= stride * h, "buffer shorter than image");
    out.resize(nv12_len(width, height), 0);
    let (y_plane, uv_plane) = out.split_at_mut(w * h);

    // Luma, full resolution. BT.709 limited range, 8-bit fixed point (scaled by 256).
    for y in 0..h {
        let row = &bgra[y * stride..y * stride + w * 4];
        let dst = &mut y_plane[y * w..(y + 1) * w];
        for (px, d) in row.as_chunks::<4>().0.iter().zip(dst.iter_mut()) {
            let b = i32::from(px[0]);
            let g = i32::from(px[1]);
            let r = i32::from(px[2]);
            *d = ((47 * r + 157 * g + 16 * b + 128) >> 8)
                .clamp(0, 255)
                .cast_unsigned() as u8
                + 16;
        }
    }

    // Chroma, 2x2 averaged, interleaved U then V.
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    for cy in 0..ch {
        let y0 = cy * 2;
        let y1 = (y0 + 1).min(h - 1);
        for cx in 0..cw {
            let x0 = cx * 2;
            let x1 = (x0 + 1).min(w - 1);
            let mut r = 0i32;
            let mut g = 0i32;
            let mut b = 0i32;
            for (yy, xx) in [(y0, x0), (y0, x1), (y1, x0), (y1, x1)] {
                let p = yy * stride + xx * 4;
                b += i32::from(bgra[p]);
                g += i32::from(bgra[p + 1]);
                r += i32::from(bgra[p + 2]);
            }
            // Averages scaled by 4; the coefficients below are scaled by 256/4 = 64.
            let u = ((-26 * r - 87 * g + 112 * b + 512) >> 10) + 128;
            let v = ((112 * r - 102 * g - 10 * b + 512) >> 10) + 128;
            let o = cy * cw * 2 + cx * 2;
            uv_plane[o] = u.clamp(0, 255).cast_unsigned() as u8;
            uv_plane[o + 1] = v.clamp(0, 255).cast_unsigned() as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, b: u8, g: u8, r: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            v.extend_from_slice(&[b, g, r, 255]);
        }
        v
    }

    #[test]
    fn white_and_black_hit_limited_range_extremes() {
        let mut out = Vec::new();
        bgra_to_nv12(&solid(4, 2, 255, 255, 255), 16, 4, 2, &mut out);
        assert_eq!(out.len(), nv12_len(4, 2));
        assert!(
            out[..8].iter().all(|&y| (234..=235).contains(&y)),
            "{out:?}"
        );
        assert!(
            out[8..].iter().all(|&c| (127..=129).contains(&c)),
            "{out:?}"
        );

        bgra_to_nv12(&solid(4, 2, 0, 0, 0), 16, 4, 2, &mut out);
        assert!(out[..8].iter().all(|&y| y == 16));
        assert!(out[8..].iter().all(|&c| (127..=129).contains(&c)));
    }

    #[test]
    fn pure_red_has_high_v_low_u() {
        let mut out = Vec::new();
        bgra_to_nv12(&solid(2, 2, 0, 0, 255), 8, 2, 2, &mut out);
        let u = out[4];
        let v = out[5];
        assert!(u < 110, "u={u}");
        assert!(v > 230, "v={v}");
    }

    #[test]
    fn handles_stride_and_odd_dimensions() {
        let width = 3;
        let height = 3;
        let stride = 32;
        let mut bgra = vec![0u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                bgra[y * stride + x * 4 + 1] = 255; // green
            }
        }
        let mut out = Vec::new();
        bgra_to_nv12(&bgra, stride, width as u32, height as u32, &mut out);
        assert_eq!(out.len(), nv12_len(3, 3));
        assert!(out[..9].iter().all(|&y| y > 150));
    }
}
