//! the heat ramp — one decaying scalar → a stepped temperature color.
//! mirrors client/config/colors.js (the ONE source of truth). terminal-native:
//! we return the xterm-256 index, not css. the tui maps that to a cell color.

/// tier thresholds, ascending. matches HEAT_THRESHOLDS in colors.js.
pub const SPARK: f64 = 10.0;
pub const WARM: f64 = 50.0;
pub const HOT: f64 = 250.0;
pub const ERUPTING: f64 = 1000.0;
pub const MYTHIC: f64 = 5000.0;

/// live-chat heat = a decaying counter (the same model as post heat). each
/// message adds `INCREMENT`; heat halves every `HALFLIFE_MS`. tuned so steady
/// chat rates land on the ramp: ~0.5 msg/s → cold/spark, ~5 msg/s → hot,
/// a raid (~50 msg/s) → erupting. no server value needed — it's local velocity.
pub const HALFLIFE_MS: f64 = 20_000.0;
pub const INCREMENT: f64 = 2.0;

/// decay `heat` forward by `dt_ms` milliseconds. exact exponential half-life.
pub fn decay(heat: f64, dt_ms: f64) -> f64 {
    if dt_ms <= 0.0 {
        return heat;
    }
    heat * 0.5_f64.powf(dt_ms / HALFLIFE_MS)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Zero,
    Cold,
    Spark,
    Warm,
    Hot,
    Erupting,
    Mythic,
}

impl Tier {
    pub fn of(heat: f64) -> Tier {
        match heat {
            h if h >= MYTHIC => Tier::Mythic,
            h if h >= ERUPTING => Tier::Erupting,
            h if h >= HOT => Tier::Hot,
            h if h >= WARM => Tier::Warm,
            h if h >= SPARK => Tier::Spark,
            h if h >= 1.0 => Tier::Cold,
            _ => Tier::Zero,
        }
    }

    /// xterm-256 index nearest the css hex in colors.js.
    /// zero #444=238 cold #585858=240 spark #ff5f00=202 warm #ff8700=208
    /// hot #ffaf00=214 erupting #ffff00=226 mythic #ffffff=231
    pub fn xterm(self) -> u8 {
        match self {
            // never below 244: the cold tiers are body text, and xterm 236-240 is
            // dark grey — legible in a truecolor emulator, but the kernel console
            // has 16 colors and folds them onto black, so every cold message went
            // invisible on a tty. dim must still be readable.
            Tier::Zero => 245,
            Tier::Cold => 250,
            Tier::Spark => 202,
            Tier::Warm => 208,
            Tier::Hot => 214,
            Tier::Erupting => 226,
            Tier::Mythic => 231,
        }
    }

    /// the ??? capstone marker — the only non-color glyph, mythic only.
    pub fn marker(self) -> &'static str {
        match self {
            Tier::Mythic => "???",
            _ => "",
        }
    }
}

/// convenience: heat → xterm color index.
pub fn color(heat: f64) -> u8 {
    Tier::of(heat).xterm()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_are_inclusive_lower() {
        assert_eq!(Tier::of(0.0), Tier::Zero);
        assert_eq!(Tier::of(0.9), Tier::Zero);
        assert_eq!(Tier::of(1.0), Tier::Cold);
        assert_eq!(Tier::of(9.9), Tier::Cold);
        assert_eq!(Tier::of(10.0), Tier::Spark);
        assert_eq!(Tier::of(50.0), Tier::Warm);
        assert_eq!(Tier::of(250.0), Tier::Hot);
        assert_eq!(Tier::of(1000.0), Tier::Erupting);
        assert_eq!(Tier::of(5000.0), Tier::Mythic);
        assert_eq!(Tier::of(999999.0), Tier::Mythic);
    }

    #[test]
    fn only_mythic_has_a_marker() {
        assert_eq!(Tier::Mythic.marker(), "???");
        assert_eq!(Tier::Warm.marker(), "");
    }
}
