//! WP18 (docs/PLAN_NETCODE.md 12.5, "Idle entities"): which entities the server polls in a tick.
//!
//! An entity is in one of three states:
//!
//! - **hot**: polled every tick (its cold properties every `period` ticks, staggered);
//! - **idle**: nothing it has changed for `idle_after` polls in a row; polled every `resting` ticks
//!   with its cold properties, until a poll finds a change;
//! - **asleep**: its bodies sleep; the bodies' hot properties are not polled at all, the rest is polled
//!   every `resting` ticks like an idle entity.
//!
//! `resting` (`gdnet/sync/resting_poll_ticks`) is never below `period` (`cold_poll_ticks`): a resting
//! entity is polled at most as often as a hot one's cold properties. Hot entities sit in a list of
//! their own and the resting ones in `resting` buckets by their stagger, so a tick costs the hot
//! entities plus one bucket, not every entity. Entities are addressed by the
//! slot index [`Activity::insert`] returns; `K` is the caller's key, handed back by [`Activity::due`].

/// What to poll of an entity this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
    /// Its hot properties, and the cold ones when `cold`.
    Hot { cold: bool },
    /// Everything (hot and cold).
    Idle,
    /// Its bodies' cold properties and everything of its other parts; check that the bodies still sleep.
    Asleep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Hot,
    Idle,
    Asleep,
}

#[derive(Clone, Debug)]
struct Slot<K> {
    key: K,
    live: bool,
    state: State,
    stagger: u64,
    /// Hot polls in a row that changed nothing.
    quiet: u32,
    /// The entity's bodies sleep (set by the caller from the engine).
    asleep: bool,
    /// Never idle nor asleep (predicted, or simulated from input).
    always: bool,
    /// Index in `hot` while hot.
    hot_at: usize,
    /// Bucket and index in it.
    bucket: usize,
    bucket_at: usize,
}

/// The hot list and the resting buckets.
#[derive(Clone, Debug)]
pub struct Activity<K> {
    slots: Vec<Slot<K>>,
    free: Vec<u32>,
    /// Hot entities: (slot, stagger).
    hot: Vec<(u32, u64)>,
    /// Every live entity, by the tick of the resting period it is due on.
    buckets: Vec<Vec<u32>>,
    /// The cold rate of hot entities.
    period: u64,
    /// The resting rate as asked for (1 = the same as `period`); the effective one (`buckets.len()`)
    /// is at least `period`.
    resting_asked: u64,
    idle_after: u32,
    idle: usize,
    asleep: usize,
}

impl<K: Copy> Default for Activity<K> {
    fn default() -> Self {
        Self::new(6, 30)
    }
}

