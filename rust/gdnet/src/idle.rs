//! WP18 (docs/PLAN_NETCODE.md 12.5, "Idle entities"): what the server polls each tick. A child module
//! of `manager` (it reads the manager's entries).
//!
//! Every spawned entity on the server has a slot in the manager's [`netcore::Activity`]: hot (polled
//! every tick), idle (nothing changed for `gdnet/sync/idle_after_ticks` polls: polled every
//! `cold_poll_ticks` with its cold properties) or asleep (its RigidBody parts sleep: their hot
//! properties are not polled at all, the rest is polled like an idle entity's). A body's
//! `sleeping_state_changed` signal moves it; `Net.touch`, a call, an owner change, a reparent, travel and
//! restored level state make an entity hot at once. Entities whose scripts read `Net.input` (players,
//! including an away seat's body the server simulates with neutral input) and entities a client owns
//! and the server simulates for it are never idle nor asleep.
//!
//! Reads go through `fastread` (the built-in properties' getters called directly), the rest through
//! `apply::read`. The session's post-tick hook calls `record` per manager, hands the values to netcore
//! (`record_part` says whether any of them changed) and reports back with `observe`.

use super::*;
use crate::fastread;
use crate::net::Net;
use netcore::activity::Poll;
use std::cell::RefCell;
use std::collections::HashSet;

/// One entity `record` polled: its slot, how many parts it pushed (consecutive in the batch), and, for
/// a resting poll of an entity with bodies, whether they all sleep (and none is frozen).
pub struct Polled {
    pub slot: u32,
    pub parts: usize,
    pub rests: Option<bool>,
}

/// What `record` polls of one entity.
struct Work {
    iid: InstanceId,
    slot: u32,
    id: NetId,
    rt: Rc<RuntimeProps>,
    poll: Poll,
    res: Rc<Resolved>,
    /// The hash of the values of its last resting poll (0: none since it was hot).
    sig: u64,
}

/// The getters `fastread` picked for an entity's part nodes (per part, per field), and each RigidBody
/// part's sleeping and frozen getters, with the node list and property table they were resolved for.
pub(super) struct Resolved {
    nodes: PartNodes,
    rt: Rc<RuntimeProps>,
    fields: Vec<Vec<fastread::Getter>>,
    bodies: Vec<Option<(fastread::Getter, fastread::Getter)>>,
}

impl Resolved {
    fn new(rt: &Rc<RuntimeProps>, nodes: &PartNodes) -> Self {
        let mut fields = Vec::with_capacity(nodes.len());
        let mut bodies = Vec::with_capacity(nodes.len());
        for (p, n) in nodes.iter().enumerate() {
            let specs = rt.fields.get(p).map_or(&[][..], |f| f.as_slice());
            match n {
                Some(n) => {
                    fields.push(specs.iter().map(|f| fastread::resolve(n, f.fast)).collect());
                    bodies.push(fastread::resolve_body(n));
                }
                None => {
                    fields.push(vec![fastread::NONE; specs.len()]);
                    bodies.push(None);
                }
            }
        }
        Self {
            nodes: nodes.clone(),
            rt: rt.clone(),
            fields,
            bodies,
        }
    }

    fn has_bodies(&self) -> bool {
        self.bodies.iter().any(Option::is_some)
    }
}

const SIG_SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// One step of the resting polls' value hash (a 64-bit multiply-rotate mix).
fn mix(h: u64, x: u64) -> u64 {
    (h.rotate_left(5) ^ x).wrapping_mul(0x517c_c1b7_2722_0a95)
}

fn mix_f32s(mut h: u64, xs: &[f32]) -> u64 {
    for x in xs {
        h = mix(h, u64::from(x.to_bits()));
    }
    h
}

fn mix_bytes(mut h: u64, b: &[u8]) -> u64 {
    h = mix(h, b.len() as u64);
    for c in b.chunks(8) {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        h = mix(h, u64::from_le_bytes(w));
    }
    h
}

