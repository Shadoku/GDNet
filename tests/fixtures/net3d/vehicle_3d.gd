extends RigidBody3D
## Test fixture (WP19): a hovering vehicle in 3D, plain single-player code: a force from its input in
## `_physics_process`. Who simulates it is the physics mode's business, not this script's.

## Metres per second squared per unit of mass.
const THRUST := 6.0


func _physics_process(_delta: float) -> void:
	var input := Net.input(self)
	apply_central_force(Vector3(input.get_axis("left", "right"), 0.0, input.get_axis("up", "down")) * THRUST * mass)
