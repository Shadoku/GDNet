//! gdnet: the GDExtension side. Everything that touches `Gd<T>` lives here, on the main thread.
//! See docs/PLAN_NETCODE.md.

use godot::prelude::*;

mod version;

struct GdNetExtension;

#[gdextension]
unsafe impl ExtensionLibrary for GdNetExtension {}
