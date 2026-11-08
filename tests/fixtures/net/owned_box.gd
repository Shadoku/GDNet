extends Node2D
## Test fixture (WP7): a `client_owned` box whose owner moves it directly in its own script (no input,
## no prediction), and that records the positions its `_physics_process` sees.

## Pixels per second (set on the owner's copy).
var speed: Vector2 = Vector2.ZERO
## The position at the start of every `_physics_process` call.
var seen: PackedVector2Array = PackedVector2Array()


func _physics_process(delta: float) -> void:
	seen.append(position)
	position += speed * delta
