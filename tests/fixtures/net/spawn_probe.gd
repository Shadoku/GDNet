extends Node2D
## Test fixture (WP2): a scene root that records gdnet's spawn callbacks.

## The `info` of the last `_net_spawned` call.
var spawn_info: Dictionary = {}
## How many times `_net_spawned` ran on this node.
var spawn_calls: int = 0


func _net_spawned(info: Dictionary) -> void:
	spawn_calls += 1
	spawn_info = info


func _net_despawning() -> void:
	despawn_log().append(str(get_path()))


## Paths of the probes whose `_net_despawning` ran, in order (process-wide; tests clear it). Kept in an
## Engine meta, not a `static var`: a script with static variables can be reported as a resource still
## in use at exit, depending on which test files share a runner process.
static func despawn_log() -> Array:
	if not Engine.has_meta(&"_spawn_probe_despawn_log"):
		Engine.set_meta(&"_spawn_probe_despawn_log", [])
	return Engine.get_meta(&"_spawn_probe_despawn_log")
