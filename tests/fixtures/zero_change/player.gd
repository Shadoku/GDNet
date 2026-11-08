extends CharacterBody2D
## A plain single-player character: it wanders in a slow circle on its own (no input), playing its
## walk animation. Gate G7's fixture (docs/PLAN_NETCODE.md 5, 6): no multiplayer code here.

const SPEED := 60.0


func _ready() -> void:
	velocity = Vector2.RIGHT.rotated(randf() * TAU) * SPEED


func _physics_process(_delta: float) -> void:
	velocity = velocity.rotated(0.02)
	move_and_slide()
