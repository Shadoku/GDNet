//! The network emulator (docs/PLAN_NETCODE.md 1 "Network emulation", WP1): a pure component that takes
//! `(packet, now)` and releases packets according to latency, variance, loss, reorder and duplication,
//! deterministic from a seed. One `Emulator` models one direction; a link uses two.
//!
//! Reliable ordered traffic models what a reliable transport does: it is never lost, reordered or
//! duplicated, but every simulated loss costs a resend delay, and it never overtakes the packet before
//! it on the same channel. At `loss >= 1.0` the link is dead and reliable packets are discarded too.

/// A small PCG-XSH-RR 32-bit generator (O'Neill 2014). Enough for an emulator; not for cryptography.
#[derive(Clone, Debug)]
pub struct Pcg32 {
    state: u64,
    inc: u64,
}

impl Pcg32 {
    const MUL: u64 = 6_364_136_223_846_793_005;

    pub fn new(seed: u64) -> Self {
        let mut rng = Self {
            state: 0,
            inc: (seed << 1) | 1,
        };
        rng.next_u32();
        rng.state = rng.state.wrapping_add(seed ^ 0x853c_49e6_748f_ea9b);
        rng.next_u32();
        rng
    }

    pub fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old.wrapping_mul(Self::MUL).wrapping_add(self.inc);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }

    /// Uniform in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        let hi = u64::from(self.next_u32()) << 21;
        let lo = u64::from(self.next_u32() >> 11);
        ((hi | lo) as f64) / ((1u64 << 53) as f64)
    }

    /// True with probability `p` (clamped to [0, 1]).
    pub fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && (p >= 1.0 || self.next_f64() < p)
    }
}

/// Emulation of one direction. Probabilities are in `[0, 1]`; times in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EmulSettings {
    /// Minimum one-way delay.
    pub latency_ms: f64,
    /// Extra delay, uniform in `[0, variance_ms]`, added to `latency_ms` (Unreal's min/max lag).
    pub variance_ms: f64,
    /// Probability an unreliable packet is dropped (and a reliable one needs a resend).
    pub loss: f64,
    /// Probability an unreliable packet is held back long enough to arrive after later ones.
    pub reorder: f64,
    /// Probability an unreliable packet arrives twice.
    pub duplicate: f64,
}

impl EmulSettings {
    /// Unreal's network emulation profiles: `Average` (30-60 ms, 1 % loss) and `Bad` (100-200 ms, 5 %
    /// loss); `Off` is no emulation. WP8 adds `Lan` (5 ms) and `Mobile` (120-140 ms, 2 % loss). Names are
    /// case-insensitive.
    /// The preset names.
    pub const PRESETS: [&'static str; 5] = ["off", "lan", "average", "bad", "mobile"];

    pub fn preset(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "off" | "none" => Some(Self::default()),
            "average" => Some(Self {
                latency_ms: 30.0,
                variance_ms: 30.0,
                loss: 0.01,
                reorder: 0.0,
                duplicate: 0.0,
            }),
            "lan" => Some(Self {
                latency_ms: 5.0,
                ..Self::default()
            }),
            "mobile" => Some(Self {
                latency_ms: 120.0,
                variance_ms: 20.0,
                loss: 0.02,
                reorder: 0.0,
                duplicate: 0.0,
            }),
            "bad" => Some(Self {
                latency_ms: 100.0,
                variance_ms: 100.0,
                loss: 0.05,
                reorder: 0.0,
                duplicate: 0.0,
            }),
            _ => None,
        }
    }

    /// True when the settings change nothing (packets pass straight through).
    pub fn is_off(&self) -> bool {
        self.latency_ms <= 0.0
            && self.variance_ms <= 0.0
            && self.loss <= 0.0
            && self.reorder <= 0.0
            && self.duplicate <= 0.0
    }

    /// Clamps every field into its valid range.
    pub fn clamped(self) -> Self {
        let p = |x: f64| {
            if x.is_finite() {
                x.clamp(0.0, 1.0)
            } else {
                0.0
            }
        };
        let ms = |x: f64| if x.is_finite() { x.max(0.0) } else { 0.0 };
        Self {
            latency_ms: ms(self.latency_ms),
            variance_ms: ms(self.variance_ms),
            loss: p(self.loss),
            reorder: p(self.reorder),
            duplicate: p(self.duplicate),
        }
    }
}

/// Simulated resends are capped; the chance of reaching it at loss < 1 is negligible.
const MAX_RESENDS: u32 = 64;

/// One direction of an emulated link.
#[derive(Clone, Debug)]
pub struct Emulator<T> {
    settings: EmulSettings,
    rng: Pcg32,
    queue: Vec<(u64, u64, T)>,
    seq: u64,
    last_ordered: [u64; 256],
    dropped: u64,
}

