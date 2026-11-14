extends RigidBody3D
## Test fixture (WP19 body prediction, 3D): the twin of ball.gd. The server throws it (`launch`); it
## records in `_process` what it shows and, beside it, the server's copy (`other`) and its speed.

var recording: bool = false
var shown: PackedVector3Array = PackedVector3Array()
var shown_rotations: Array[Quaternion] = []
var other: RigidBody3D = null
var other_shown: PackedVector3Array = PackedVector3Array()
var other_rotations: Array[Quaternion] = []
var other_speed: PackedFloat64Array = PackedFloat64Array()


## Server: unfreezes the ball with `velocity` and `spin` (the scene starts frozen).
func launch(velocity: Vector3, spin: Vector3 = Vector3.ZERO) -> void:
	freeze = false
	linear_velocity = velocity
	angular_velocity = spin
	Net.touch(self)


func _process(_delta: float) -> void:
	if not recording:
		return
	shown.append(position)
	shown_rotations.append(quaternion)
	if other != null:
		other_shown.append(other.position)
		other_rotations.append(other.quaternion)
		other_speed.append(other.linear_velocity.length())


func clear_recording() -> void:
	shown.clear()
	shown_rotations.clear()
	other_shown.clear()
	other_rotations.clear()
	other_speed.clear()
