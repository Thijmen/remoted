//! The tray icon's pictures: rd Desktop's icon (macOS: its menu bar template image), dimmed
//! while the agent is not reachable, with a dot while someone is connected.

use anyhow::Context as _;

use super::IconState;

/// macOS: drawn by the menu bar in its own colour (only the alpha channel counts).
#[cfg(target_os = "macos")]
const PNG: &[u8] = include_bytes!("../../../../clients/desktop/src-tauri/icons/tray-template.png");
#[cfg(windows)]
const PNG: &[u8] = include_bytes!("../../../../clients/desktop/src-tauri/icons/32x32.png");

/// The dot's colour where the icon is drawn as is (Windows); a template ignores it.
const DOT: [u8; 3] = [0xe5, 0x48, 0x4d];
/// Opacity of the dimmed icon.
const DIM: f32 = 0.4;

/// An RGBA picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// The base picture of the icon.
pub fn base() -> anyhow::Result<Rgba> {
    decode(PNG)
}

pub fn for_state(base: &Rgba, state: IconState) -> Rgba {
    match state {
        IconState::Idle => base.clone(),
        IconState::Away => dimmed(base),
        IconState::Active => with_dot(base),
    }
}

fn decode(png_bytes: &[u8]) -> anyhow::Result<Rgba> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("reading the icon")?;
    let mut buf = vec![0; reader.output_buffer_size().context("icon too large")?];
    let info = reader.next_frame(&mut buf).context("decoding the icon")?;
    buf.truncate(info.buffer_size());
    let pixels = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 0xff])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[g, a]| [g, g, g, a])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 0xff]).collect(),
        png::ColorType::Indexed => anyhow::bail!("indexed icon not expanded"),
    };
    Ok(Rgba {
        width: info.width,
        height: info.height,
        pixels,
    })
}

fn dimmed(base: &Rgba) -> Rgba {
    let mut out = base.clone();
    for px in out.pixels.as_chunks_mut::<4>().0 {
        px[3] = (f32::from(px[3]) * DIM).round() as u8;
    }
    out
}

/// A filled dot in the bottom-right corner, with a transparent ring around it so it stands
/// out from the picture under it.
fn with_dot(base: &Rgba) -> Rgba {
    let mut out = base.clone();
    let size = base.width.min(base.height) as f32;
    let r = size * 0.22;
    let ring = size * 0.08;
    let (cx, cy) = (base.width as f32 - r - 0.5, base.height as f32 - r - 0.5);
    for y in 0..base.height {
        for x in 0..base.width {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let i = ((y * base.width + x) * 4) as usize;
            let px = &mut out.pixels[i..i + 4];
            if d <= r + 0.5 {
                // Antialiased edge; the ring around it is transparent.
                px[..3].copy_from_slice(&DOT);
                px[3] = (255.0 * (r + 0.5 - d).clamp(0.0, 1.0)).round() as u8;
            } else if d <= r + ring {
                px[3] = 0;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_derives() {
        let base = base().unwrap();
        assert!(base.width >= 16 && base.width == base.height);
        assert_eq!(base.pixels.len(), (base.width * base.height * 4) as usize);
        let alpha = |p: &Rgba| {
            p.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .map(|px| u32::from(px[3]))
                .sum::<u32>()
        };
        assert!(alpha(&dimmed(&base)) < alpha(&base));
        let dot = with_dot(&base);
        // The dot's centre, near the bottom-right corner, is the dot's colour now.
        let (w, h) = (base.width, base.height);
        let r = (w as f32 * 0.22) as u32;
        let i = (((h - 1 - r) * w + (w - 1 - r)) * 4) as usize;
        assert_eq!(&dot.pixels[i..i + 3], &DOT);
        assert_eq!(dot.pixels[i + 3], 255);
        assert_eq!(for_state(&base, IconState::Idle), base);
    }
}
