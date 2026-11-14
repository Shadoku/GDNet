//! The tick clock: a fixed tick rate given by the caller (Godot's physics ticks per second), the local
//! tick counter, and the estimate of the server's tick from `Time` ping/pong samples
//! (docs/PLAN_NETCODE.md 2.2, 2.4).
//!
//! Time is passed in by the caller as microseconds since an arbitrary epoch (`now_us`); the clock never
//! reads a system clock, so it is deterministic in tests.

/// Weight of a new offset sample in the moving average (after the first, which is taken as is).
pub const OFFSET_ALPHA: f64 = 0.1;

/// A fixed-rate tick counter plus the estimated offset to the server's counter.
#[derive(Clone, Debug)]
pub struct TickClock {
    tick_rate: u32,
    tick: u64,
    /// Estimated `server tick - local tick`, in ticks (fractional). `None` until the first sample.
    offset: Option<f64>,
    samples: u32,
}

impl TickClock {
    /// A clock at `tick_rate` ticks per second (clamped to at least 1), starting at tick 0.
    pub fn new(tick_rate: u32) -> Self {
        Self {
            tick_rate: tick_rate.max(1),
            tick: 0,
            offset: None,
            samples: 0,
        }
    }

    pub fn tick_rate(&self) -> u32 {
        self.tick_rate
    }

    /// The length of one tick in microseconds.
    pub fn tick_us(&self) -> f64 {
        1_000_000.0 / f64::from(self.tick_rate)
    }

    /// The local tick counter.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Sets the local tick counter (a client adopting the server's tick at `Welcome`).
    pub fn set_tick(&mut self, tick: u64) {
        self.tick = tick;
    }

    /// Advances one tick and returns the new tick.
    pub fn advance(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Feeds one `Time` sample: the server reported `server_tick` when it answered, and the round trip
    /// took `rtt_us`. The server's tick now is about `server_tick + rtt/2`; the difference to the local
    /// tick is averaged.
    pub fn observe_server_tick(&mut self, server_tick: u64, rtt_us: f64) {
        let server_now = server_tick as f64 + (rtt_us.max(0.0) / 2.0) / self.tick_us();
        let sample = server_now - self.tick as f64;
        self.offset = Some(match self.offset {
            None => sample,
            Some(o) => o + OFFSET_ALPHA * (sample - o),
        });
        self.samples += 1;
    }

    /// Estimated `server tick - local tick` (0 before any sample).
    pub fn offset(&self) -> f64 {
        self.offset.unwrap_or(0.0)
    }

    /// Number of `Time` samples seen.
    pub fn samples(&self) -> u32 {
        self.samples
    }

    /// The estimated current server tick (the local tick before any sample).
    pub fn server_tick_estimate(&self) -> f64 {
        self.tick as f64 + self.offset()
    }
}

/// Round-trip time and jitter of one peer, RFC 6298 style smoothing: `srtt` the smoothed RTT, `rttvar`
/// its mean deviation, which is the jitter estimate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RttEstimator {
    srtt_us: Option<f64>,
    rttvar_us: f64,
    samples: u32,
    /// WP19: the last [`RTT_WINDOW`] samples (a ring), for the lower envelope.
    recent: [f64; RTT_WINDOW],
}

/// WP19: samples the lower envelope of the round trip is taken over (4 s at the default 4 pings/s).
pub const RTT_WINDOW: usize = 16;

impl RttEstimator {
    pub fn sample(&mut self, rtt_us: f64) {
        let r = rtt_us.max(0.0);
        match self.srtt_us {
            None => {
                self.srtt_us = Some(r);
                self.rttvar_us = r / 2.0;
            }
            Some(s) => {
                self.rttvar_us = 0.75 * self.rttvar_us + 0.25 * (s - r).abs();
                self.srtt_us = Some(0.875 * s + 0.125 * r);
            }
        }
        self.recent[self.samples as usize % RTT_WINDOW] = r;
        self.samples += 1;
    }

    /// WP19: the smallest of the last [`RTT_WINDOW`] samples in milliseconds (the round trip without
    /// the queueing and frame waits that delayed the others; body prediction pairs it with the leading
    /// edge of the snapshot arrivals), `None` before the first sample.
    pub fn min_rtt_ms(&self) -> Option<f64> {
        let n = (self.samples as usize).min(RTT_WINDOW);
        (n > 0).then(|| {
            self.recent[..n]
                .iter()
                .copied()
                .fold(f64::INFINITY, f64::min)
                / 1000.0
        })
    }

    /// Smoothed RTT in milliseconds, `None` before the first sample.
    pub fn rtt_ms(&self) -> Option<f64> {
        self.srtt_us.map(|s| s / 1000.0)
    }

    /// Jitter (mean RTT deviation) in milliseconds, `None` before the first sample.
    pub fn jitter_ms(&self) -> Option<f64> {
        self.srtt_us.map(|_| self.rttvar_us / 1000.0)
    }

    pub fn samples(&self) -> u32 {
        self.samples
    }
}

/// WP19: the round trips in steps, the last [`RTT_WINDOW`] of them (`Session::min_rtt_steps`).
#[derive(Clone, Copy, Debug, Default)]
pub struct StepRtt {
    recent: [u64; RTT_WINDOW],
    samples: u64,
}