impl<K: Copy> Activity<K> {
    /// `period`: the cold rate in ticks (at least 1), also the resting rate until
    /// [`Activity::set_resting`] asks for a slower one; `idle_after`: polls without a change before an entity goes idle
    /// (0: never).
    pub fn new(period: u32, idle_after: u32) -> Self {
        let period = u64::from(period.max(1));
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            hot: Vec::new(),
            buckets: vec![Vec::new(); period as usize],
            period,
            resting_asked: 1,
            idle_after,
            idle: 0,
            asleep: 0,
        }
    }

    /// The effective resting rate: how many ticks apart a resting entity is polled.
    pub fn resting(&self) -> u64 {
        self.buckets.len() as u64
    }

    fn bucket_of(&self, stagger: u64) -> usize {
        let n = self.resting();
        ((n - stagger % n) % n) as usize
    }

    /// Adds an entity, hot (its first poll records its initial state). `stagger` spreads the
    /// cold polls: a hot entity's cold properties are due on ticks where `(tick + stagger) % period ==
    /// 0`, a resting entity on ticks where `(tick + stagger) % resting == 0`.
    pub fn insert(&mut self, key: K, stagger: u64) -> u32 {
        let bucket = self.bucket_of(stagger);
        let slot = Slot {
            key,
            live: true,
            state: State::Hot,
            stagger,
            quiet: 0,
            asleep: false,
            always: false,
            hot_at: self.hot.len(),
            bucket,
            bucket_at: self.buckets[bucket].len(),
        };
        let i = match self.free.pop() {
            Some(i) => {
                self.slots[i as usize] = slot;
                i
            }
            None => {
                self.slots.push(slot);
                (self.slots.len() - 1) as u32
            }
        };
        self.hot.push((i, stagger));
        self.buckets[bucket].push(i);
        i
    }

    fn get(&self, i: u32) -> Option<&Slot<K>> {
        self.slots.get(i as usize).filter(|s| s.live)
    }

    /// Removes an entity (a stale or unknown index is ignored).
    pub fn remove(&mut self, i: u32) {
        let Some(s) = self.get(i) else {
            return;
        };
        let (bucket, at) = (s.bucket, s.bucket_at);
        self.set_state(i, State::Hot);
        self.unhot(i);
        self.buckets[bucket].swap_remove(at);
        if let Some(&moved) = self.buckets[bucket].get(at) {
            self.slots[moved as usize].bucket_at = at;
        }
        self.slots[i as usize].live = false;
        self.free.push(i);
    }

    /// Takes a hot entity out of the hot list (its state is set by the caller).
    fn unhot(&mut self, i: u32) {
        let at = self.slots[i as usize].hot_at;
        if self.hot.get(at).map(|h| h.0) != Some(i) {
            return;
        }
        self.hot.swap_remove(at);
        if let Some(&(moved, _)) = self.hot.get(at) {
            self.slots[moved as usize].hot_at = at;
        }
    }

    fn set_state(&mut self, i: u32, to: State) {
        let from = self.slots[i as usize].state;
        if from == to {
            return;
        }
        match from {
            State::Idle => self.idle -= 1,
            State::Asleep => self.asleep -= 1,
            State::Hot => self.unhot(i),
        }
        match to {
            State::Idle => self.idle += 1,
            State::Asleep => self.asleep += 1,
            State::Hot => {
                let stagger = self.slots[i as usize].stagger;
                self.slots[i as usize].hot_at = self.hot.len();
                self.hot.push((i, stagger));
            }
        }
        self.slots[i as usize].state = to;
    }

    /// Makes an entity hot now: it is polled in full at the next [`Activity::due`].
    pub fn wake(&mut self, i: u32) {
        if self.get(i).is_none() {
            return;
        }
        self.slots[i as usize].quiet = 0;
        self.set_state(i, State::Hot);
    }

    /// The entity's bodies fell asleep (`true`) or woke (`false`). Falling asleep takes effect after
    /// the next hot poll that changes nothing; waking makes it hot at once.
    pub fn set_asleep(&mut self, i: u32, asleep: bool) {
        let Some(s) = self.get(i) else {
            return;
        };
        let state = s.state;
        self.slots[i as usize].asleep = asleep;
        if !asleep {
            self.wake(i);
        } else if state == State::Idle && !self.slots[i as usize].always {
            self.set_state(i, State::Asleep);
        }
    }

    /// An entity that is never idle nor asleep while `always` (it is hot at once).
    pub fn set_always(&mut self, i: u32, always: bool) {
        if self.get(i).is_none() {
            return;
        }
        self.slots[i as usize].always = always;
        if always {
            self.wake(i);
        }
    }

    pub fn is_always(&self, i: u32) -> bool {
        self.get(i).is_some_and(|s| s.always)
    }

    pub fn state(&self, i: u32) -> Option<State> {
        self.get(i).map(|s| s.state)
    }

    /// The entities to poll at `tick`, with what to poll of each, appended to `out`.
    pub fn due(&self, tick: u64, out: &mut Vec<(u32, K, Poll)>) {
        for &(i, stagger) in &self.hot {
            let cold = (tick + stagger).is_multiple_of(self.period);
            out.push((i, self.slots[i as usize].key, Poll::Hot { cold }));
        }
        let bucket = &self.buckets[(tick % self.resting()) as usize];
        for &i in bucket {
            let s = &self.slots[i as usize];
            match s.state {
                State::Hot => {}
                State::Idle => out.push((i, s.key, Poll::Idle)),
                State::Asleep => out.push((i, s.key, Poll::Asleep)),
            }
        }
    }

    /// What a poll found: `changed` when any value it read differed from the last one recorded.
    pub fn observe(&mut self, i: u32, changed: bool) {
        let Some(s) = self.get(i) else {
            return;
        };
        let s = s.clone();
        if s.state != State::Hot {
            if changed {
                self.wake(i);
            }
            return;
        }
        let quiet = if changed {
            0
        } else {
            s.quiet.saturating_add(1)
        };
        self.slots[i as usize].quiet = quiet;
        if s.always {
            return;
        }
        if s.asleep && quiet >= 1 {
            self.set_state(i, State::Asleep);
        } else if self.idle_after > 0 && quiet >= self.idle_after {
            self.set_state(i, State::Idle);
        }
    }

    /// Changes the cold rate of hot entities (the resting rate is raised to it when below).
    pub fn set_period(&mut self, period: u32) {
        let period = u64::from(period.max(1));
        if period == self.period {
            return;
        }
        self.period = period;
        for h in &mut self.hot {
            h.1 = self.slots[h.0 as usize].stagger;
        }
        self.rebucket();
    }

    /// Changes the resting rate: idle and sleeping entities are polled every `resting` ticks, or every
    /// `period` ticks when that is more (the buckets are rebuilt when the effective rate differs).
    pub fn set_resting(&mut self, resting: u32) {
        self.resting_asked = u64::from(resting.max(1));
        self.rebucket();
    }

    /// Sets both rates (see [`Activity::set_period`] and [`Activity::set_resting`]).
    pub fn set_rates(&mut self, period: u32, resting: u32) {
        self.resting_asked = u64::from(resting.max(1));
        self.set_period(period);
        self.rebucket();
    }

    /// Rebuilds the buckets when the effective resting rate changed.
    fn rebucket(&mut self) {
        let n = self.resting_asked.max(self.period);
        if n == self.resting() {
            return;
        }
        self.buckets = vec![Vec::new(); n as usize];
        for i in 0..self.slots.len() {
            if !self.slots[i].live {
                continue;
            }
            let b = self.bucket_of(self.slots[i].stagger);
            self.slots[i].bucket = b;
            self.slots[i].bucket_at = self.buckets[b].len();
            self.buckets[b].push(i as u32);
        }
    }

    /// Changes `idle_after`; 0 wakes every idle entity (the idle rule is off).
    pub fn set_idle_after(&mut self, n: u32) {
        if n == self.idle_after {
            return;
        }
        self.idle_after = n;
        if n == 0 {
            for i in 0..self.slots.len() as u32 {
                if self.slots[i as usize].live && self.slots[i as usize].state == State::Idle {
                    self.wake(i);
                }
            }
        }
    }

    /// (hot, idle, asleep) entity counts.
    pub fn counts(&self) -> (usize, usize, usize) {
        (self.hot.len(), self.idle, self.asleep)
    }

    pub fn len(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn due(a: &Activity<u32>, tick: u64) -> Vec<(u32, Poll)> {
        let mut out = Vec::new();
        a.due(tick, &mut out);
        let mut v: Vec<(u32, Poll)> = out.into_iter().map(|(_, k, p)| (k, p)).collect();
        v.sort_by_key(|x| x.0);
        v
    }

    /// Polls every due entity of `tick`, reporting a change for the keys in `changed`.
    fn tick(a: &mut Activity<u32>, t: u64, changed: &[u32]) -> Vec<(u32, Poll)> {
        let mut out = Vec::new();
        a.due(t, &mut out);
        for &(i, k, _) in &out {
            a.observe(i, changed.contains(&k));
        }
        let mut v: Vec<(u32, Poll)> = out.into_iter().map(|(_, k, p)| (k, p)).collect();
        v.sort_by_key(|x| x.0);
        v
    }

    #[test]
    fn a_new_entity_is_hot_and_its_cold_properties_are_staggered() {
        let mut a = Activity::new(6, 30);
        a.insert(10, 1);
        a.insert(11, 2);
        assert_eq!(a.counts(), (2, 0, 0));
        assert_eq!(
            due(&a, 5),
            vec![
                (10, Poll::Hot { cold: true }),
                (11, Poll::Hot { cold: false })
            ]
        );
        assert_eq!(
            due(&a, 4),
            vec![
                (10, Poll::Hot { cold: false }),
                (11, Poll::Hot { cold: true })
            ]
        );
    }

    #[test]
    fn an_unchanging_entity_goes_idle_after_idle_after_polls_and_is_polled_at_the_cold_rate() {
        let mut a = Activity::new(6, 30);
        a.insert(7, 7);
        tick(&mut a, 0, &[7]);
        for t in 1..30 {
            tick(&mut a, t, &[]);
            assert_eq!(a.counts(), (1, 0, 0), "still hot at tick {t}");
        }
        tick(&mut a, 30, &[]);
        assert_eq!(a.counts(), (0, 1, 0), "idle after 30 quiet polls");
        let polled: Vec<u64> = (31..61).filter(|&t| !due(&a, t).is_empty()).collect();
        assert_eq!(
            polled,
            vec![35, 41, 47, 53, 59],
            "every 6 ticks, where (t + 7) % 6 == 0"
        );
        assert_eq!(due(&a, 35), vec![(7, Poll::Idle)]);
    }

    #[test]
    fn a_change_found_by_a_resting_poll_makes_it_hot_again() {
        let mut a = Activity::new(6, 2);
        a.insert(1, 0);
        for t in 0..3 {
            tick(&mut a, t, &[]);
        }
        assert_eq!(a.counts(), (0, 1, 0));
        tick(&mut a, 6, &[1]);
        assert_eq!(a.counts(), (1, 0, 0));
        assert_eq!(due(&a, 7), vec![(1, Poll::Hot { cold: false })]);
    }

    #[test]
    fn a_sleeping_entity_leaves_the_hot_list_after_its_next_quiet_poll() {
        let mut a = Activity::new(6, 30);
        let i = a.insert(3, 3);
        a.set_asleep(i, true);
        tick(&mut a, 0, &[3]);
        assert_eq!(
            a.counts(),
            (1, 0, 0),
            "its first poll (the initial state) changed"
        );
        tick(&mut a, 1, &[]);
        assert_eq!(a.counts(), (0, 0, 1));
        assert!(due(&a, 2).is_empty());
        assert_eq!(due(&a, 3), vec![(3, Poll::Asleep)]);
        a.set_asleep(i, false);
        assert_eq!(
            due(&a, 4),
            vec![(3, Poll::Hot { cold: false })],
            "woken: hot at once"
        );
    }

    #[test]
    fn wake_and_always() {
        let mut a = Activity::new(6, 1);
        let i = a.insert(1, 0);
        let j = a.insert(2, 0);
        a.set_always(j, true);
        for t in 0..5 {
            tick(&mut a, t, &[]);
        }
        assert_eq!(a.state(i), Some(State::Idle));
        assert_eq!(a.state(j), Some(State::Hot), "always active never idles");
        a.set_asleep(j, true);
        tick(&mut a, 5, &[]);
        assert_eq!(a.state(j), Some(State::Hot), "nor sleeps");
        a.wake(i);
        assert_eq!(a.state(i), Some(State::Hot));
        assert_eq!(a.counts(), (2, 0, 0));
    }

    #[test]
    fn idle_after_zero_turns_the_rule_off_and_wakes_the_idle() {
        let mut a = Activity::new(6, 1);
        let i = a.insert(1, 0);
        tick(&mut a, 0, &[]);
        assert_eq!(a.state(i), Some(State::Idle));
        a.set_idle_after(0);
        assert_eq!(a.state(i), Some(State::Hot));
        for t in 1..100 {
            tick(&mut a, t, &[]);
        }
        assert_eq!(a.state(i), Some(State::Hot));
    }

    #[test]
    fn removal_and_reuse_keep_the_lists_consistent() {
        let mut a = Activity::new(3, 1);
        let ids: Vec<u32> = (0..10).map(|k| a.insert(k, u64::from(k))).collect();
        tick(&mut a, 0, &[]);
        tick(&mut a, 1, &[]);
        a.wake(ids[4]);
        a.remove(ids[4]);
        a.remove(ids[0]);
        a.remove(ids[0]);
        assert_eq!(a.len(), 8);
        let n = a.insert(99, 0);
        assert!(n == ids[0] || n == ids[4], "a freed slot is reused");
        assert_eq!(a.counts(), (1, 8, 0));
        let mut seen = Vec::new();
        for t in 2..5 {
            seen.extend(due(&a, t).into_iter().map(|x| x.0));
        }
        seen.sort();
        seen.dedup();
        assert_eq!(
            seen,
            vec![1, 2, 3, 5, 6, 7, 8, 9, 99],
            "every live entity is due once a period"
        );
    }

    #[test]
    fn a_new_period_rebuilds_the_buckets() {
        let mut a = Activity::new(6, 1);
        for k in 0..12 {
            a.insert(k, u64::from(k));
        }
        tick(&mut a, 0, &[]);
        tick(&mut a, 1, &[]);
        assert_eq!(a.counts(), (0, 12, 0));
        a.set_period(4);
        for t in 0..4u64 {
            let d = due(&a, t);
            assert_eq!(d.len(), 3);
            assert!(d.iter().all(|(k, _)| (t + u64::from(*k)) % 4 == 0));
        }
    }

    #[test]
    fn resting_entities_are_polled_at_the_resting_rate_and_hot_ones_cold_at_the_cold_rate() {
        let mut a = Activity::new(6, 1);
        a.set_resting(24);
        assert_eq!(a.resting(), 24);
        let idle = a.insert(1, 5);
        let hot = a.insert(2, 5);
        a.set_always(hot, true);
        tick(&mut a, 0, &[]);
        assert_eq!(a.state(idle), Some(State::Idle));
        let resting: Vec<u64> = (1..100)
            .filter(|&t| due(&a, t).iter().any(|x| x.0 == 1))
            .collect();
        assert_eq!(
            resting,
            vec![19, 43, 67, 91],
            "every 24 ticks, where (t + 5) % 24 == 0"
        );
        assert_eq!(
            due(&a, 19),
            vec![(1, Poll::Idle), (2, Poll::Hot { cold: true })]
        );
        let cold: Vec<u64> = (1..40)
            .filter(|&t| due(&a, t).contains(&(2, Poll::Hot { cold: true })))
            .collect();
        assert_eq!(
            cold,
            vec![1, 7, 13, 19, 25, 31, 37],
            "the hot one's cold properties every 6"
        );
        let i = a.insert(3, 0);
        a.set_asleep(i, true);
        tick(&mut a, 1, &[3]);
        tick(&mut a, 2, &[]);
        assert_eq!(a.state(i), Some(State::Asleep));
        let asleep: Vec<u64> = (3..60)
            .filter(|&t| due(&a, t).iter().any(|x| x.0 == 3))
            .collect();
        assert_eq!(asleep, vec![24, 48], "a sleeping entity too");
    }

    #[test]
    fn the_resting_rate_is_never_below_the_cold_rate_and_follows_changes() {
        let mut a = Activity::new(6, 1);
        assert_eq!(a.resting(), 6, "the cold rate until asked for another");
        a.set_resting(3);
        assert_eq!(a.resting(), 6, "raised to the cold rate");
        a.set_resting(24);
        a.set_period(30);
        assert_eq!(a.resting(), 30, "a slower cold rate raises it");
        a.set_period(6);
        assert_eq!(a.resting(), 24, "and back");
        a.set_rates(12, 8);
        assert_eq!(a.resting(), 12);
        for k in 0..30u32 {
            a.insert(k, u64::from(k));
        }
        tick(&mut a, 0, &[]);
        tick(&mut a, 1, &[]);
        assert_eq!(a.counts(), (0, 30, 0));
        for n in [24u32, 7, 6, 1] {
            a.set_rates(6, n);
            let r = a.resting();
            assert_eq!(r, u64::from(n.max(6)));
            let mut seen = Vec::new();
            for t in 100..100 + r {
                for (k, p) in due(&a, t) {
                    assert_eq!(p, Poll::Idle);
                    assert_eq!(
                        (t + u64::from(k)) % r,
                        0,
                        "staggered by id over {r} buckets"
                    );
                    seen.push(k);
                }
            }
            seen.sort();
            assert_eq!(
                seen,
                (0..30).collect::<Vec<u32>>(),
                "each once a period at {r}"
            );
        }
        a.wake(5);
        assert_eq!(
            a.counts(),
            (1, 29, 0),
            "a wake is immediate whatever the rate"
        );
        assert_eq!(due(&a, 101).iter().filter(|x| x.0 == 5).count(), 1);
    }

    #[test]
    fn five_thousand_resting_entities_cost_one_bucket_a_tick() {
        let mut a = Activity::new(6, 30);
        for k in 0..5000u32 {
            let i = a.insert(k, u64::from(k));
            a.set_asleep(i, true);
        }
        for t in 0..2 {
            tick(&mut a, t, &[]);
        }
        assert_eq!(a.counts(), (0, 0, 5000));
        let mut out = Vec::new();
        a.due(2, &mut out);
        assert!(out.len() <= 834, "one bucket: {}", out.len());
        a.set_resting(24);
        out.clear();
        a.due(2, &mut out);
        assert!(out.len() <= 209, "one of 24 buckets: {}", out.len());
    }
}
