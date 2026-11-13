extends Node2D
## Test fixture (WP16, level state): a chest or a coin. `open` and `value` are exported, so they
## replicate as cold state; `_net_spawned` records what they were when it ran, so a test can check
## that a restored level's state was in place before it.

## A chest's lid.
@export var open: bool = false
## A coin's worth.
@export var value: int = 1

## `open` and `value` when `_net_spawned` ran (null before it ran).
var at_spawn: Variant = null


func _net_spawned(_info: Dictionary) -> void:
	at_spawn = {"open": open, "value": value}
