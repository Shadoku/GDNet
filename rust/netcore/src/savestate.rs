//! Session state (docs/PLAN_NETCODE.md 12.3 "Session state", WP17): what `NetSession.save_state()`
//! writes and `Net.host(port, state)` / `Net.offline(state)` start from.
//!
//! It holds the keyed state (the map and the write rules; views are code and are declared again), the
//! seats with their keys (and which seat was the host's), and every level's state as the level state
//! blobs of levelstate.rs, by scene path. A session started from it has the keyed map, reserves every
//! seat for its key (a client joining with a saved key gets its seat back) and loads each level from
//! its blob. Nothing here goes on the wire: the bytes belong to one game build and its saves.
//!
//! A blob is [`MAGIC`], the format version (`u16`, little endian, [`SESSION_STATE_VERSION`]), then the
//! postcard encoding of [`SessionState`]. Another magic or version is refused ([`SessionStateError`]).

use crate::keyed::Who;
use crate::seats::Seat;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::string::String;
use std::vec::Vec;

/// The first four bytes of every session state.
pub const MAGIC: [u8; 4] = *b"GDSS";
/// The format version this build writes and reads.
pub const SESSION_STATE_VERSION: u16 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    /// Every keyed path and its value (gdnet's Variant encoding).
    pub keyed: Vec<(String, Vec<u8>)>,
    /// The keyed write rules.
    pub rules: Vec<(String, Who)>,
    /// Every seat and its key.
    pub seats: Vec<(Seat, String)>,
    /// The host's seat on a listen server (a new host takes it whatever its key).
    pub host_seat: Option<Seat>,
    /// Level scene path -> level state blob (levelstate.rs).
    pub levels: Vec<(String, Vec<u8>)>,
}

/// Why a session state was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionStateError {
    /// It does not start with [`MAGIC`].
    NotSessionState,
    /// A format version this build does not read.
    UnknownVersion(u16),
    /// The header is right but the rest does not decode.
    Corrupt(String),
}

impl fmt::Display for SessionStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSessionState => write!(f, "it is not a session state (no GDSS header)"),
            Self::UnknownVersion(v) => write!(
                f,
                "its format version is {v}; this build reads version {SESSION_STATE_VERSION}"
            ),
            Self::Corrupt(why) => write!(f, "it does not decode ({why})"),
        }
    }
}

impl SessionState {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&SESSION_STATE_VERSION.to_le_bytes());
        out.extend_from_slice(&postcard::to_allocvec(self).unwrap_or_default());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SessionStateError> {
        if bytes.len() < 6 || bytes[..4] != MAGIC {
            return Err(SessionStateError::NotSessionState);
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != SESSION_STATE_VERSION {
            return Err(SessionStateError::UnknownVersion(version));
        }
        postcard::from_bytes(&bytes[6..])
            .map_err(|e| SessionStateError::Corrupt(std::format!("{e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let st = SessionState {
            keyed: std::vec![("score/1".into(), std::vec![1, 2])],
            rules: std::vec![("hand/".into(), Who::Seat), ("chat/".into(), Who::All)],
            seats: std::vec![(1, "host".into()), (2, "alice".into())],
            host_seat: Some(1),
            levels: std::vec![("res://cave.tscn".into(), std::vec![b'G', b'D', b'L', b'S'])],
        };
        let bytes = st.encode();
        assert_eq!(&bytes[..4], &MAGIC);
        assert_eq!(SessionState::decode(&bytes), Ok(st));
    }
}
