//! emote rendering: decode frames, cache, and draw into reserved cells across
//! the terminal-graphics tier ladder (native / sixel / half-block / text).

pub mod decode;
pub mod fb;
pub mod fx;
pub mod render;
