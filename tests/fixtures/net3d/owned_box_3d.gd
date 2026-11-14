extends Node3D
## Test fixture (WP18): a `client_owned` 3D box whose owner moves and turns it in its own script (no
## input, no prediction), recording the positions its `_physics_process` sees.

## Metres per second (set on the owner's copy).
var speed: Vector3 = Vector3.ZERO
## Radians per second about Y (set on the owner's copy).
var spin: float = 0.0
## The position at the start of every `_physics_process` call.
var seen: PackedVector3Array = PackedVector3Array()


func _physics_process(delta: float) -> void:
	seen.append(position)
	position += speed * delta
	rotate_y(spin * delta)
