extends "res://tests/fixtures/predict/ball.gd"
## Test fixture (WP19 `custom`): a hook that ignores gdnet's step and puts the ball somewhere
## recognisable, so a test can tell the hook's result is what the replica shows.

const SPOT := Vector2(1234.0, -1234.0)


func _net_predict_body(_delta: float) -> void:
	position = SPOT
	rotation = 1.0
