//! `NetEmulation`: the network emulator's settings as a Resource (docs/PLAN_NETCODE.md 1, 3.1). Latency,
//! variance, loss, reorder and duplication for what this side sends and what it receives, plus
//! Unreal's presets. The numbers are read at every poll, so editing a live resource takes effect at once.

use godot::prelude::*;
use netcore::EmulSettings;

#[derive(GodotClass)]
#[class(init, base = Resource)]
pub struct NetEmulation {
    base: Base<Resource>,
    /// Minimum delay added to every packet this side sends, in milliseconds.
    #[export(range = (0.0, 2000.0, 1.0, or_greater))]
    send_latency_ms: f64,
    /// Extra random delay on sent packets, uniform in [0, variance], in milliseconds.
    #[export(range = (0.0, 2000.0, 1.0, or_greater))]
    send_variance_ms: f64,
    /// Probability a sent packet is lost (reliable packets are resent, which shows as delay; at 1.0 the
    /// link is dead and nothing arrives).
    #[export(range = (0.0, 1.0, 0.01))]
    send_loss: f64,
    /// Probability an unreliable sent packet is held back and arrives after later ones.
    #[export(range = (0.0, 1.0, 0.01))]
    send_reorder: f64,
    /// Probability an unreliable sent packet arrives twice.
    #[export(range = (0.0, 1.0, 0.01))]
    send_duplicate: f64,
    /// Minimum delay added to every packet this side receives, in milliseconds.
    #[export(range = (0.0, 2000.0, 1.0, or_greater))]
    receive_latency_ms: f64,
    /// Extra random delay on received packets, in milliseconds.
    #[export(range = (0.0, 2000.0, 1.0, or_greater))]
    receive_variance_ms: f64,
    /// Probability a received packet is lost.
    #[export(range = (0.0, 1.0, 0.01))]
    receive_loss: f64,
    /// Probability an unreliable received packet is reordered.
    #[export(range = (0.0, 1.0, 0.01))]
    receive_reorder: f64,
    /// Probability an unreliable received packet is duplicated.
    #[export(range = (0.0, 1.0, 0.01))]
    receive_duplicate: f64,
    /// Seed of the emulator's random numbers; the same seed gives the same schedule.
    #[export]
    seed: i64,
}

#[godot_api]
impl NetEmulation {
    /// Sets both directions to a named preset: `"off"`, `"average"` (30-60 ms, 1 % loss) or `"bad"`
    /// (100-200 ms, 5 % loss), Unreal's profiles. Returns false (and changes nothing) for an unknown name.
    #[func]
    fn preset(&mut self, name: GString) -> bool {
        let Some(s) = EmulSettings::preset(&name.to_string()) else {
            return false;
        };
        self.apply(s, s);
        true
    }

    /// True when every value is zero (no emulation).
    #[func]
    pub fn is_off(&self) -> bool {
        self.send_settings().is_off() && self.receive_settings().is_off()
    }
}

impl NetEmulation {
    pub fn send_settings(&self) -> EmulSettings {
        EmulSettings {
            latency_ms: self.send_latency_ms,
            variance_ms: self.send_variance_ms,
            loss: self.send_loss,
            reorder: self.send_reorder,
            duplicate: self.send_duplicate,
        }
        .clamped()
    }

    pub fn receive_settings(&self) -> EmulSettings {
        EmulSettings {
            latency_ms: self.receive_latency_ms,
            variance_ms: self.receive_variance_ms,
            loss: self.receive_loss,
            reorder: self.receive_reorder,
            duplicate: self.receive_duplicate,
        }
        .clamped()
    }

    pub fn seed_value(&self) -> u64 {
        self.seed as u64
    }

    fn apply(&mut self, send: EmulSettings, receive: EmulSettings) {
        self.send_latency_ms = send.latency_ms;
        self.send_variance_ms = send.variance_ms;
        self.send_loss = send.loss;
        self.send_reorder = send.reorder;
        self.send_duplicate = send.duplicate;
        self.receive_latency_ms = receive.latency_ms;
        self.receive_variance_ms = receive.variance_ms;
        self.receive_loss = receive.loss;
        self.receive_reorder = receive.reorder;
        self.receive_duplicate = receive.duplicate;
    }
}

/// The settings of an optional resource: `(send, receive, seed)`, all off without one.
pub fn settings_of(e: &Option<Gd<NetEmulation>>) -> (EmulSettings, EmulSettings, u64) {
    match e {
        Some(e) => {
            let b = e.bind();
            (b.send_settings(), b.receive_settings(), b.seed_value())
        }
        None => (EmulSettings::default(), EmulSettings::default(), 0),
    }
}
