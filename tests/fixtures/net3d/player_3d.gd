extends CharacterBody3D
## Test fixture (WP18): rung 1's player in 3D: a plain `_physics_process` reading `Net.input`, walking on
## the x/z plane, falling under gravity and jumping, with `move_and_slide()`. The counters are for the
## tests and never change what the player does.

## Metres per second on the ground.
const SPEED := 4.0
## The jump's upward speed, metres per second.
const JUMP_SPEED := 4.5
## Metres per second squared.
const GRAVITY := 9.8

## Every `_physics_process` call, fresh or replayed.
var physics_calls: int = 0
## `_physics_process` calls inside a replay (`Net.is_fresh()` false).
var replayed_calls: int = 0
## Fresh ticks on which a jump started.
var jumps: int = 0


func _physics_process(delta: float) -> void:
	var input := Net.input(self)
	var dir := input.get_vector("left", "right", "up", "down")
	velocity.x = dir.x * SPEED
	velocity.z = dir.y * SPEED
	if dir != Vector2.ZERO:
		# Face the way it walks (-Z is forward).
		rotation.y = atan2(-dir.x, -dir.y)
	if is_on_floor():
		if input.is_action_just_pressed("jump"):
			velocity.y = JUMP_SPEED
			if Net.is_fresh():
				jumps += 1
	else:
		velocity.y -= GRAVITY * delta
	move_and_slide()
	physics_calls += 1
	if not Net.is_fresh():
		replayed_calls += 1
