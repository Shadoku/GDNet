//! netcore: the sans-IO protocol engine behind the gdnet Godot extension.
//!
//! Rules (docs/PLAN_NETCODE.md section 2): this crate never depends on `godot`, never opens a socket,
//! never spawns a thread. Packets come in through `poll`-style calls and go out as byte buffers; the
//! caller (gdnet on the main thread, or a test) does the I/O. It must build for
//! `wasm32-unknown-emscripten`.

#![forbid(unsafe_code)]

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
        rust_sources(Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path(), &mut files);
        assert!(!files.is_empty());
        for file in files {
            let src = std::fs::read_to_string(&file).expect("read source");
            for needle in &needles {
                assert!(!src.contains(needle.as_str()), "{} uses {needle}", file.display());
            }
        }
    }
}