fn mix_value(h: u64, v: &Value) -> u64 {
    match v {
        Value::Nil => mix(h, 1),
        Value::Bool(b) => mix(h, 2 + u64::from(*b)),
        Value::Int(i) => mix(mix(h, 4), *i as u64),
        Value::Float(f) => mix(mix(h, 5), f.to_bits()),
        Value::Vector2(a) => mix_f32s(mix(h, 6), a),
        Value::Vector3(a) => mix_f32s(mix(h, 7), a),
        Value::Vector4(a) => mix_f32s(mix(h, 8), a),
        Value::Vector2i(a) => mix(mix(mix(h, 9), a[0] as u64), a[1] as u64),
        Value::Vector3i(a) => mix(mix(mix(mix(h, 10), a[0] as u64), a[1] as u64), a[2] as u64),
        Value::Quaternion(a) => mix_f32s(mix(h, 11), a),
        Value::Transform2D(a) => mix_f32s(mix(h, 12), a),
        Value::Transform3D(a) => mix_f32s(mix(h, 13), a),
        Value::Color(a) => mix_f32s(mix(h, 14), a),
        Value::Str(s) => mix_bytes(mix(h, 15), s.as_bytes()),
        Value::Bytes(b) => mix_bytes(mix(h, 16), b),
    }
}

thread_local! {
    /// Entity roots whose scripts called `Net.input` (each once), and the ones not handed over yet.
    static INPUT_NOTED: RefCell<HashSet<InstanceId>> = RefCell::new(HashSet::new());
    static INPUT_QUEUE: RefCell<Vec<InstanceId>> = const { RefCell::new(Vec::new()) };
}

/// `Net.input(node)` was called for the entity rooted at `root`: it is simulated from input.
pub fn note_input(root: &Gd<Node>) {
    let iid = root.instance_id();
    let fresh = INPUT_NOTED.with(|s| s.borrow_mut().insert(iid));
    if fresh {
        INPUT_QUEUE.with(|q| q.borrow_mut().push(iid));
    }
}

/// Hands the entities noted by [`note_input`] since the last call to their managers.
pub fn flush_input_driven() {
    let queue = INPUT_QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut()));
    for iid in queue {
        let Ok(node) = Gd::<Node>::try_from_instance_id(iid) else {
            INPUT_NOTED.with(|s| s.borrow_mut().remove(&iid));
            continue;
        };
        if let Some((root, mut m)) = entity_root_of(&node) {
            m.bind_mut().set_input_driven(&root);
        }
    }
}

impl NetManager {
    /// 2.2 step 5 on the server: polls the hot list (hot properties, and the cold ones every `cold`
    /// ticks staggered by id, or right after `mark_cold_dirty`) and this tick's bucket of idle and
    /// sleeping entities. `polled` says which parts of `out` belong to which entity. A resting poll
    /// whose values hash the same as the previous resting poll's pushes nothing (netcore holds exactly
    /// those values: every poll since the entity stopped being hot recorded all of them).
    pub fn record(
        &mut self,
        tick: u64,
        cold: u32,
        idle_after: u32,
        out: &mut Vec<Recorded>,
        polled: &mut Vec<Polled>,
    ) {
        if !self.online || !self.is_server {
            return;
        }
        self.activate_pending();
        self.activity.set_period(cold);
        self.activity.set_idle_after(idle_after);
        let mut due = Vec::new();
        self.activity.due(tick, &mut due);
        let mut work = Vec::with_capacity(due.len());
        let mut stale = Vec::new();
        for (slot, iid, poll) in due {
            let Some(e) = self.entries.get_mut(&iid) else {
                stale.push(slot);
                continue;
            };
            let (Some(id), Some(rt)) = (e.id, e.props.clone()) else {
                continue;
            };
            // WP7: a `client_owned` entity a client owns is recorded from its owner's state.
            if e.client_owned && e.owner != netcore::SERVER {
                continue;
            }
            let poll = match poll {
                Poll::Hot { cold } => {
                    e.rest_sig = 0;
                    Poll::Hot {
                        cold: cold || e.cold_force,
                    }
                }
                p => p,
            };
            e.cold_force = false;
            let res = match &e.resolved {
                Some(r) if Rc::ptr_eq(&r.nodes, &e.nodes) && Rc::ptr_eq(&r.rt, &rt) => r.clone(),
                _ => {
                    let r = Rc::new(Resolved::new(&rt, &e.nodes));
                    e.resolved = Some(r.clone());
                    r
                }
            };
            let w = Work {
                iid,
                slot,
                id,
                rt,
                poll,
                res,
                sig: e.rest_sig,
            };
            work.push(w);
        }
        for slot in stale {
            self.activity.remove(slot);
        }
        let mut sigs = Vec::new();
        {
            let _guard = self.base_mut();
            // This entity's values as (part, field, value), parts in order.
            let mut flat: Vec<(u16, u16, Value)> = Vec::new();
            for w in work {
                let resting = !matches!(w.poll, Poll::Hot { .. });
                let mut rests = (resting && w.res.has_bodies()).then_some(true);
                let mut sig = SIG_SEED;
                flat.clear();
                for (p, node) in w.res.nodes.iter().enumerate() {
                    let Some(node) = node else {
                        continue;
                    };
                    if !node.is_instance_valid() {
                        continue;
                    }
                    let body = w.res.bodies.get(p).copied().flatten();
                    if let (Some(g), true) = (body, resting) {
                        // SAFETY: checked alive above, on this thread; `g` was resolved for it.
                        if !unsafe { fastread::body_rests(node, g) } {
                            rests = Some(false);
                        }
                    }
                    let body = body.is_some();
                    let none: &[u16] = &[];
                    let (hot, colds) = match w.poll {
                        Poll::Hot { cold } => (
                            w.rt.hot[p].as_slice(),
                            if cold { w.rt.cold[p].as_slice() } else { none },
                        ),
                        Poll::Asleep if body => (none, w.rt.cold[p].as_slice()),
                        _ => (w.rt.hot[p].as_slice(), w.rt.cold[p].as_slice()),
                    };
                    if hot.is_empty() && colds.is_empty() {
                        continue;
                    }
                    let getters = &w.res.fields[p];
                    for &f in hot.iter().chain(colds) {
                        let field = &w.rt.fields[p][usize::from(f)];
                        let g = getters[usize::from(f)];
                        let v = if g == fastread::NONE {
                            apply::read(node, field)
                        } else {
                            // SAFETY: checked alive above, `g` was resolved for this node; a built-in
                            // getter runs no user code.
                            unsafe { fastread::read_resolved(node, g) }
                                .unwrap_or_else(|| apply::read(node, field))
                        };
                        if resting {
                            sig = mix(sig, (p as u64) << 16 | u64::from(f));
                            sig = mix_value(sig, &v);
                        }
                        flat.push((p as u16, f, v));
                    }
                }
                let start = out.len();
                let same = resting && w.sig != 0 && sig == w.sig;
                if !same {
                    for (p, f, v) in flat.drain(..) {
                        let mine = out.len() > start;
                        match out.last_mut() {
                            Some((_, part, vals)) if mine && *part == p => vals.push((f, v)),
                            _ => out.push((w.id, p, vec![(f, v)])),
                        }
                    }
                }
                if resting && sig != w.sig {
                    sigs.push((w.iid, sig));
                }
                polled.push(Polled {
                    slot: w.slot,
                    parts: out.len() - start,
                    rests,
                });
            }
        }
        for (iid, sig) in sigs {
            if let Some(e) = self.entries.get_mut(&iid) {
                e.rest_sig = sig;
            }
        }
    }

