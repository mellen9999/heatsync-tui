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
    // modifier / overlay grammar, for any demo set that carries these names
    "Kappa RainTime w!",
    "KEKW ffzW ffzRainbow",
    "w!h!ffzX KEKW",
    "GAMBA notL ffzSpin",
    "c!#0f0 PogU ffzBounce",
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
                let (kind, what, text) = match (self.rng.unit() * 8.0) as u32 {
                    0 => (
                        crate::NoteKind::Sub,
                        "subscribed at Tier 1. They've subscribed for 14 months!",
                        "still here Pog",
                    ),
                    1 => (crate::NoteKind::Gift, "is gifting 5 Tier 1 Subs!", ""),
                    2 => (crate::NoteKind::Raid, "12 raiders have joined!", ""),
                    3 => (crate::NoteKind::Redeem, "redeemed Hydrate (500)", ""),
                    4 => (crate::NoteKind::Live, "went live — Just Chatting", ""),
                    5 => (crate::NoteKind::Category, "switched to Elden Ring", ""),
                    6 => (crate::NoteKind::Offline, "went offline", ""),
                    _ => (crate::NoteKind::Cheer, "cheered 100 bits", "gg"),
                };
                ch.record(
                    Message {
                        platform: ch.platform,
                        user: match kind {
                            crate::NoteKind::Raid => String::new(),
                            // stream rows name the streamer, like the live feed's
                            crate::NoteKind::Live
                            | crate::NoteKind::Category
                            | crate::NoteKind::Offline => ch.name.to_lowercase(),
                            _ => user,
                        },
                        text: text.to_string(),
                        color: None,
                        badges: Vec::new(),
                        id: None,
                        gone: None,
                        reply_to: None,
                        note: Some(crate::Note {
                            kind,
                            what: what.to_string(),
                        }),
                        extra: Default::default(),
                        emotes: None,
                        tags: Vec::new(),
                        heat: 0.0,
                        uid: None,
                    },
                    now,
                );
                if matches!(
                    kind,
                    crate::NoteKind::Live | crate::NoteKind::Category | crate::NoteKind::Offline
                ) {
                    ch.flag_event();
                }
            }
            while budget > 0.0 {
                if self.rng.unit() < budget.min(1.0) {
                    let user = self.rng.pick(USERS).to_string();
                    let text = self.rng.pick(WORDS).to_string();
                    // sprinkle role badges so the offline feed exercises the
                    // same chrome the live one does.
                    let badges = match self.rng.unit() {
                        r if r < 0.03 => vec![crate::Badge::new(crate::BadgeKind::Moderator)],
                        r if r < 0.05 => vec![crate::Badge::new(crate::BadgeKind::Vip)],
                        r if r < 0.12 => vec![crate::Badge::new(crate::BadgeKind::Subscriber)],
                        _ => Vec::new(),
                    };
                    // a few replies, /me lines and first messages so the demo
                    // shows the richer chrome too.
                    let roll = self.rng.unit();
                    let reply_to = (roll < 0.04).then(|| self.rng.pick(USERS).to_string());
                    let extra = crate::Extra {
                        reply_text: reply_to
                            .as_ref()
                            .map(|_| "wait is that actually true".to_string())
                            .unwrap_or_default(),
                        action: (0.04..0.06).contains(&roll),
                        first: (0.06..0.07).contains(&roll),
                        ..Default::default()
                    };
                    ch.record(
                        Message {
                            platform: ch.platform,
                            user,
                            text,
                            color: None,
                            badges,
                            id: None,
                            gone: None,
                            reply_to,
                            note: None,
                            extra,
                            emotes: None,
                            tags: Vec::new(),
                            heat: 0.0,
                            uid: None,
                        },
                        now,
                    );
                }
                budget -= 1.0;
            }
        }
    }
}
