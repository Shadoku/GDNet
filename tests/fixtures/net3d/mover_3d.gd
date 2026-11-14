extends Node3D
## Test fixture (WP18): moves at a constant velocity and turns where it simulates (the server), and
## records what it shows in `_process`, where gdnet has already written a replica's interpolated values
## for the frame (the session's `_process` runs first).

## Metres per second (set on the server's copy).
var speed: Vector3 = Vector3.ZERO
## Turn rate about `axis`, radians per second.
var spin: float = 0.0
## The axis `spin` turns about (unit length).
var axis: Vector3 = Vector3.UP
## Record what `_process` sees.
var recording: bool = false
var shown: PackedVector3Array = PackedVector3Array()
var shown_quaternions: Array[Quaternion] = []


func _physics_process(delta: float) -> void:
	position += speed * delta
	if spin != 0.0:
		quaternion = (Quaternion(axis, spin * delta) * quaternion).normalized()


func _process(_delta: float) -> void:
	if recording:
		shown.append(position)
		shown_quaternions.append(quaternion)


func clear_recording() -> void:
	shown.clear()
	shown_quaternions.clear()