    /// What `record_part` made of this tick's polls: (slot, changed, bodies rest) per polled entity.
    pub fn observe(&mut self, results: &[(u32, bool, Option<bool>)]) {
        use netcore::activity::State;
        for &(slot, changed, rests) in results {
            match (self.activity.state(slot), rests) {
                // A sleeping body was woken without its signal (`sleeping = false`) or frozen.
                (Some(State::Asleep), Some(false)) => self.activity.set_asleep(slot, false),
                // An idle body was put to sleep without its signal (`sleeping = true`).
                (Some(State::Idle), Some(true)) => {
                    self.activity.observe(slot, changed);
                    self.activity.set_asleep(slot, true);
                }
                _ => self.activity.observe(slot, changed),
            }
        }
    }

    /// (hot, idle, sleeping) entity counts.
    pub fn activity_counts(&self) -> (usize, usize, usize) {
        self.activity.counts()
    }

    /// Gives the entities that got an id since the last record their slots.
    fn activate_pending(&mut self) {
        if self.pending_act.is_empty() {
            return;
        }
        for iid in std::mem::take(&mut self.pending_act) {
            self.activate(iid);
        }
    }

    fn activate(&mut self, iid: InstanceId) {
        let Some(e) = self.entries.get(&iid) else {
            return;
        };
        let Some(id) = e.id else {
            return;
        };
        if e.act.is_some() {
            return;
        }
        let slot = self.activity.insert(iid, u64::from(id));
        let me = self.to_gd();
        let res = e
            .props
            .as_ref()
            .map(|rt| Rc::new(Resolved::new(rt, &e.nodes)));
        let mut conns = Vec::new();
        if let Some(res) = &res {
            for (p, n) in res.nodes.iter().enumerate() {
                let (Some(n), Some(_)) = (n, res.bodies[p]) else {
                    continue;
                };
                let cb = Callable::from_object_method(&me, "_net_sleep_changed")
                    .bindv(&varray![iid.to_i64()]);
                let mut n = n.clone();
                n.connect("sleeping_state_changed", &cb);
                conns.push((n, cb));
            }
        }
        if let Some(e) = self.entries.get_mut(&iid) {
            e.act = Some(slot);
            e.resolved = res;
            e.sleep_conns = conns;
        }
        self.refresh_always(iid);
        self.refresh_sleep(iid);
    }

