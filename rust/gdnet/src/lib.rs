//! gdnet: the GDExtension side. Everything that touches `Gd<T>` lives here, on the main thread.
//! See docs/PLAN_NETCODE.md.

use godot::classes::Engine;
use godot::init::InitStage;
use godot::prelude::*;

mod api_ext;
mod apply;
mod configure;
mod emulation;
mod entity_node;
mod infer;
mod manager;
mod net;
mod parts;
mod pipe_peer;
mod poll;
mod profiles;
mod session;
mod version;

struct GdNetExtension;

/// The singleton's name as GDScript sees it.
const NET_SINGLETON: &str = "Net";

#[gdextension]
unsafe impl ExtensionLibrary for GdNetExtension {
    fn on_stage_init(stage: InitStage) {
        if stage == InitStage::Scene {
            Engine::singleton().register_singleton(NET_SINGLETON, &net::Net::new_alloc());
        }
    }

    fn on_stage_deinit(stage: InitStage) {
        if stage == InitStage::Scene {
            let mut engine = Engine::singleton();
            if let Some(net) = engine.get_singleton(NET_SINGLETON) {
                engine.unregister_singleton(NET_SINGLETON);
                net.free();
            }
        }
    }
}
