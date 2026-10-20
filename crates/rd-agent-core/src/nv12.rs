//! NV12 frames in system memory: converting BGRA images into them.
//!
//! The platforms' fast paths convert on the GPU (ScreenCaptureKit on macOS, a D3D11 video
//! processor on Windows). A capture path that has no GPU at its disposal (Windows' GDI
//! fallback, see `rd-platform-windows`'s `gdi` module) uses [`bgra_to_nv12`] instead. It
//! produces the same BT.709 video-range Y'CbCr the video processor is configured for (Y in
//! 16..=235, Cb/Cr in 16..=240, chroma averaged over 2x2 blocks), so a session looks the same
//! whichever path captured it.

use anyhow::ensure;

use crate::pattern::Nv12Planes;

/// BT.709, video range: `(Y, Cb, Cr)` for 8-bit full-range RGB. The coefficients are in
/// 256ths of the studio-range excursion (219 levels of luma, 224 of chroma).
fn ycbcr(r: i32, g: i32, b: i32) -> (u8, u8, u8) {
    let y = 16 + ((47 * r + 157 * g + 16 * b) >> 8);
    let cb = 128 + ((-26 * r - 87 * g + 113 * b) >> 8);
    let cr = 128 + ((113 * r - 102 * g - 11 * b) >> 8);
    let clamp = |v: i32| v.clamp(0, 255) as u8;
    (clamp(y), clamp(cb), clamp(cr))
}

