extends TestCase
## WP19 robustness (docs/PLAN_NETCODE.md 12.4): a predicted player pushing a body must not depend on the
## physics spaces' history. Godot solves pairs and contacts in an order the broadphase layout decides,
## and a client and a server never share that history. Each push scenario runs twice: in the new spaces
## the runner gives every test, and after `used_layout.gd` left the layout in which, before the fix, one
## side's pusher stalled against the body on alternate ticks and the client rolled back nearly every
## tick (22 to 36 rollbacks in the 3D handoff push, 42 and 9 px apart in the 2D simulated push).
##
## The 3D case is the layout this file builds. The 2D one needed the history test_net_body_prediction_motion.gd
## and test_net_physics_simulated.gd left in one process (about 40 s of tests: no small layout found
## reproduces it); there the client's crate tumbled where the server's slid, and the replica, resting
## off the server's crate, was corrected forever. Here the 2D push runs in both layouts as a guard.

const USED := preload("res://tests/fixtures/physics/used_layout.gd")
const MODES := preload("res://tests/integration/test_net_physics_modes.gd")
const BOX_3D := preload("res://tests/fixtures/net3d/box_body_3d.tscn")
const FLOOR_3D := preload("res://tests/fixtures/net3d/floor_3d.tscn")
const PUSHER_3D := preload("res://tests/fixtures/handoff/pusher_3d.tscn")
const CRATE := preload("res://tests/fixtures/physics/crate.tscn")
const FLOOR_2D := preload("res://tests/fixtures/physics/floor.tscn")
const WALKER := preload("res://tests/fixtures/physics/walker.tscn")
## The bound of test_net_physics_handoff_3d.gd's push: fewer than one rollback in 15 ticks.
const ROLLBACKS_3D := 10
## A resting `simulated` replica ends at its server body's pose: half a 0.1 px step per axis.
const SETTLED_2D := 0.075
## The 2D push's rollbacks, from the first contact until the crate rests (about 100 ticks). Both
## crates simulate by themselves, and when they part (one tumbles, the other slides) the pushing
## player meets a different crate on each side until the correction lands: about 12 measured in both
## layouts, against 42 in the storm.
const ROLLBACKS_2D := 20

var net: NetFixture


func after_each() -> void:
	Input.action_release("right")
	if net != null:
		net.teardown()
		net = null


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


func _link(ms: float) -> NetEmulation:
	var e := NetEmulation.new()
	e.send_latency_ms = ms
	e.receive_latency_ms = ms
	e.seed = 19
	return e


func _start(clients: int, mode: NetFixture.HostMode, physics: int) -> Dictionary:
	net = NetFixture.new(self, clients, mode)
	for c in net.clients:
		net.set_emulation(c, _link(50.0))
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers("World", func(m: NetManager) -> void: m.physics = physics)
	var branches: Array[Node] = [net.server]
	branches.append_array(net.clients)
	for k in branches.size():
		var bit := 1 << (1 + k)
		(managers[String(branches[k].name)] as Node).child_entered_tree.connect(func(n: Node) -> void: MODES.layer(n, bit))
	return managers


func _copy(branch: Node, path: String) -> Node:
	assert_true(await net.wait_until(func() -> bool: return branch.has_node(path)), "%s reached %s" % [path, branch.name])
	return branch.get_node(path)


## The 3D handoff push (test_net_physics_handoff_3d.gd's first test, one host mode): {rollbacks, pushed,
## moved}.
func _push_3d(mode: NetFixture.HostMode) -> Dictionary:
	add_child(FLOOR_3D.instantiate())
	var managers := await _start(2, mode, NetEntity.PHYSICS_HANDOFF)
	var world: Node = managers["Server"]
	var a: Node = net.clients[0]
	var sb: RigidBody3D = BOX_3D.instantiate()
	sb.name = "Box"
	sb.position = Vector3(1.0, 0.25, 0.0)
	world.add_child(sb)
	var p: CharacterBody3D = Net.spawn_for(net.peer_id(a), PUSHER_3D, world, "P")
	p.position = Vector3.ZERO
	var ba: RigidBody3D = await _copy(a, "World/Box")
	var pa: CharacterBody3D = await _copy(a, "World/P")
	assert_true(await net.wait_until(func() -> bool: return pa.is_on_floor() and sb.sleeping, 5.0), "everything rests")
	await _frames(10)
	var r0: int = net.session(a).stats()["rollbacks"]
	var x0 := sb.position.x
	Input.action_press("right")
	assert_true(await net.wait_until(func() -> bool: return pa.first_contact_frame >= 0, 3.0), "the pusher reached the box")
	var contact: int = pa.first_contact_frame
	var moved := -1
	for t in 60:
		if moved < 0 and ba.position.x > x0 + 0.0005:
			moved = Engine.get_physics_frames() - contact
		await get_tree().physics_frame
	Input.action_release("right")
	var out := {"rollbacks": int(net.session(a).stats()["rollbacks"]) - r0, "pushed": sb.position.x - x0, "moved": moved}
	net.teardown()
	net = null
	return out


