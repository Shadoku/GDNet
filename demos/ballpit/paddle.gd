class_name BallpitPaddle
extends CharacterBody2D
## Rung 1's predicted player (docs/PLAN_NETCODE.md section 1). With `server` balls its collision mask
## holds only the walls: the paddle's own motion never depends on the balls, so the owner's prediction
## is exact, while the balls (which do collide with it) are pushed by the server's copy. With `handoff`
## or `simulated` balls the Spawner adds the balls to the mask, and the paddle pushes what it hits.

const SPEED := 300.0
## The impulse a ball gets per tick of contact (a CharacterBody stops at a RigidBody; it does not push).
const PUSH := 30.0


func _physics_process(_delta: float) -> void:
	velocity = Net.input(self).get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()
	for i in get_slide_collision_count():
		var hit := get_slide_collision(i)
		var ball := hit.get_collider() as RigidBody2D
		if ball != null:
			ball.apply_central_impulse(-hit.get_normal() * PUSH)
