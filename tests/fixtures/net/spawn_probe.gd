extends Node2D
## Test fixture (WP2): a scene root that records gdnet's spawn callbacks.

## Paths of the probes whose `_net_despawning` ran, in order (process-wide; tests clear it).
static var despawn_log: Array[String] = []

## The `info` of the last `_net_spawned` call.
var spawn_info: Dictionary = {}
## How many times `_net_spawned` ran on this node.
var spawn_calls: int = 0


func _net_spawned(info: Dictionary) -> void:
	spawn_calls += 1
	spawn_info = info


func _net_despawning() -> void:
	despawn_log.append(str(get_path()))