impl<T: Clone> Emulator<T> {
    pub fn new(settings: EmulSettings, seed: u64) -> Self {
        Self {
            settings: settings.clamped(),
            rng: Pcg32::new(seed),
            queue: Vec::new(),
            seq: 0,
            last_ordered: [0; 256],
            dropped: 0,
        }
    }

    pub fn settings(&self) -> EmulSettings {
        self.settings
    }

    /// Changes the settings; packets already in flight keep their release times.
    pub fn set_settings(&mut self, settings: EmulSettings) {
        self.settings = settings.clamped();
    }

    /// Packets in flight.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Packets dropped so far (simulated loss).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    fn delay_us(&mut self) -> u64 {
        let s = self.settings;
        let ms = s.latency_ms + s.variance_ms * self.rng.next_f64();
        (ms * 1000.0) as u64
    }

    fn enqueue(&mut self, release: u64, item: T) {
        self.seq += 1;
        self.queue.push((release, self.seq, item));
    }

    /// Accepts a packet sent at `now_us`. `ordered` is the channel of a reliable ordered packet, `None`
    /// for an unreliable one.
    pub fn push(&mut self, now_us: u64, item: T, ordered: Option<u8>) {
        let s = self.settings;
        match ordered {
            Some(channel) => {
                if s.loss >= 1.0 {
                    self.dropped += 1;
                    return;
                }
                let resend_us = ((2.0 * s.latency_ms).max(30.0) * 1000.0) as u64;
                let mut release = now_us + self.delay_us();
                let mut resends = 0;
                while resends < MAX_RESENDS && self.rng.chance(s.loss) {
                    release += resend_us;
                    resends += 1;
                }
                let slot = &mut self.last_ordered[usize::from(channel)];
                release = release.max(*slot);
                *slot = release;
                self.enqueue(release, item);
            }
            None => {
                if self.rng.chance(s.loss) {
                    self.dropped += 1;
                    return;
                }
                let mut release = now_us + self.delay_us();
                if self.rng.chance(s.reorder) {
                    let hold_ms = s.latency_ms.max(20.0) * (0.5 + self.rng.next_f64());
                    release += (hold_ms * 1000.0) as u64;
                }
                if self.rng.chance(s.duplicate) {
                    let copy_release = now_us + self.delay_us();
                    self.enqueue(copy_release, item.clone());
                }
                self.enqueue(release, item);
            }
        }
    }

    /// Releases every packet due at `now_us`, earliest first (ties in send order).
    pub fn pop_ready(&mut self, now_us: u64) -> Vec<T> {
        if self.queue.is_empty() {
            return Vec::new();
        }
        let mut ready: Vec<(u64, u64, T)> = Vec::new();
        let mut i = 0;
        while i < self.queue.len() {
            if self.queue[i].0 <= now_us {
                ready.push(self.queue.swap_remove(i));
            } else {
                i += 1;
            }
        }
        ready.sort_by_key(|(release, seq, _)| (*release, *seq));
        ready.into_iter().map(|(_, _, item)| item).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(settings: EmulSettings, seed: u64, n: u32, ordered: Option<u8>) -> Vec<(u32, u64)> {
        let mut e = Emulator::new(settings, seed);
        for i in 0..n {
            e.push(u64::from(i) * 1000, i, ordered);
        }
        // Release in 1 ms steps; record (packet, arrival ms).
        let mut out = Vec::new();
        let mut now = 0u64;
        while !e.is_empty() {
            for p in e.pop_ready(now) {
                out.push((p, now / 1000));
            }
            now += 1000;
        }
        out
    }

    #[test]
    fn pcg_is_deterministic_and_uniform() {
        let mut a = Pcg32::new(42);
        let mut b = Pcg32::new(42);
        let mut c = Pcg32::new(43);
        let xs: Vec<u32> = (0..8).map(|_| a.next_u32()).collect();
        let ys: Vec<u32> = (0..8).map(|_| b.next_u32()).collect();
        let zs: Vec<u32> = (0..8).map(|_| c.next_u32()).collect();
        assert_eq!(xs, ys);
        assert_ne!(xs, zs);
        let mean: f64 = (0..10_000).map(|_| a.next_f64()).sum::<f64>() / 10_000.0;
        assert!((mean - 0.5).abs() < 0.02, "{mean}");
    }

    #[test]
    fn off_passes_through_in_order() {
        let out = run(EmulSettings::default(), 1, 100, None);
        assert_eq!(out.len(), 100);
        for (i, (p, at)) in out.iter().enumerate() {
            assert_eq!(*p, i as u32);
            assert_eq!(*at, i as u64);
        }
    }

    #[test]
    fn latency_and_variance_bound_the_delay() {
        let s = EmulSettings {
            latency_ms: 50.0,
            variance_ms: 20.0,
            ..Default::default()
        };
        let out = run(s, 9, 5000, None);
        assert_eq!(out.len(), 5000);
        let delays: Vec<u64> = out.iter().map(|(p, at)| at - u64::from(*p)).collect();
        assert!(
            delays.iter().all(|d| (50..=71).contains(d)),
            "{:?}",
            delays.iter().max()
        );
        let mean = delays.iter().sum::<u64>() as f64 / delays.len() as f64;
        assert!((mean - 60.5).abs() < 1.5, "mean {mean}");
    }

    #[test]
    fn loss_drops_about_that_share() {
        let s = EmulSettings {
            loss: 0.1,
            ..Default::default()
        };
        let out = run(s, 3, 10_000, None);
        let lost = 10_000 - out.len();
        assert!((800..=1200).contains(&lost), "{lost}");
        // Total loss drops everything, reliable included.
        let dead = EmulSettings {
            loss: 1.0,
            ..Default::default()
        };
        assert!(run(dead, 3, 100, None).is_empty());
        assert!(run(dead, 3, 100, Some(0)).is_empty());
    }

    #[test]
    fn duplicate_adds_copies() {
        let s = EmulSettings {
            duplicate: 0.2,
            ..Default::default()
        };
        let out = run(s, 5, 10_000, None);
        let extra = out.len() - 10_000;
        assert!((1700..=2300).contains(&extra), "{extra}");
    }

    #[test]
    fn reorder_inverts_some_packets() {
        let s = EmulSettings {
            latency_ms: 10.0,
            reorder: 0.1,
            ..Default::default()
        };
        let out = run(s, 11, 5000, None);
        assert_eq!(out.len(), 5000);
        let inversions = out.windows(2).filter(|w| w[1].0 < w[0].0).count();
        assert!(inversions > 200, "{inversions}");
        let none = run(
            EmulSettings {
                latency_ms: 10.0,
                ..Default::default()
            },
            11,
            5000,
            None,
        );
        assert_eq!(none.windows(2).filter(|w| w[1].0 < w[0].0).count(), 0);
    }

    #[test]
    fn reliable_ordered_is_never_lost_or_reordered() {
        let s = EmulSettings {
            latency_ms: 20.0,
            variance_ms: 40.0,
            loss: 0.3,
            reorder: 0.5,
            duplicate: 0.5,
        };
        let out = run(s, 13, 2000, Some(0));
        assert_eq!(
            out.len(),
            2000,
            "reliable packets are never lost or duplicated"
        );
        assert!(out.windows(2).all(|w| w[0].0 < w[1].0), "in order");
        // Losses cost resends: the mean delay is well above the unreliable one.
        let mean = out
            .iter()
            .map(|(p, at)| (at - u64::from(*p)) as f64)
            .sum::<f64>()
            / 2000.0;
        assert!(mean > 40.0, "{mean}");
    }

    #[test]
    fn same_seed_same_schedule() {
        let s = EmulSettings::preset("bad").unwrap();
        assert_eq!(run(s, 77, 500, None), run(s, 77, 500, None));
        assert_ne!(run(s, 77, 500, None), run(s, 78, 500, None));
    }

    #[test]
    fn presets() {
        let avg = EmulSettings::preset("Average").unwrap();
        assert_eq!(
            (avg.latency_ms, avg.variance_ms, avg.loss),
            (30.0, 30.0, 0.01)
        );
        let bad = EmulSettings::preset("BAD").unwrap();
        assert_eq!(
            (bad.latency_ms, bad.variance_ms, bad.loss),
            (100.0, 100.0, 0.05)
        );
        assert!(EmulSettings::preset("off").unwrap().is_off());
        assert!(EmulSettings::preset("dialup").is_none());
        // WP8: lan and mobile as data.
        let lan = EmulSettings::preset("lan").unwrap();
        assert_eq!((lan.latency_ms, lan.variance_ms, lan.loss), (5.0, 0.0, 0.0));
        let m = EmulSettings::preset("Mobile").unwrap();
        assert_eq!((m.latency_ms, m.variance_ms, m.loss), (120.0, 20.0, 0.02));
        for name in EmulSettings::PRESETS {
            assert!(EmulSettings::preset(name).is_some(), "{name}");
        }
        // The Bad preset over 10,000 packets: 5 % lost, delays within 100-200 ms.
        let out = run(bad, 21, 10_000, None);
        let lost = 10_000 - out.len();
        assert!((400..=600).contains(&lost), "{lost}");
        assert!(out
            .iter()
            .all(|(p, at)| (100..=201).contains(&(at - u64::from(*p)))));
    }

    #[test]
    fn clamps_bad_values() {
        let s = EmulSettings {
            latency_ms: -5.0,
            variance_ms: f64::NAN,
            loss: 2.0,
            reorder: -1.0,
            duplicate: 0.5,
        }
        .clamped();
        assert_eq!(
            s,
            EmulSettings {
                latency_ms: 0.0,
                variance_ms: 0.0,
                loss: 1.0,
                reorder: 0.0,
                duplicate: 0.5
            }
        );
    }
}
