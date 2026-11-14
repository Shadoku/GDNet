extends CharacterBody2D
## Test fixture (WP19): a predicted player (rung 1's movement) that records the first body it walks
## into on a fresh tick: where that body was then, and where `reference` (the server's copy of that
## body, set by the test) was at the same moment.
const SPEED := 200.0

var reference: Node2D = null
var touched: Node2D = null
var touched_at: Vector2 = Vector2.INF
var reference_at: Vector2 = Vector2.INF


func _physics_process(_delta: float) -> void:
	velocity = Net.input(self).get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()
	if touched != null or not Net.is_fresh():
		return
	for i in get_slide_collision_count():
		var c := get_slide_collision(i).get_collider() as Node2D
		if c != null and c is RigidBody2D:
			touched = c
			touched_at = c.position
			if reference != null:
				reference_at = reference.position
			return