## The 2D simulated crate push (test_net_physics_simulated.gd's): {pushing (rollbacks until the release),
## rollbacks, moved_by, apart, server_x}.
func _push_2d() -> Dictionary:
	add_child(FLOOR_2D.instantiate())
	var managers := await _start(1, NetFixture.HostMode.DEDICATED, NetEntity.PHYSICS_SIMULATED)
	var c: Node = net.clients[0]
	var sc: RigidBody2D = CRATE.instantiate()
	sc.name = "Crate"
	sc.position = Vector2(60, 290)
	managers["Server"].add_child(sc)
	var sw: CharacterBody2D = Net.spawn_for(net.peer_id(c), WALKER, managers["Server"], "W")
	sw.position = Vector2(0, 288)
	var cw: CharacterBody2D = await _copy(c, "World/W")
	var cc: RigidBody2D = await _copy(c, "World/Crate")
	assert_true(await net.wait_until(func() -> bool: return cw.is_on_floor() and cc.sleeping and sc.sleeping, 5.0), "everything rests")
	var r0: int = net.session(c).stats()["rollbacks"]
	var x0 := cc.position.x
	Input.action_press("right")
	assert_true(await net.wait_until(func() -> bool: return cw.first_contact_frame >= 0, 3.0), "the walker reached the crate")
	var moved_by := cc.position.x - x0
	await _frames(40)
	Input.action_release("right")
	var pushing: int = int(net.session(c).stats()["rollbacks"]) - r0
	assert_true(await net.wait_until(func() -> bool: return sc.sleeping and cc.sleeping, 6.0), "the crate rests on both")
	var out := {"pushing": pushing, "rollbacks": int(net.session(c).stats()["rollbacks"]) - r0, "moved_by": moved_by, "apart": cc.position.distance_to(sc.position), "server_x": sc.position.x}
	return out


func _check_3d(layout: String) -> void:
	timeout_sec = 40.0
	if layout == "used":
		await USED.leave(self)
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		var r := await _push_3d(mode)
		print("3D handoff push, %s layout, host mode %d: %d rollbacks, pushed %.3f m, moved %d frame(s) after the contact" % [layout, mode, r["rollbacks"], r["pushed"], r["moved"]])
		assert_lt(r["rollbacks"], ROLLBACKS_3D, "%s layout, mode %d: no rollback storm (%d)" % [layout, mode, r["rollbacks"]])
		assert_gt(r["pushed"], 0.3, "the push moved the box")
		assert_true(r["moved"] >= 0 and r["moved"] <= 2, "on the pusher within two ticks of the contact (%d)" % r["moved"])


func _check_2d(layout: String) -> void:
	timeout_sec = 30.0
	if layout == "used":
		await USED.leave(self)
	var r := await _push_2d()
	print("2D simulated push, %s layout: %d rollbacks (%d while pushing), moved %.2f px in the contact tick, settled %.2f px apart" % [layout, r["rollbacks"], r["pushing"], r["moved_by"], r["apart"]])
	assert_gt(r["moved_by"], 0.0, "the crate moved in the tick of the contact on the client")
	assert_gt(r["server_x"], 61.0, "the server's crate was pushed too")
	assert_lt(r["apart"], SETTLED_2D, "%s layout: the client's crate rests on the server's" % layout)
	assert_lt(r["rollbacks"], ROLLBACKS_2D, "%s layout: no rollback storm (%d)" % [layout, r["rollbacks"]])


func test_handoff_push_3d_in_new_spaces() -> void:
	await _check_3d("new")


func test_handoff_push_3d_in_used_spaces() -> void:
	await _check_3d("used")


func test_simulated_push_2d_in_new_spaces() -> void:
	await _check_2d("new")


func test_simulated_push_2d_in_used_spaces() -> void:
	await _check_2d("used")
