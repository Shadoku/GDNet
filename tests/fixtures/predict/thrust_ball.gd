extends "res://tests/fixtures/predict/ball.gd"
## Test fixture (WP19 `custom`): a ball pushed sideways by a constant force on the server, and a
## `_net_predict_body` hook that models the same push on top of gdnet's built-in step.

const THRUST := Vector2(2400.0, 0.0)

## Hook calls (on a replica), and the deltas they were given.
var hook_calls: int = 0
var hook_time: float = 0.0


func launch(velocity: Vector2) -> void:
	constant_force = THRUST * mass
	super.launch(velocity)


func _net_predict_body(delta: float) -> void:
	hook_calls += 1
	hook_time += delta
	Net.dead_reckon(self, delta)
	linear_velocity += THRUST * delta
	position += THRUST * delta * delta
