//! name paints: heatsync's own paint spec and 7TV's paint object, reduced to
//! one gradient model a terminal can sample per character. pure data in,
//! rgb out — fetching and caching live in the tui.
//!
//! a terminal cell has one color, so only the resting fill survives: motion
//! effects, glow, shadows and image fills are dropped (an image paint parses
//! to nothing and the name keeps its plain color).

use serde_json::Value;

/// cells are about twice as tall as wide — the name's box for gradient math.
const CELL_ASPECT: f32 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Solid,
    /// css angle in degrees (0 = up, 90 = right).
    Linear(f32),
    Conic,
    Radial,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stop {
    /// 0..1 along the gradient.
    pub at: f32,
    pub rgb: [u8; 3],
}

#[derive(Clone, Debug, PartialEq)]
pub struct Paint {
    pub kind: Kind,
    /// sorted by `at`, never empty.
    pub stops: Vec<Stop>,
    /// the gradient tiles: one period spans `period` name-widths.
    pub repeat: bool,
    pub period: f32,
}

impl Paint {
    fn new(kind: Kind, mut stops: Vec<Stop>, repeat: bool, period: f32) -> Option<Paint> {
        if stops.is_empty() {
            return None;
        }
        stops.sort_by(|a, b| a.at.total_cmp(&b.at));
        let kind = if stops.len() == 1 { Kind::Solid } else { kind };
        Some(Paint {
            kind,
            stops,
            repeat,
            period: if period > 0.0 { period } else { 1.0 },
        })
    }

    /// heatsync paint spec: the `fill` block's first drawable layer, else
    /// `base`. `None` for specs with nothing a terminal can show.
    pub fn from_heatsync(spec: &Value) -> Option<Paint> {
        if let Some(fill) = spec.get("fill") {
            let angle = num(fill.get("angle")).unwrap_or(0.0);
            let layers = fill.get("layers").and_then(Value::as_array);
            let layer = layers?.iter().find(|l| {
                l.get("legacy") != Some(&Value::Bool(true))
                    && l.get("kind") != Some(&Value::from("image"))
            });
            if let Some(l) = layer {
                let stops = stops_of(l.get("stops"), 100.0, hex_rgb)?;
                let kind = match l.get("kind").and_then(Value::as_str) {
                    Some("conic") => Kind::Conic,
                    Some("radial") => Kind::Radial,
                    _ => Kind::Linear(angle + num(l.get("tilt")).unwrap_or(0.0)),
                };
                let repeat = l.get("repeat") == Some(&Value::Bool(true));
                let tile = l.get("tile");
                let size = num(tile.and_then(|t| t.get("size"))).unwrap_or(1.0);
                // a px tile is measured against a ~8px cell
                let period = match tile.and_then(|t| t.get("unit")).and_then(Value::as_str) {
                    Some("px") => size / 8.0,
                    _ => size,
                };
                return Paint::new(kind, stops, repeat, period);
            }
        }
        let base = spec.get("base")?;
        let stops = stops_of(base.get("stops"), 100.0, hex_rgb)?;
        let angle = num(base.get("angle")).unwrap_or(0.0);
        let kind = match base.get("type").and_then(Value::as_str) {
            Some("solid") => Kind::Solid,
            Some("conic") => Kind::Conic,
            _ => Kind::Linear(angle),
        };
        // a banded base repeats; its tile is pixels, ~8px per cell
        let banded = base.get("type").and_then(Value::as_str) == Some("repeating-linear");
        let size = num(base.get("tileWidth")).unwrap_or(8.0);
        Paint::new(kind, stops, banded, size / 8.0)
    }

    /// 7TV paint object (`function`, `stops[{at,color}]`, `angle`, `repeat`,
    /// `shape`, solid `color`). image paints and empty ones → `None`.
    pub fn from_7tv(p: &Value) -> Option<Paint> {
        let func = p
            .get("function")
            .and_then(Value::as_str)?
            .to_ascii_lowercase();
        let rgb = |v: &Value| stv_rgb(v.as_i64()?);
        let stops = stops_of(p.get("stops"), 1.0, rgb);
        match func.replace('-', "_").as_str() {
            "linear_gradient" => Paint::new(
                Kind::Linear(num(p.get("angle")).unwrap_or(0.0)),
                stops?,
                p.get("repeat") == Some(&Value::Bool(true)),
                1.0,
            ),
            "radial_gradient" => Paint::new(
                Kind::Radial,
                stops?,
                p.get("repeat") == Some(&Value::Bool(true)),
                1.0,
            ),
            // a solid `color` paint, or any gradient-less paint that still sets one
            _ => {
                let rgb = stv_rgb(p.get("color")?.as_i64()?)?;
                Paint::new(Kind::Solid, vec![Stop { at: 0.0, rgb }], false, 1.0)
            }
        }
    }

