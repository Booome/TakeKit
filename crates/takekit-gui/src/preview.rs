//! The still picture behind the source editor.
//!
//! The editor shows one frozen frame and lets the user drag its edges, so what
//! it needs is a picture, not a stream: `takekit_core::source::preview` grabs
//! one through GDI, and this module turns it into something an `<img>` can
//! hold.
//!
//! Two things matter about that picture and they are not the same number:
//!
//! * the *reported* size is the source's own pixel size, which is the space
//!   every crop value is expressed in — the recorder crops those pixels, so
//!   the editor's numbers have to be those numbers;
//! * the *encoded* size is smaller, because a 4K still is 33 MB of BGRA and a
//!   data URL that size would stall the page. Display detail costs nothing
//!   here: an edge is dragged by pointing at it, and a keyboard nudge is
//!   always exactly one source pixel whatever the picture's resolution.

use base64::Engine as _;
use takekit_core::source::Capture;
use takekit_media::Snapshot;

/// The longest side the PNG is allowed to be.
///
/// 4096, not 2048: the crop workspace zooms with nearest-neighbour
/// interpolation so the user can see the source's own pixels as blocks, and a
/// downscaled picture would show the *wrong* blocks. 4K fits; only beyond it
/// is anything thrown away.
const MAX_SIDE: u32 = 4096;

/// What the editor gets: the picture (or why there is none) and the pixel
/// size every crop number is counted in.
#[derive(serde::Serialize)]
pub struct Still {
    /// `data:image/png;base64,…`, or `None` when nothing could be grabbed.
    pub image: Option<String>,
    pub width: u32,
    pub height: u32,
    /// Why there is no picture, for the panel to show in place of one.
    pub note: Option<String>,
}

/// Grab one still of `capture`.
///
/// The size is the take's, not the picture's: a crop is a set of numbers
/// against the frame that will be recorded, and those are known whether or not
/// a picture could be grabbed. The editor opens either way.
pub fn still(capture: &Capture) -> Still {
    let p = takekit_core::source::preview(capture);
    let mut note = p.reason;
    let image = p.frame.as_ref().and_then(|frame| {
        let reduced = shrink(frame, MAX_SIDE);
        match data_url(&reduced) {
            Ok(url) => Some(url),
            Err(e) => {
                note = Some(e);
                None
            }
        }
    });
    Still {
        image,
        width: p.width,
        height: p.height,
        note,
    }
}

/// Box-average a snapshot down to `max_side`, or copy it when it already fits.
///
/// Averaging rather than picking every n-th pixel: a screenshot is mostly thin
/// text and 1px borders, and dropping pixels turns those into garbage that
/// makes an edge look like it is somewhere it is not.
fn shrink(snapshot: &Snapshot, max_side: u32) -> Snapshot {
    let (width, height) = (snapshot.width, snapshot.height);
    if width == 0 || height == 0 {
        return Snapshot {
            width,
            height,
            bgra: snapshot.bgra.clone(),
        };
    }
    if width <= max_side && height <= max_side {
        return Snapshot {
            width,
            height,
            bgra: snapshot.bgra.clone(),
        };
    }
    let scale = (width.max(height) as f64) / f64::from(max_side);
    let out_w = ((f64::from(width) / scale).round() as u32).max(1);
    let out_h = ((f64::from(height) / scale).round() as u32).max(1);
    let mut bgra = vec![0u8; out_w as usize * out_h as usize * 4];
    let src = &snapshot.bgra;
    for oy in 0..out_h {
        // The source rows this output row covers. `max(x + 1)` keeps the
        // block non-empty when the ratio rounds to less than one row.
        let y0 = (oy as u64 * u64::from(height) / u64::from(out_h)) as usize;
        let y1 = (((oy + 1) as u64 * u64::from(height) / u64::from(out_h)) as usize).max(y0 + 1);
        for ox in 0..out_w {
            let x0 = (ox as u64 * u64::from(width) / u64::from(out_w)) as usize;
            let x1 = (((ox + 1) as u64 * u64::from(width) / u64::from(out_w)) as usize).max(x0 + 1);
            let mut sum = [0u32; 4];
            let mut count = 0u32;
            for y in y0..y1.min(height as usize) {
                let row = y * width as usize * 4;
                for x in x0..x1.min(width as usize) {
                    let at = row + x * 4;
                    let Some(px) = src.get(at..at + 4) else { continue };
                    for (slot, byte) in sum.iter_mut().zip(px) {
                        *slot += u32::from(*byte);
                    }
                    count += 1;
                }
            }
            let count = count.max(1);
            let at = (oy as usize * out_w as usize + ox as usize) * 4;
            for (i, slot) in sum.iter().enumerate() {
                bgra[at + i] = (*slot / count) as u8;
            }
        }
    }
    Snapshot {
        width: out_w,
        height: out_h,
        bgra,
    }
}

