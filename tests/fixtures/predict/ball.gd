extends RigidBody2D
## Test fixture (WP19 body prediction): a ball the server throws (`launch`), which records in `_process`
## what it shows. gdnet writes a replica's pose in the session's `_process` (priority -1000), so this
## script (priority 0) sees the frame's pose; `other` (the server's copy) is recorded beside it, with
## its speed, in the same frame.

var recording: bool = false
var shown: PackedVector2Array = PackedVector2Array()
var shown_rotations: PackedFloat64Array = PackedFloat64Array()
var other: RigidBody2D = null
var other_shown: PackedVector2Array = PackedVector2Array()
var other_speed: PackedFloat64Array = PackedFloat64Array()
var frame_times: PackedInt64Array = PackedInt64Array()


## Server: unfreezes the ball with `velocity` (the scene starts frozen, so it waits in the air).
func launch(velocity: Vector2) -> void:
	freeze = false
	linear_velocity = velocity
	Net.touch(self)


func _process(_delta: float) -> void:
	if not recording:
		return
	shown.append(position)
	shown_rotations.append(rotation)
	frame_times.append(Time.get_ticks_usec())
	if other != null:
		other_shown.append(other.position)
		other_speed.append(other.linear_velocity.length())


func clear_recording() -> void:
	shown.clear()
	shown_rotations.clear()
	other_shown.clear()
	other_speed.clear()
	frame_times.clear()
