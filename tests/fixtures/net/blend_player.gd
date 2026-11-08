extends "res://tests/fixtures/net/predicted_player.gd"
## Test fixture (WP5): rung 1's player, recording the `position` its `_physics_process` (fresh and
## replayed) and its `_process` see. After a rollback gdnet shows a decaying offset in `_process` only.

var record_frames: bool = false
var seen_in_physics: PackedVector2Array = PackedVector2Array()
var seen_in_process: PackedVector2Array = PackedVector2Array()


func _physics_process(delta: float) -> void:
	if record_frames:
		seen_in_physics.append(position)
	super(delta)


func _process(delta: float) -> void:
	super(delta)
	if record_frames:
		seen_in_process.append(position)