    /// Takes an entity out of the activity and disconnects its bodies.
    pub(super) fn deactivate(&mut self, iid: InstanceId) {
        let Some(e) = self.entries.get_mut(&iid) else {
            return;
        };
        let conns = std::mem::take(&mut e.sleep_conns);
        e.resolved = None;
        e.rest_sig = 0;
        if let Some(slot) = e.act.take() {
            self.activity.remove(slot);
        }
        for (mut n, cb) in conns {
            if n.is_instance_valid() && n.is_connected("sleeping_state_changed", &cb) {
                n.disconnect("sleeping_state_changed", &cb);
            }
        }
    }

    pub(super) fn deactivate_all(&mut self) {
        let iids: Vec<InstanceId> = self
            .entries
            .iter()
            .filter(|(_, e)| e.act.is_some())
            .map(|(i, _)| *i)
            .collect();
        for iid in iids {
            self.deactivate(iid);
        }
        self.activity = netcore::Activity::default();
        self.pending_act.clear();
    }

    /// Polls the entity at the next record (no-op for one without a slot).
    pub(super) fn wake_iid(&mut self, iid: InstanceId) {
        if let Some(slot) = self.entries.get(&iid).and_then(|e| e.act) {
            self.activity.wake(slot);
        }
    }

    /// Its owner changed: polled at once, and always active while a client owns it (the server
    /// simulates it from that client's input).
    pub(super) fn owner_moved(&mut self, iid: InstanceId) {
        self.refresh_always(iid);
        self.wake_iid(iid);
    }

    fn refresh_always(&mut self, iid: InstanceId) {
        let Some(e) = self.entries.get(&iid) else {
            return;
        };
        let Some(slot) = e.act else {
            return;
        };
        let always = e.input_driven || (e.owner != netcore::SERVER && !e.client_owned);
        if always != self.activity.is_always(slot) {
            self.activity.set_always(slot, always);
        }
    }

    /// Reads whether every RigidBody part of the entity sleeps (and none is frozen).
    fn refresh_sleep(&mut self, iid: InstanceId) {
        let Some(e) = self.entries.get(&iid) else {
            return;
        };
        let (Some(slot), Some(res)) = (e.act, e.resolved.clone()) else {
            return;
        };
        if !res.has_bodies() {
            return;
        }
        let mut rests = true;
        for (n, g) in res.nodes.iter().zip(&res.bodies) {
            let Some(g) = g else {
                continue;
            };
            let ok = n.as_ref().is_some_and(|n| {
                // SAFETY: checked alive first, on this thread; `g` was resolved for this node.
                n.is_instance_valid() && unsafe { fastread::body_rests(n, *g) }
            });
            rests &= ok;
        }
        self.activity.set_asleep(slot, rests);
    }

    /// The entity rooted at `root` reads `Net.input`: never idle nor asleep from now on.
    fn set_input_driven(&mut self, root: &Gd<Node>) {
        let iid = root.instance_id();
        if let Some(e) = self.entries.get_mut(&iid) {
            if !e.input_driven {
                e.input_driven = true;
                self.refresh_always(iid);
            }
        }
    }
}

#[godot_api(secondary)]
impl NetManager {
    /// WP18: a RigidBody part of the entity whose root has instance id `root` fell asleep or woke.
    #[func]
    fn _net_sleep_changed(&mut self, root: i64) {
        self.refresh_sleep(InstanceId::from_i64(root));
    }
}

/// `Net.touch` / `NetSession.touch`: the entity `node` belongs to (any part of it) is polled in full at
/// the next record, idle or asleep.
fn touch(node: &Gd<Node>) {
    if let Some((root, mut m)) = entity_root_of(node) {
        m.bind_mut().mark_cold_dirty(&root);
    }
}

#[godot_api(secondary)]
impl NetSession {
    /// WP18: the entity `node` belongs to (any part of it) is polled at the next server tick even when
    /// idle or asleep, so a change a script made directly is sent without the idle delay.
    #[func]
    fn touch(&self, node: Gd<Node>) {
        touch(&node);
    }
}

#[godot_api(secondary)]
impl Net {
    /// WP18: the entity `node` belongs to (any part of it) is polled at the next server tick even when
    /// idle or asleep, so a change a script made directly is sent without the idle delay.
    #[func]
    fn touch(&self, node: Gd<Node>) {
        touch(&node);
    }
}
