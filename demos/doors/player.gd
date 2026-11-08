class_name DoorsPlayer
extends CharacterBody2D
## Rung 1's predicted player (docs/PLAN_NETCODE.md section 1) with a camera that follows its owner.

const SPEED := 200.0


func _net_spawned(_info: Dictionary) -> void:
	$Camera2D.enabled = Net.is_owner(self)


func _physics_process(_delta: float) -> void:
	velocity = Net.input(self).get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()


## After a door: the player stands in another level, so the camera jumps instead of gliding there.
func _net_travelled(_from: String, _to: String) -> void:
	$Camera2D.reset_smoothing()
