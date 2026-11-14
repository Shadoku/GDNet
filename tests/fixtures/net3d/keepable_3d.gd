extends Node3D
## Test fixture (WP18, level state): a 3D chest or coin. `open` and `value` are exported, so they
## replicate as cold state; `_net_spawned` records them, so a test can check that a restored level's
## state was in place before it.

## A chest's lid.
@export var open: bool = false
## A coin's worth.
@export var value: int = 1

## `open` and `value` when `_net_spawned` ran (null before it ran).
var at_spawn: Variant = null


func _net_spawned(_info: Dictionary) -> void:
	at_spawn = {"open": open, "value": value}