/// Encode a snapshot as a PNG data URL.
///
/// RGB, not RGBA: a screen grab is opaque, and an alpha channel that is 255
/// everywhere is a quarter more bytes to compress and to ship.
fn data_url(snapshot: &Snapshot) -> Result<String, String> {
    let pixels = snapshot.width as usize * snapshot.height as usize;
    if pixels == 0 || snapshot.bgra.len() < pixels * 4 {
        return Err("the grab came back empty".into());
    }
    let mut rgb = Vec::with_capacity(pixels * 3);
    for px in snapshot.bgra.chunks_exact(4) {
        // GDI hands over BGRA; an image file wants RGBA order.
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }

    let mut png_bytes: Vec<u8> = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, snapshot.width, snapshot.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        // The default (slow) filter set finds far more redundancy in a screen
        // full of flat panels and text than the fastest one does, and this
        // runs once per dialog, not per frame.
        encoder.set_compression(png::Compression::Default);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("png header: {e}"))?;
        writer
            .write_image_data(&rgb)
            .map_err(|e| format!("png data: {e}"))?;
    }
    Ok(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&png_bytes)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, bgra: [u8; 4]) -> Snapshot {
        Snapshot {
            width,
            height,
            bgra: bgra.repeat(width as usize * height as usize),
        }
    }

    /// A picture that already fits is handed back unchanged — the common case
    /// for a 1080p screen at 2048.
    #[test]
    fn a_small_snapshot_is_not_resized() {
        let snap = solid(120, 80, [1, 2, 3, 255]);
        let out = shrink(&snap, 2048);
        assert_eq!((out.width, out.height), (120, 80));
        assert_eq!(out.bgra, snap.bgra);
    }

    /// The long side is what the cap applies to, and the aspect ratio survives
    /// the division to whole pixels.
    #[test]
    fn a_large_snapshot_keeps_its_aspect_ratio() {
        let out = shrink(&solid(3840, 2160, [0, 0, 0, 255]), 2048);
        assert_eq!(out.width, 2048);
        assert_eq!(out.height, 1152);
        assert_eq!(out.bgra.len(), 2048 * 1152 * 4);
    }

    /// Averaging is the point: four pixels of different greys become their
    /// mean, where picking one of them would keep a hard edge that is not
    /// really there.
    #[test]
    fn shrinking_averages_the_pixels_it_merges() {
        let snap = Snapshot {
            width: 2,
            height: 2,
            bgra: vec![0, 0, 0, 255, 40, 40, 40, 255, 80, 80, 80, 255, 120, 120, 120, 255],
        };
        let out = shrink(&snap, 1);
        assert_eq!((out.width, out.height), (1, 1));
        assert_eq!(out.bgra, vec![60, 60, 60, 255]);
    }

    /// What the page receives is a PNG it can put straight into `src`.
    #[test]
    fn a_snapshot_becomes_a_png_data_url() {
        let url = data_url(&solid(4, 4, [10, 20, 30, 255])).unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        let raw = base64::engine::general_purpose::STANDARD
            .decode(url.trim_start_matches("data:image/png;base64,"))
            .unwrap();
        assert_eq!(&raw[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    }
}
