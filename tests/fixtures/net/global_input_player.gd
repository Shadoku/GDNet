extends CharacterBody2D
## Test fixture (WP4): rung 1's "before" player, still reading the global `Input` inside its tick; gdnet
## warns once when an entity with this script gets a client as its owner (D14).
const SPEED := 200.0


func _physics_process(_delta: float) -> void:
	velocity = Input.get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()
