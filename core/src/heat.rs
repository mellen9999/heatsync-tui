//! the heat ramp — one decaying scalar → a stepped temperature color.
//! terminal-native: a tier is an ANSI color name plus vt320 attributes, so it
//! follows the user's terminal palette. each frontend maps `Look` to its toolkit.

/// the 8 ansi colors, by name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hue {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
}

/// a hue plus emphasis attributes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Look {
    pub hue: Hue,
    pub bold: bool,
    pub reversed: bool,
    pub blink: bool,
}

impl Look {
    pub const fn new(hue: Hue) -> Look {
        Look { hue, bold: false, reversed: false, blink: false }
    }
    pub const fn bold(mut self) -> Look {
        self.bold = true;
        self
    }
    pub const fn reversed(mut self) -> Look {
        self.reversed = true;
        self
    }
    pub const fn blink(mut self) -> Look {
        self.blink = true;
        self
    }
}

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

    /// how this tier reads on the 8-color palette: cold = plain white, then
    /// white bold → yellow → yellow bold → red bold → red reversed + blink.
    pub fn look(self) -> Look {
        match self {
            Tier::Zero | Tier::Cold => Look::new(Hue::White),
            Tier::Spark => Look::new(Hue::White).bold(),
            Tier::Warm => Look::new(Hue::Yellow),
            Tier::Hot => Look::new(Hue::Yellow).bold(),
            Tier::Erupting => Look::new(Hue::Red).bold(),
            Tier::Mythic => Look::new(Hue::Red).reversed().blink(),
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

/// convenience: heat → look.
pub fn look(heat: f64) -> Look {
    Tier::of(heat).look()
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
    fn ladder_escalates_and_blinks_only_at_the_top() {
        assert_eq!(Tier::Cold.look(), Look::new(Hue::White));
        assert_eq!(Tier::Erupting.look(), Look::new(Hue::Red).bold());
        for t in [Tier::Zero, Tier::Cold, Tier::Spark, Tier::Warm, Tier::Hot, Tier::Erupting] {
            assert!(!t.look().blink && !t.look().reversed);
        }
        assert!(Tier::Mythic.look().blink && Tier::Mythic.look().reversed);
    }

    #[test]
    fn only_mythic_has_a_marker() {
        assert_eq!(Tier::Mythic.marker(), "???");
        assert_eq!(Tier::Warm.marker(), "");
    }
}
