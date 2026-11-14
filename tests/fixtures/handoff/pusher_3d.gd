extends CharacterBody3D
## Test fixture (WP19 handoff): the 3D twin of pusher.gd: walks along x the way of `direction` when
## "right" is held, falls, and gives every RigidBody3D it walks into an impulse (the usual push snippet).
## The counters are for the tests and never change what it does.

## Metres per second.
const SPEED := 1.5
## Metres per second squared.
const GRAVITY := 9.8
## The impulse per tick of sideways contact.
const PUSH := 0.3

## 1: "right" walks toward +x; -1: toward -x.
@export var direction: float = 1.0

var contacts: int = 0
var first_contact_frame: int = -1


func _physics_process(delta: float) -> void:
	velocity.x = Net.input(self).get_axis("left", "right") * SPEED * direction
	velocity.z = 0.0
	if is_on_floor():
		velocity.y = 0.0
	else:
		velocity.y -= GRAVITY * delta
	move_and_slide()
	for i in get_slide_collision_count():
		var c := get_slide_collision(i)
		var body := c.get_collider() as RigidBody3D
		if body != null and absf(c.get_normal().x) > 0.5:
			body.apply_central_impulse(-c.get_normal() * PUSH)
			if Net.is_fresh():
				contacts += 1
				if first_contact_frame < 0:
					first_contact_frame = Engine.get_physics_frames()
