extends TestCase
## Every test starts with new physics spaces (tests/framework/runner.gd `_fresh_worlds`). A space that
## earlier tests emptied is not a new one: its broadphase keeps the layout their bodies left, and the
## same scene then simulates differently in its last bits. That decided whether
## test_net_physics_handoff_3d.gd's push (22 to 36 rollbacks against 4) and
## test_net_physics_simulated.gd's crate push (42 against 2) passed, by which files ran before them in
## the same process. The first test leaves such a layout behind (a floor, two frames later a frozen
## body, both freed together), as test_net_body_prediction_3d.gd's bounce test did with its fixture's
## teardown; the second drops the same box in the world the runner gave it and in a world made for
## it, and expects the same result to the bit. Without `_fresh_worlds` the 3D box ends about 3e-6 m
## and the 2D one 3e-5 px away from the new world's.

const FLOOR_3D := preload("res://tests/fixtures/net3d/floor_3d.tscn")
const BALL_3D := preload("res://tests/fixtures/predict/bouncy_ball_3d.tscn")
const BOX_3D := preload("res://tests/fixtures/net3d/box_body_3d.tscn")
const FLOOR_2D := preload("res://tests/fixtures/predict/floor.tscn")
const BALL_2D := preload("res://tests/fixtures/predict/bouncy_ball.tscn")
const BOX_2D := preload("res://tests/fixtures/physics/crate.tscn")
## Physics frames of each drop.
const FRAMES := 90
## A meta on the root: the physics spaces the first test used (each test is a new instance).
const USED := &"_test_fresh_physics_space_used"


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


## A floor, then (two frames later) a frozen body, then both freed at once: the order that left the
## broadphase in the layout that failed the handoff test.
func test_a_leaves_a_used_broadphase_behind() -> void:
	get_tree().root.set_meta(USED, [get_viewport().find_world_3d().space, get_viewport().find_world_2d().space])
	var branch := Node.new()
	get_tree().root.add_child(branch)
	var holder := Node.new()
	branch.add_child(holder)
	branch.add_child(FLOOR_3D.instantiate())
	branch.add_child(FLOOR_2D.instantiate())
	await wait_frames(2)
	var b3: Node3D = BALL_3D.instantiate()
	b3.position = Vector3(0.0, 2.0, 0.0)
	holder.add_child(b3)
	var b2: Node2D = BALL_2D.instantiate()
	b2.position = Vector2(0.0, -200.0)
	holder.add_child(b2)
	await wait_seconds(1.0)
	branch.free()
	assert_false(is_instance_valid(b3), "the bodies are gone")


## A spinning box dropped on a floor under `parent` (in whatever world it is in): returns the box.
func _drop(parent: Node, three_d: bool) -> Node:
	if three_d:
		parent.add_child(FLOOR_3D.instantiate())
		var b: RigidBody3D = BOX_3D.instantiate()
		b.position = Vector3(0.3, 1.0, 0.2)
		b.angular_velocity = Vector3(1.0, 2.0, 3.0)
		parent.add_child(b)
		return b
	parent.add_child(FLOOR_2D.instantiate())
	var c: RigidBody2D = BOX_2D.instantiate()
	c.position = Vector2(30.0, -100.0)
	c.angular_velocity = 3.0
	parent.add_child(c)
	return c


func _pose(n: Node) -> String:
	if n is Node3D:
		return var_to_str([(n as Node3D).position, (n as Node3D).quaternion])
	return var_to_str([(n as Node2D).position, (n as Node2D).rotation])


func test_b_starts_with_new_physics_spaces() -> void:
	var used: Array = get_tree().root.get_meta(USED, [])
	get_tree().root.remove_meta(USED)
	assert_eq(used.size(), 2, "the first test ran")
	assert_false(used.has(get_viewport().find_world_3d().space), "a new 3D space")
	assert_false(used.has(get_viewport().find_world_2d().space), "a new 2D space")
	var view := SubViewport.new()
	view.own_world_3d = true
	view.world_2d = World2D.new()
	add_child(view)
	var given := Node.new()
	add_child(given)
	var made := Node.new()
	view.add_child(made)
	var bodies: Array = [_drop(given, true), _drop(made, true), _drop(given, false), _drop(made, false)]
	await _frames(FRAMES)
	assert_eq(_pose(bodies[0]), _pose(bodies[1]), "3D: the same drop as in a new world")
	assert_eq(_pose(bodies[2]), _pose(bodies[3]), "2D: the same drop as in a new world")
