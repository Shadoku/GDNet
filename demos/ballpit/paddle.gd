class_name BallpitPaddle
extends CharacterBody2D
## Rung 1's predicted player (docs/PLAN_NETCODE.md section 1). Its collision mask holds only the
## walls: the paddle's own motion never depends on the balls, so the owner's prediction is exact,
## while the balls (which do collide with it) are pushed by the server's copy.

const SPEED := 300.0


func _physics_process(_delta: float) -> void:
	velocity = Net.input(self).get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()