    /// the color at `t` along the gradient (0..1 across one period when
    /// repeating; clamped otherwise).
    pub fn sample(&self, t: f32) -> [u8; 3] {
        let s = &self.stops;
        let t = if self.repeat {
            (t / self.period).rem_euclid(1.0)
        } else {
            t
        };
        if t <= s[0].at {
            return s[0].rgb;
        }
        let last = s[s.len() - 1];
        if t >= last.at {
            return last.rgb;
        }
        let i = s.windows(2).position(|w| t < w[1].at).unwrap_or(0);
        let (a, b) = (s[i], s[i + 1]);
        let f = (t - a.at) / (b.at - a.at);
        let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
        [
            mix(a.rgb[0], b.rgb[0]),
            mix(a.rgb[1], b.rgb[1]),
            mix(a.rgb[2], b.rgb[2]),
        ]
    }

    /// one color per character of an `n`-cell name. a linear gradient is
    /// projected the way css does it, over a box `n` wide and one cell tall.
    pub fn colors(&self, n: usize) -> Vec<[u8; 3]> {
        (0..n).map(|i| self.sample(self.t_at(i, n))).collect()
    }

    fn t_at(&self, i: usize, n: usize) -> f32 {
        let w = n as f32;
        let x = i as f32 + 0.5;
        match self.kind {
            Kind::Solid => 0.0,
            Kind::Linear(deg) => {
                let (sin, cos) = deg.to_radians().sin_cos();
                let len = w * sin.abs() + CELL_ASPECT * cos.abs();
                0.5 + (x - w / 2.0) * sin / len
            }
            // the conic sweep is spread along the name
            Kind::Conic => x / w,
            Kind::Radial => (x - w / 2.0).abs() / (w / 2.0),
        }
    }
}

fn num(v: Option<&Value>) -> Option<f32> {
    v?.as_f64().map(|n| n as f32)
}