/// Converts a BGRA image (the desktop, full-range RGB, top-down rows of `stride` bytes) into
/// NV12 planes of the same size. `width` and `height` must be even: NV12 subsamples chroma 2x2.
pub fn bgra_to_nv12(
    bgra: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    dst: &mut Nv12Planes<'_>,
) -> anyhow::Result<()> {
    let (w, h) = (width as usize, height as usize);
    ensure!(
        w > 0 && h > 0 && w.is_multiple_of(2) && h.is_multiple_of(2),
        "NV12 needs an even, non-zero size, got {width}x{height}"
    );
    ensure!(
        stride >= w * 4 && bgra.len() >= stride * (h - 1) + w * 4,
        "a {width}x{height} BGRA image with stride {stride} does not fit its {} bytes",
        bgra.len()
    );
    ensure!(
        dst.luma_stride >= w && dst.luma.len() >= dst.luma_stride * h,
        "the luma plane is too small for {width}x{height} at stride {}",
        dst.luma_stride
    );
    ensure!(
        dst.chroma_stride >= w && dst.chroma.len() >= dst.chroma_stride * (h / 2),
        "the chroma plane is too small for {width}x{height} at stride {}",
        dst.chroma_stride
    );
    for y in 0..h {
        let row = &bgra[y * stride..y * stride + w * 4];
        let luma = &mut dst.luma[y * dst.luma_stride..y * dst.luma_stride + w];
        for (px, y) in row.as_chunks::<4>().0.iter().zip(luma.iter_mut()) {
            *y = ycbcr(i32::from(px[2]), i32::from(px[1]), i32::from(px[0])).0;
        }
    }
    for cy in 0..h / 2 {
        let top = &bgra[cy * 2 * stride..];
        let bottom = &bgra[(cy * 2 + 1) * stride..];
        let chroma = &mut dst.chroma[cy * dst.chroma_stride..cy * dst.chroma_stride + w];
        for (cx, out) in chroma.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            // The 2x2 block of pixels `cx * 2` and `cx * 2 + 1` in both rows.
            let (mut r, mut g, mut b) = (0i32, 0i32, 0i32);
            for row in [top, bottom] {
                for px in row[cx * 8..cx * 8 + 8].as_chunks::<4>().0 {
                    b += i32::from(px[0]);
                    g += i32::from(px[1]);
                    r += i32::from(px[2]);
                }
            }
            let (_, cb, cr) = ycbcr(r / 4, g / 4, b / 4);
            out[0] = cb;
            out[1] = cr;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `width x height` BGRA image of one colour, `stride` bytes per row.
    fn frame(width: usize, height: usize, stride: usize, rgb: [u8; 3]) -> Vec<u8> {
        let mut data = vec![0xee; stride * height];
        for y in 0..height {
            for x in 0..width {
                let px = &mut data[y * stride + x * 4..y * stride + x * 4 + 4];
                px.copy_from_slice(&[rgb[2], rgb[1], rgb[0], 0xff]);
            }
        }
        data
    }

    fn planes(width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
        (
            vec![0; (width * height) as usize],
            vec![0; (width * height / 2) as usize],
        )
    }

    fn convert(width: u32, height: u32, stride: usize, rgb: [u8; 3]) -> (Vec<u8>, Vec<u8>) {
        let (mut luma, mut chroma) = planes(width, height);
        let mut dst = Nv12Planes {
            luma: &mut luma,
            luma_stride: width as usize,
            chroma: &mut chroma,
            chroma_stride: width as usize,
        };
        let bgra = frame(width as usize, height as usize, stride, rgb);
        bgra_to_nv12(&bgra, stride, width, height, &mut dst).unwrap();
        (luma, chroma)
    }

    #[test]
    fn black_and_white_are_video_range() {
        let (luma, chroma) = convert(4, 4, 16, [0, 0, 0]);
        assert!(luma.iter().all(|&y| y == 16), "{luma:?}");
        assert!(chroma.iter().all(|&c| c == 128), "{chroma:?}");

        let (luma, chroma) = convert(4, 4, 16, [255, 255, 255]);
        assert!(luma.iter().all(|&y| (235..=236).contains(&y)), "{luma:?}");
        assert!(chroma.iter().all(|&c| c == 128), "{chroma:?}");
    }

    #[test]
    fn chroma_points_where_the_colour_is() {
        // BT.709: red has the positive Cr, blue the positive Cb.
        let red = convert(2, 2, 8, [255, 0, 0]).1;
        assert!(red[1] > 128, "red Cr {}", red[1]);
        assert!(red[0] < 128, "red Cb {}", red[0]);
        let blue = convert(2, 2, 8, [0, 0, 255]).1;
        assert!(blue[0] > 128, "blue Cb {}", blue[0]);
        assert!(blue[1] < 128, "blue Cr {}", blue[1]);
        let green = convert(2, 2, 8, [0, 255, 0]).1;
        assert!(green[0] < 128 && green[1] < 128, "green {green:?}");
    }

    #[test]
    fn chroma_averages_the_2x2_block() {
        // Left column red, right column blue: the one chroma sample of a 2x2 frame sits
        // between what either would give alone.
        let (red, blue) = ([255u8, 0, 0], [0u8, 0, 255]);
        let mut bgra = frame(2, 2, 8, red);
        for y in 0..2 {
            bgra[y * 8 + 4..y * 8 + 8].copy_from_slice(&[blue[2], blue[1], blue[0], 0xff]);
        }
        let (mut luma, mut chroma) = planes(2, 2);
        let mut dst = Nv12Planes {
            luma: &mut luma,
            luma_stride: 2,
            chroma: &mut chroma,
            chroma_stride: 2,
        };
        bgra_to_nv12(&bgra, 8, 2, 2, &mut dst).unwrap();
        let mixed = ycbcr(127, 0, 128);
        assert_eq!((chroma[0], chroma[1]), (mixed.1, mixed.2));
        let red_y = ycbcr(255, 0, 0).0;
        let blue_y = ycbcr(0, 0, 255).0;
        assert_eq!(luma, [red_y, blue_y, red_y, blue_y]);
    }

    #[test]
    fn honours_strides_in_both_planes() {
        // Padding after every row (source) and a wider destination stride.
        let (luma, chroma) = convert(2, 2, 31, [255, 0, 0]);
        assert_eq!(luma.len(), 4);
        assert_eq!(chroma.len(), 2);
        let (mut wide_luma, mut wide_chroma) = (vec![0; 4 * 2], vec![0; 4]);
        let mut dst = Nv12Planes {
            luma: &mut wide_luma,
            luma_stride: 4,
            chroma: &mut wide_chroma,
            chroma_stride: 4,
        };
        let bgra = frame(2, 2, 31, [255, 0, 0]);
        bgra_to_nv12(&bgra, 31, 2, 2, &mut dst).unwrap();
        assert_eq!(&wide_luma[..2], &luma[..2]);
        assert_eq!(&wide_luma[4..6], &luma[2..], "the second row landed");
        assert_eq!(
            [wide_luma[2], wide_luma[3], wide_luma[6], wide_luma[7]],
            [0; 4],
            "padding stays untouched"
        );
        assert_eq!(&wide_chroma[..2], &chroma[..]);
    }

    #[test]
    fn rejects_what_it_cannot_convert() {
        let bgra = frame(4, 4, 16, [0, 0, 0]);
        let (mut luma, mut chroma) = planes(4, 4);
        let mut dst = Nv12Planes {
            luma: &mut luma,
            luma_stride: 4,
            chroma: &mut chroma,
            chroma_stride: 4,
        };
        // Odd sizes have no NV12 chroma layout.
        assert!(bgra_to_nv12(&bgra, 16, 3, 4, &mut dst).is_err());
        assert!(bgra_to_nv12(&bgra, 16, 4, 3, &mut dst).is_err());
        // A stride too small for the width, and a buffer too small for the stride.
        assert!(bgra_to_nv12(&bgra, 12, 4, 4, &mut dst).is_err());
        assert!(bgra_to_nv12(&bgra[..40], 16, 4, 4, &mut dst).is_err());
    }
}
