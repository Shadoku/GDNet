class_name DoorsPlayer
extends CharacterBody2D
## Rung 1's predicted player (docs/PLAN_NETCODE.md section 1) with a camera that follows its owner.

const SPEED := 200.0


func _net_spawned(_info: Dictionary) -> void:
	$Camera2D.enabled = Net.is_owner(self)


func _physics_process(_delta: float) -> void:
	velocity = Net.input(self).get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()


## After a door the player stands somewhere else, so the camera jumps instead of gliding there. A walk
## into the next chunk is a travel too, and the camera keeps gliding.
func _net_travelled(_from: String, _to: String) -> void:
	var camera: Camera2D = $Camera2D
	if camera.get_screen_center_position().distance_to(global_position) > 600.0:
		camera.reset_smoothing()
