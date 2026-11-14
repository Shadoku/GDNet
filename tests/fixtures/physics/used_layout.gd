extends RefCounted
## Test fixture (WP19): leaves the physics spaces of `test`'s world with history, as a real game's
## always have. The runner gives every test new spaces (tests/framework/runner.gd `_fresh_worlds`); this
## builds, in those, the broadphase layout that test_fresh_physics_space.gd's first test leaves (a
## floor, two frames later a frozen body, both freed together, in 3D and 2D), the layout in which the
## solver's order of pairs and contacts made a predicted player's push stall on alternate ticks on one
## side only (WP19's robustness work).

const FLOOR_3D := preload("res://tests/fixtures/net3d/floor_3d.tscn")
const BALL_3D := preload("res://tests/fixtures/predict/bouncy_ball_3d.tscn")
const FLOOR_2D := preload("res://tests/fixtures/predict/floor.tscn")
const BALL_2D := preload("res://tests/fixtures/predict/bouncy_ball.tscn")


static func leave(test: TestCase) -> void:
	var branch := Node.new()
	test.get_tree().root.add_child(branch)
	var holder := Node.new()
	branch.add_child(holder)
	branch.add_child(FLOOR_3D.instantiate())
	branch.add_child(FLOOR_2D.instantiate())
	await test.wait_frames(2)
	var b3: Node3D = BALL_3D.instantiate()
	b3.position = Vector3(0.0, 2.0, 0.0)
	holder.add_child(b3)
	var b2: Node2D = BALL_2D.instantiate()
	b2.position = Vector2(0.0, -200.0)
	holder.add_child(b2)
	await test.wait_seconds(1.0)
	branch.free()
