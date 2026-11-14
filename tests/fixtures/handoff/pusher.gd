extends CharacterBody2D
## Test fixture (WP19 handoff): a predicted player that walks and pushes, plain single-player code
## (walker.gd's), walking the way of `direction` when "right" is held, so two of them can walk at each
## other on one key. The counters are for the tests and never change what it does.

const SPEED := 120.0
const GRAVITY := 980.0
## The impulse per tick of sideways contact.
const PUSH := 30.0

## 1: "right" walks right; -1: it walks left.
@export var direction: float = 1.0

## Fresh ticks with a sideways slide collision against a RigidBody2D, and the first one's physics frame.
var contacts: int = 0
var first_contact_frame: int = -1


func _physics_process(delta: float) -> void:
	velocity.x = Net.input(self).get_axis("left", "right") * SPEED * direction
	if is_on_floor():
		velocity.y = 0.0
	else:
		velocity.y += GRAVITY * delta
	move_and_slide()
	for i in get_slide_collision_count():
		var c := get_slide_collision(i)
		var body := c.get_collider() as RigidBody2D
		if body != null and absf(c.get_normal().x) > 0.5:
			body.apply_central_impulse(-c.get_normal() * PUSH)
			if Net.is_fresh():
				contacts += 1
				if first_contact_frame < 0:
					first_contact_frame = Engine.get_physics_frames()
