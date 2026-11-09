//! emote frame decode — webp/gif/png → a flat list of rgba frames + delays.
//! pure-Rust (`image` crate), no libwebp/libgif C deps. static images decode to a
//! single frame; animated webp/gif expand to their real frames. the render layer
//! resizes to cell-pixel dims — decode stays terminal-agnostic and testable.

use std::io::Cursor;

use image::codecs::gif::GifDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, ImageFormat, RgbaImage};

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// one decoded frame: pixels + how long to show it.
#[derive(Clone)]
pub struct Frame {
    pub img: RgbaImage,
    pub delay_ms: u32,
}

/// a fully decoded emote — one or more frames.
pub struct Decoded {
    pub frames: Vec<Frame>,
    /// native pixel dims (frame 0) — used by tests + future aspect handling.
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
}

impl Decoded {
    #[allow(dead_code)]
    pub fn animated(&self) -> bool {
        self.frames.len() > 1
    }
}

/// decode raw image bytes into frames. animated formats expand; everything else
/// is a single frame. a zero-delay animation frame is floored to 20ms so a
/// malformed emote can't spin the animator at infinite fps (bulletproof).
pub fn decode(bytes: &[u8]) -> Result<Decoded, BoxErr> {
    guard(|| decode_inner(bytes))
}

thread_local!(static DECODING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });

/// run a decode so a panic inside it (untrusted bytes from strangers) becomes an
/// error — the emote shows as text — instead of taking the app down.
fn guard<T>(f: impl FnOnce() -> Result<T, BoxErr>) -> Result<T, BoxErr> {
    DECODING.with(|d| d.set(true));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    DECODING.with(|d| d.set(false));
    r.unwrap_or_else(|_| Err("decoder panicked".into()))
}

/// call once after the terminal hook is installed. a panic inside `guard` is
/// swallowed (no terminal restore, no stderr noise on the live screen); any
/// other panic runs the previous hook and, off the main thread, aborts as the
/// release profile used to — a dead worker must not leave a half-alive app on a
/// restored terminal.
pub fn install_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if DECODING.with(|d| d.get()) {
            return;
        }
        prev(info);
        if std::thread::current().name() != Some("main") {
            std::process::abort();
        }
    }));
}

fn decode_inner(bytes: &[u8]) -> Result<Decoded, BoxErr> {
    let fmt = image::guess_format(bytes)?;
    let frames = match fmt {
        ImageFormat::Gif => {
            let dec = GifDecoder::new(Cursor::new(bytes))?;
            collect(dec)?
        }
        ImageFormat::WebP => {
            let dec = WebPDecoder::new(Cursor::new(bytes))?;
            if dec.has_animation() {
                collect(dec)?
            } else {
                single(bytes)?
            }
        }
        _ => single(bytes)?,
    };
    let (width, height) = frames.first().map(|f| f.img.dimensions()).unwrap_or((0, 0));
    if frames.is_empty() {
        return Err("decoded zero frames".into());
    }
    Ok(Decoded {
        frames,
        width,
        height,
    })
}

fn single(bytes: &[u8]) -> Result<Vec<Frame>, BoxErr> {
    let img = image::load_from_memory(bytes)?.to_rgba8();
    Ok(vec![Frame { img, delay_ms: 0 }])
}

fn collect<'a>(dec: impl AnimationDecoder<'a>) -> Result<Vec<Frame>, BoxErr> {
    let mut out = Vec::new();
    for f in dec.into_frames() {
        let f = f?;
        let (num, den) = f.delay().numer_denom_ms();
        let ms = num.checked_div(den).unwrap_or(0);
        out.push(Frame {
            img: f.into_buffer(),
            delay_ms: ms.max(20), // floor: never spin faster than 50fps per frame
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame as IFrame, Rgba};

    fn solid(w: u32, h: u32, px: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba(px))
    }

    #[test]
    fn static_png_is_one_frame() {
        let mut buf = Vec::new();
        solid(8, 8, [255, 135, 0, 255])
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        let d = decode(&buf).unwrap();
        assert_eq!(d.frames.len(), 1);
        assert!(!d.animated());
        assert_eq!((d.width, d.height), (8, 8));
    }

    #[test]
    fn animated_gif_expands_to_all_frames() {
        let mut buf = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut buf);
            for px in [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]] {
                let frame =
                    IFrame::from_parts(solid(4, 4, px), 0, 0, Delay::from_numer_denom_ms(100, 1));
                enc.encode_frame(frame).unwrap();
            }
        }
        let d = decode(&buf).unwrap();
        assert_eq!(d.frames.len(), 3);
        assert!(d.animated());
        assert!(d.frames.iter().all(|f| f.delay_ms >= 20));
    }

    #[test]
    fn a_panicking_decode_is_an_error_and_the_app_lives() {
        let r: Result<(), BoxErr> = guard(|| panic!("hostile image"));
        assert_eq!(r.unwrap_err().to_string(), "decoder panicked");
        // the flag is cleared and the next decode works
        assert!(!DECODING.with(|d| d.get()));
        assert!(guard(|| Ok(7)).unwrap() == 7);
    }

    #[test]
    fn garbage_bytes_error_not_panic() {
        assert!(decode(b"not an image at all").is_err());
    }
}
