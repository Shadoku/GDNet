extends Node3D
## Test fixture (WP18): a plain 3D entity root with a nested part (`Arm`). `hp` is exported, so it
## replicates as a cold property; `_net_spawned` records the state it saw, so a test can check that the
## initial state was in place before it.

## Hit points (a cold `@export` property).
@export var hp: int = 10

## position, quaternion, scale, hp and the arm's position when `_net_spawned` ran (empty before).
var at_spawn: Dictionary = {}


func _net_spawned(_info: Dictionary) -> void:
	at_spawn = {
		"position": position,
		"quaternion": quaternion,
		"scale": scale,
		"hp": hp,
		"arm": ($Arm as Node3D).position,
	}