/// `[{pos|at, color}]` → stops, positions divided by `scale` (100 for
/// percent, 1 for fractions). malformed stops are skipped; none left → `None`.
fn stops_of(
    v: Option<&Value>,
    scale: f32,
    color: impl Fn(&Value) -> Option<[u8; 3]>,
) -> Option<Vec<Stop>> {
    let out: Vec<Stop> = v?
        .as_array()?
        .iter()
        .filter_map(|s| {
            let at = num(s.get("pos").or_else(|| s.get("at")))? / scale;
            Some(Stop {
                at: at.clamp(0.0, 1.0),
                rgb: color(s.get("color")?)?,
            })
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

/// `#rrggbb` or `#rrggbbaa` (alpha dropped).
fn hex_rgb(v: &Value) -> Option<[u8; 3]> {
    let h = v.as_str()?.strip_prefix('#')?;
    if !(h.len() == 6 || h.len() == 8) || !h.is_ascii() {
        return None;
    }
    let b = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([b(0)?, b(2)?, b(4)?])
}

/// 7TV packs rgba into a signed int32: `(r<<24)|(g<<16)|(b<<8)|a`.
fn stv_rgb(n: i64) -> Option<[u8; 3]> {
    let v = n as i32 as u32;
    Some([(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn heatsync_base_linear() {
        let p = Paint::from_heatsync(&json!({"base": {"type": "linear", "angle": 90,
            "stops": [{"color": "#ff0000", "pos": 0}, {"color": "#0000ff", "pos": 100}]}}))
        .unwrap();
        assert_eq!(p.kind, Kind::Linear(90.0));
        assert_eq!(p.sample(0.0), [255, 0, 0]);
        assert_eq!(p.sample(1.0), [0, 0, 255]);
        assert_eq!(p.sample(0.5), [128, 0, 128]);
    }

    #[test]
    fn heatsync_solid_and_single_stop() {
        let p = Paint::from_heatsync(&json!({"base": {"type": "solid",
            "stops": [{"color": "#112233", "pos": 0}]}}))
        .unwrap();
        assert_eq!(p.kind, Kind::Solid);
        assert!(p.colors(5).iter().all(|c| *c == [0x11, 0x22, 0x33]));
    }

    #[test]
    fn heatsync_fill_wins_over_base_and_skips_image_layers() {
        let p = Paint::from_heatsync(&json!({
            "base": {"type": "solid", "stops": [{"color": "#000000", "pos": 0}]},
            "fill": {"angle": 90, "layers": [
                {"kind": "image", "src": "x"},
                {"kind": "linear", "tilt": 0, "stops": [
                    {"color": "#ff0000ff", "pos": 0}, {"color": "#00ff00", "pos": 100}]}]}}))
        .unwrap();
        assert_eq!(p.kind, Kind::Linear(90.0));
        assert_eq!(p.sample(0.0), [255, 0, 0]);
    }

    #[test]
    fn heatsync_junk_is_none() {
        assert!(Paint::from_heatsync(&json!({})).is_none());
        assert!(Paint::from_heatsync(&json!({"base": {"type": "linear", "stops": []}})).is_none());
        assert!(
            Paint::from_heatsync(&json!({"base": {"stops": [{"color": "red", "pos": 0}]}}))
                .is_none()
        );
    }

    #[test]
    fn seven_tv_gradient_decodes_packed_rgba() {
        // 0xff0000ff = opaque red; 0x0000ffff = opaque blue (signed int32 on the wire)
        let p = Paint::from_7tv(&json!({"function": "LINEAR_GRADIENT", "angle": 90, "repeat": false,
            "stops": [{"at": 0.0, "color": 0xff0000ffu32 as i32}, {"at": 1.0, "color": 0x0000ffffu32 as i32}]}))
        .unwrap();
        assert_eq!(p.sample(0.0), [255, 0, 0]);
        assert_eq!(p.sample(1.0), [0, 0, 255]);
    }

    #[test]
    fn seven_tv_solid_and_image() {
        let solid =
            Paint::from_7tv(&json!({"function": "SOLID", "color": 0x336699ffu32 as i32})).unwrap();
        assert_eq!(solid.sample(0.7), [0x33, 0x66, 0x99]);
        assert!(Paint::from_7tv(
            &json!({"function": "URL", "image_url": "https://cdn.7tv.app/paint/x/1x.webp"})
        )
        .is_none());
    }

    #[test]
    fn hard_stops_and_repeat() {
        let p = Paint::from_7tv(&json!({"function": "LINEAR_GRADIENT", "angle": 90, "repeat": true,
            "stops": [{"at": 0.0, "color": 0xff0000ffu32 as i32}, {"at": 0.5, "color": 0xff0000ffu32 as i32},
                      {"at": 0.5, "color": 0x0000ffffu32 as i32}, {"at": 1.0, "color": 0x0000ffffu32 as i32}]}))
        .unwrap();
        assert_eq!(p.sample(0.25), [255, 0, 0]);
        assert_eq!(p.sample(0.75), [0, 0, 255]);
        assert_eq!(p.sample(1.25), [255, 0, 0]); // wrapped
    }

    #[test]
    fn horizontal_gradient_runs_left_to_right_and_vertical_is_flat() {
        let stops = |angle| {
            json!({"base": {"type": "linear", "angle": angle,
            "stops": [{"color": "#000000", "pos": 0}, {"color": "#ffffff", "pos": 100}]}})
        };
        let h = Paint::from_heatsync(&stops(90)).unwrap().colors(10);
        assert!(h.windows(2).all(|w| w[0][0] < w[1][0]));
        let v = Paint::from_heatsync(&stops(0)).unwrap().colors(10);
        assert!(v.iter().all(|c| *c == v[0]));
    }

    #[test]
    fn one_cell_name_does_not_divide_by_zero() {
        let p = Paint::from_heatsync(&json!({"base": {"type": "linear", "angle": 90,
            "stops": [{"color": "#000000", "pos": 0}, {"color": "#ffffff", "pos": 100}]}}))
        .unwrap();
        assert_eq!(p.colors(1).len(), 1);
        assert!(p.colors(0).is_empty());
    }
}
