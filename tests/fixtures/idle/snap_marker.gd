extends Node2D
## Test fixture (WP18): a marker whose position and rotation replicate without interpolation, so a
## client's copy changes in the frame the snapshot arrives; `teleport` is a call that moves it.


func teleport(p: Vector2) -> void:
	position = p
