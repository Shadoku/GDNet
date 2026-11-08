use godot::prelude::*;

/// Reports the extension's version. Exists so a project (and `tests/unit/test_gdnet_loads.gd`) can check
/// that the library loaded and that gdnet and netcore agree.
#[derive(GodotClass)]
#[class(init, base = RefCounted)]
pub struct NetVersion {
    base: Base<RefCounted>,
}

#[godot_api]
impl NetVersion {
    /// The gdnet crate version, `MAJOR.MINOR.PATCH`.
    #[func]
    fn version(&self) -> GString {
        GString::from(env!("CARGO_PKG_VERSION"))
    }

    /// The netcore crate version; equal to `version()` in a consistent build.
    #[func]
    fn core_version(&self) -> GString {
        GString::from(netcore::VERSION)
    }
}
