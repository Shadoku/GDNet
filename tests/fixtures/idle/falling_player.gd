extends CharacterBody2D
## Test fixture (WP18): a predicted player that walks with "left" / "right" and falls (no floor), so
## an away seat's body, simulated by the server with neutral input, keeps moving.
const SPEED := 200.0
const GRAVITY := 300.0


func _physics_process(delta: float) -> void:
	velocity.x = Net.input(self).get_axis("left", "right") * SPEED
	velocity.y += GRAVITY * delta
	move_and_slide()
