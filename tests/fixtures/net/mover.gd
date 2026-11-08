extends Node2D
## Test fixture (WP5): moves at a constant velocity and spins where it simulates (the server), and
## records what it shows in `_process`, where gdnet has already written a replica's interpolated values
## for the frame (the session's `_process` runs first, at priority -1000).

## Pixels per second (set on the server's copy).
var speed: Vector2 = Vector2.ZERO
## Radians per second; the rotation is kept in [-PI, PI).
var spin: float = 0.0
## Record what `_process` sees.
var recording: bool = false
var shown: PackedVector2Array = PackedVector2Array()
var shown_rotations: PackedFloat64Array = PackedFloat64Array()
var shown_modulates: Array[Color] = []
## Microseconds at each recorded frame.
var frame_times: PackedInt64Array = PackedInt64Array()
## Another node (the server's copy) whose position is recorded beside this one's, in the same frame.
var other: Node2D = null
var other_shown: PackedVector2Array = PackedVector2Array()
## `_process` calls.
var process_frames: int = 0


func _physics_process(delta: float) -> void:
	position += speed * delta
	if spin != 0.0:
		rotation = wrapf(rotation + spin * delta, -PI, PI)


func _process(_delta: float) -> void:
	process_frames += 1
	if not recording:
		return
	shown.append(position)
	shown_rotations.append(rotation)
	shown_modulates.append(modulate)
	frame_times.append(Time.get_ticks_usec())
	if other != null:
		other_shown.append(other.position)


func clear_recording() -> void:
	shown.clear()
	shown_rotations.clear()
	shown_modulates.clear()
	frame_times.clear()
	other_shown.clear()
