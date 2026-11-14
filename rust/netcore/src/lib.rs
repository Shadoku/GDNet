//! netcore: the sans-IO protocol engine behind the gdnet Godot extension.
//!
//! Rules (docs/PLAN_NETCODE.md section 2): this crate never depends on `godot`, never opens a socket,
//! never spawns a thread. Packets come in through `poll`-style calls and go out as byte buffers; the
//! caller (gdnet on the main thread, or a test) does the I/O. It must build for
//! `wasm32-unknown-emscripten`.
//!
//! WP1 modules: [`clock`] (tick clock, offset, RTT/jitter), [`peers`] (the peer table), [`wire`]
//! (framing and the session messages), [`emul`] (the network emulator), [`session`] (the handshake and
//! `Time` state machine). WP2: [`entity`] (the entity table, id ranges, part tables, `EntityAction`).
//! WP3: [`props`] (property tables), [`quant`] (bit packing, quantisers), [`snapshot`] (the snapshot
//! format), [`priority`] (accumulator and budget), [`delta`] (dirty tracking, acked baselines, packing).
//! WP4: [`input`] (input frames, redundancy, the server's input buffer, adaptive delay), [`predict`]
//! (prediction histories, mismatch detection, rollback requests, the tick lead). WP5: [`interp`] (the
//! render timeline, the adaptive jitter buffer, sample buffers, error offsets). WP6: [`call`] (calls and
//! returns, the argument encoding, the routing verbs). WP7: [`authority`] (transfer rules, owner state
//! acceptance, rejected writes), [`keyed`] (the keyed state map, permissions, views). WP8: [`hash`] (hash
//! telemetry: per-entity hashes, sampling, the client's comparison). WP13: [`rooms`] (rooms, interest by
//! room, travel). WP16: [`interest`] (radius with hysteresis, `visible_to`, filter results), [`levelstate`]
//! (the level state blob kept when a level unloads). WP17: [`seats`] (seat keys, the rejoin window),
//! [`savestate`] (the saved session state: keyed state, seats, level states). WP18: [`activity`] (the
//! hot list, idle and sleeping entities: what the server polls in a tick).

#![forbid(unsafe_code)]

pub mod activity;
pub mod authority;
pub mod call;
pub mod clock;
pub mod correct;
pub mod delta;
pub mod emul;
pub mod entity;
pub mod hash;
pub mod input;
pub mod interest;
pub mod interp;
pub mod keyed;
pub mod levelstate;
pub mod peers;
pub mod predict;
pub mod priority;
pub mod props;
pub mod quant;
pub mod reckon;
pub mod rooms;
pub mod savestate;
pub mod seats;
pub mod session;
pub mod snapshot;
pub mod wire;

pub use activity::{Activity, Poll};
pub use authority::{AuthorityStats, Transfer};
pub use call::{Arg, CallIn, CallMsg, CallPlan, CallResult, CallStats, OutCall, Verb};
pub use correct::PhysicsMode;
pub use delta::{Ack, FieldMsg, ReplStats, Replication, StateUpdate};
pub use emul::{EmulSettings, Emulator, Pcg32};
pub use entity::{
    CreateAction, EntityAction, EntityRecord, EntityTable, NetId, ParentRef, PartTable,
    MANAGER_ROOT,
};
pub use hash::{HashMsg, HashStats};
pub use input::{InputFrame, InputMsg, TakenFrame};
pub use interest::{Interest, Visible};
pub use interp::{Interp, InterpStats};
pub use keyed::{KeyEvent, KeyMsg, KeyedState, Who};
pub use levelstate::{EntityState, LevelState, LevelStateError, ManagerState, PartValues};
pub use peers::{PeerId, SERVER};
pub use predict::{Policy, Rollback};
pub use props::{Audience, PropDesc, Quant, ScenePropsTable, Schedule, TypeTag, Value};
pub use rooms::{room_hash, Origin, RoomEntry, Rooms, GLOBAL};
pub use session::{
    EntityEvent, Event, Outgoing, PlayerMark, Role, ScopeStats, Session, SessionConfig, Spawn,
};
pub use wire::{Channel, Reliability};

/// The protocol engine version, kept equal to the gdnet crate version by `tests/unit/test_gdnet_loads.gd`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn version_is_semver() {
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "{VERSION}");
        for p in parts {
            p.parse::<u32>().expect("numeric version part");
        }
    }

    fn rust_sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// The crate's sources must not reach for the engine, the network or threads (plan section 9).
    /// The needles are assembled at runtime so this file does not match itself.
    #[test]
    fn no_godot_network_or_thread_use() {
        let needles = [
            format!("{}::", "godot"),
            format!("std::{}", "net"),
            format!("std::{}", "thread"),
            format!("{} {}", "extern", "crate"),
        ];
        let mut files = Vec::new();
        rust_sources(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path(),
            &mut files,
        );
        assert!(!files.is_empty());
        for file in files {
            let src = std::fs::read_to_string(&file).expect("read source");
            for needle in &needles {
                assert!(
                    !src.contains(needle.as_str()),
                    "{} uses {needle}",
                    file.display()
                );
            }
        }
    }
}
