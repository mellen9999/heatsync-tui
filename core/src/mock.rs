//! deterministic mock feed — proves the aesthetic before the relay WS exists.
//! no rand crate, no real clock: a seeded xorshift + a synthetic ms clock
//! advanced each tick. heat emerges from the same decaying-counter model the
//! live feed uses, so the mock and the real thing behave identically.

use crate::{Channel, Message, Platform};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

const USERS: &[&str] = &[
    "forsen",
    "nymn",
    "pajlada",
    "supibot",
    "mel",
    "kkona",
    "wide",
    "peeposit",
    "gigachad",
    "monkas",
    "widepeepo",
    "hasanabi",
    "vedal",
    "neuro",
    "anon",
];

const WORDS: &[&str] = &[
    "GAMBA",
    "OMEGALUL",
    "Sadge",
    "PepeLaugh",
    "gigachad moment",
    "actual clip",
    "no way",
    "chat is this real",
    "COPIUM",
    "based",
    "ratio",
    "Nerd",
    "KEKW",
    "this is peak",
    "mods asleep",
    "PogU",
    "sptvJAM",
    "Aware",
];

/// per-channel baseline messages/tick — spreads channels across the ramp.
struct Seed {
    name: &'static str,
    platform: Platform,
    activity: f64,
}

const SEEDS: &[Seed] = &[
    Seed {
        name: "xqc",
        platform: Platform::Twitch,
        activity: 7.0,
    },
    Seed {
        name: "forsen",
        platform: Platform::Twitch,
        activity: 2.5,
    },
    Seed {
        name: "hasanabi",
        platform: Platform::Twitch,
        activity: 1.0,
    },
    Seed {
        name: "trainwreckstv",
        platform: Platform::Kick,
        activity: 11.0,
    },
    Seed {
        name: "adin",
        platform: Platform::Kick,
        activity: 0.4,
    },
];

/// the demo channel set — same shape the live feed produces.
pub fn channels() -> Vec<Channel> {
    SEEDS
        .iter()
        .map(|s| Channel::new(s.name, s.platform, 200))
        .collect()
}

/// drives the demo channels with a synthetic clock — no real time, no rand crate.
pub struct Driver {
    rng: Rng,
    clock_ms: u64,
    activity: Vec<f64>,
}

impl Default for Driver {
    fn default() -> Driver {
        Driver::new()
    }
}

impl Driver {
    pub fn new() -> Driver {
        Driver {
            rng: Rng(0x9E3779B97F4A7C15),
            clock_ms: 1,
            activity: SEEDS.iter().map(|s| s.activity).collect(),
        }
    }

    /// one 350ms tick: advance the clock, cool every channel, then emit a
    /// poisson-ish burst per channel scaled by its activity (rare 6× spikes).
    pub fn tick(&mut self, channels: &mut [Channel]) {
        self.clock_ms += 350;
        let now = self.clock_ms;
        for (i, ch) in channels.iter_mut().enumerate() {
            ch.cool(now);
            let spike = self.rng.unit() < 0.03;
            let mult = if spike { 6.0 } else { 1.0 };
            let mut budget = self.activity.get(i).copied().unwrap_or(1.0) * mult;
            // rare inline events so the offline feed exercises note rendering.
            if self.rng.unit() < 0.008 {
                let user = self.rng.pick(USERS).to_string();
                let (kind, what, text) = match (self.rng.unit() * 5.0) as u32 {
                    0 => (
                        crate::NoteKind::Sub,
                        "subscribed at Tier 1. They've subscribed for 14 months!",
                        "still here Pog",
                    ),
                    1 => (crate::NoteKind::Gift, "is gifting 5 Tier 1 Subs!", ""),
                    2 => (crate::NoteKind::Raid, "12 raiders have joined!", ""),
                    3 => (crate::NoteKind::Redeem, "redeemed Hydrate (500)", ""),
                    _ => (crate::NoteKind::Cheer, "cheered 100 bits", "gg"),
                };
                ch.record(
                    Message {
                        platform: ch.platform,
                        user: if matches!(kind, crate::NoteKind::Raid) {
                            String::new()
                        } else {
                            user
                        },
                        text: text.to_string(),
                        color: None,
                        badges: Vec::new(),
                        reply_to: None,
                        note: Some(crate::Note {
                            kind,
                            what: what.to_string(),
                        }),
                        heat: 0.0,
                    },
                    now,
                );
            }
            while budget > 0.0 {
                if self.rng.unit() < budget.min(1.0) {
                    let user = self.rng.pick(USERS).to_string();
                    let text = self.rng.pick(WORDS).to_string();
                    // sprinkle role badges so the offline feed exercises the
                    // same chrome the live one does.
                    let badges = match self.rng.unit() {
                        r if r < 0.03 => vec![crate::Badge::Moderator],
                        r if r < 0.05 => vec![crate::Badge::Vip],
                        r if r < 0.12 => vec![crate::Badge::Subscriber],
                        _ => Vec::new(),
                    };
                    ch.record(
                        Message {
                            platform: ch.platform,
                            user,
                            text,
                            color: None,
                            badges,
                            reply_to: None,
                            note: None,
                            heat: 0.0,
                        },
                        now,
                    );
                }
                budget -= 1.0;
            }
        }
    }
}
