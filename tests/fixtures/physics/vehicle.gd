extends RigidBody2D
## Test fixture (WP19): a top-down vehicle, plain single-player code: a force from its input in
## `_physics_process`. Who simulates it is the physics mode's business, not this script's.

## Pixels per second squared per unit of mass.
const THRUST := 600.0


func _physics_process(_delta: float) -> void:
	var input := Net.input(self)
	apply_central_force(Vector2(input.get_axis("left", "right"), input.get_axis("up", "down")) * THRUST)
