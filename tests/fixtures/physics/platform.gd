extends RigidBody2D
## Test fixture (WP19): a platform that swings left and right on a spring, plain single-player code: a
## force from its own state in `_integrate_forces`, so every peer that simulates it moves it the same way.

## The spring's stiffness per unit of mass (a period of about three seconds).
const STIFFNESS := 4.4


func _integrate_forces(state: PhysicsDirectBodyState2D) -> void:
	state.apply_central_force(Vector2(-STIFFNESS * state.transform.origin.x * mass, 0.0))