impl StepRtt {
    pub fn sample(&mut self, steps: u64) {
        self.recent[self.samples as usize % RTT_WINDOW] = steps;
        self.samples += 1;
    }

    /// The smallest of the last [`RTT_WINDOW`] samples, `None` before the first.
    pub fn min(&self) -> Option<u64> {
        let n = (self.samples as usize).min(RTT_WINDOW);
        self.recent[..n].iter().copied().min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_and_set() {
        let mut c = TickClock::new(60);
        assert_eq!(c.tick(), 0);
        assert_eq!(c.advance(), 1);
        c.set_tick(100);
        assert_eq!(c.advance(), 101);
        assert!((c.tick_us() - 16_666.666).abs() < 0.01);
        assert_eq!(TickClock::new(0).tick_rate(), 1);
    }

    /// The client starts 500 ticks behind the server; one sample with a 100 ms round trip lands on the
    /// true offset (the server answered half an RTT ago, 3 ticks at 60 Hz).
    #[test]
    fn first_sample_finds_the_offset() {
        let mut c = TickClock::new(60);
        c.set_tick(10);
        // Server is at 513 when the pong arrives; it stamped 510 when answering, RTT 100 ms.
        c.observe_server_tick(510, 100_000.0);
        assert!((c.offset() - 503.0).abs() < 0.01, "{}", c.offset());
        assert!((c.server_tick_estimate() - 513.0).abs() < 0.01);
    }

    /// Simulates a client whose clock runs 0.2 % fast against a 60 Hz server for 60 s, with a noisy
    /// 80 ms ± 20 ms round trip and samples at 4 Hz: the estimate tracks the true server tick within
    /// one tick throughout, after a short warm-up.
    #[test]
    fn offset_tracks_drift_and_noise() {
        let mut c = TickClock::new(60);
        let server_rate = 60.0_f64;
        let client_rate = 60.0 * 1.002;
        let start_offset = 1234.0;
        let mut rng = crate::emul::Pcg32::new(7);
        let mut worst_after_warmup = 0.0_f64;
        let mut client_ticks = 0u64;
        let dt = 1.0 / client_rate; // seconds per client tick
        for i in 1..=3600u64 {
            client_ticks += 1;
            c.advance();
            let t = client_ticks as f64 * dt; // real time
            if i % 15 == 0 {
                let rtt = 0.080 + (rng.next_f64() - 0.5) * 0.040;
                // The server answered at t - rtt/2.
                let server_tick_then =
                    ((t - rtt / 2.0) * server_rate + start_offset).floor() as u64;
                c.observe_server_tick(server_tick_then, rtt * 1e6);
            }
            let truth = t * server_rate + start_offset;
            let err = (c.server_tick_estimate() - truth).abs();
            if i > 300 {
                worst_after_warmup = worst_after_warmup.max(err);
            }
        }
        assert!(
            worst_after_warmup < 1.0,
            "worst error {worst_after_warmup} ticks"
        );
        assert_eq!(c.samples(), 240);
    }

    #[test]
    fn rtt_and_jitter() {
        let mut r = RttEstimator::default();
        assert_eq!(r.rtt_ms(), None);
        r.sample(100_000.0);
        assert_eq!(r.rtt_ms(), Some(100.0));
        assert_eq!(r.jitter_ms(), Some(50.0));
        for _ in 0..200 {
            r.sample(100_000.0);
        }
        assert!((r.rtt_ms().unwrap() - 100.0).abs() < 0.01);
        assert!(r.jitter_ms().unwrap() < 0.01);
        // Alternating 80/120 ms: smoothed near 100, jitter near 20.
        for i in 0..400 {
            r.sample(if i % 2 == 0 { 80_000.0 } else { 120_000.0 });
        }
        assert!((r.rtt_ms().unwrap() - 100.0).abs() < 5.0);
        let j = r.jitter_ms().unwrap();
        assert!((15.0..=25.0).contains(&j), "{j}");
    }

    /// WP19: the lower envelope of the last samples.
    #[test]
    fn min_rtt_is_the_smallest_recent_sample() {
        let mut r = RttEstimator::default();
        assert_eq!(r.min_rtt_ms(), None);
        r.sample(120_000.0);
        r.sample(100_000.0);
        r.sample(116_000.0);
        assert_eq!(r.min_rtt_ms(), Some(100.0));
        // A small sample ages out after RTT_WINDOW newer ones.
        for _ in 0..RTT_WINDOW {
            r.sample(110_000.0);
        }
        assert_eq!(r.min_rtt_ms(), Some(110.0));
    }

    /// WP19: the round trip in steps, the same envelope.
    #[test]
    fn min_rtt_in_steps_is_the_smallest_recent_sample() {
        let mut r = StepRtt::default();
        assert_eq!(r.min(), None);
        for s in [8, 6, 7] {
            r.sample(s);
        }
        assert_eq!(r.min(), Some(6));
        for _ in 0..RTT_WINDOW {
            r.sample(7);
        }
        assert_eq!(r.min(), Some(7));
    }
}
