extends "res://tests/fixtures/predict/ball_3d.gd"
## Test fixture (WP19 `custom`, 3D): a ship pushed along its nose by a replicated throttle, with the
## `_net_predict_body` hook of docs/PLAN_NETCODE.md 12.4, copied verbatim. The scene turns gravity
## and damping off (scene values, read by the replica itself).

const THRUST := 6.0

## Replicated (an `@export` variable): 0..1.
@export var throttle: float = 0.0


func _physics_process(_delta: float) -> void:
	if not freeze:
		apply_central_force(-transform.basis.z * throttle * THRUST * mass)


func _net_predict_body(delta: float) -> void:
	Net.dead_reckon(self, delta)                       # the default step; optional
	linear_velocity += -transform.basis.z * throttle * THRUST * delta
	position += -transform.basis.z * throttle * THRUST * delta * delta * 0.5
