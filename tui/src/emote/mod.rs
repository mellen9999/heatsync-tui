//! emote rendering: decode frames, cache, and draw into reserved cells across
//! the terminal-graphics tier ladder (native / sixel / half-block / text).

pub mod blit;
pub mod decode;
pub mod fb;
#[cfg(target_os = "linux")]
pub mod fbdev;
pub mod fx;
pub mod render;
