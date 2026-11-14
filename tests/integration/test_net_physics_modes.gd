extends TestCase
## WP19 slice A (docs/PLAN_NETCODE.md 12.4): the `physics` option and the `owner` mode, in 2D. The
## option resolves entity, then manager, then project; `configure()` sets it; a change on the server
## reaches every peer; an untouched `handoff` body is the server's. `owner` is `client_owned` for
## bodies (D21): the owner's body is live and moves on the tick of its input, everyone else's copy is
## frozen and follows; ownership changes move the simulation; a dropped seat's body freezes in place.
##
## The branches share one physics space: every body that enters a branch's manager moves to that
## branch's own collision layer (bit 1 + branch index; the floor is layer 1), so copies never touch.

const CRATE := preload("res://tests/fixtures/physics/crate.tscn")
const FLOOR := preload("res://tests/fixtures/physics/floor.tscn")
const VEHICLE := preload("res://tests/fixtures/physics/vehicle.tscn")
const MOVER := preload("res://tests/fixtures/net/mover.tscn")
## A replica's position is within half a 0.1 px step of the value sent (D17); interpolation lands on
## the last sample exactly.
const SETTLED := 0.1

var net: NetFixture


func after_each() -> void:
	Input.action_release("right")
	ProjectSettings.set_setting("gdnet/physics/mode", null)
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


## Every body or character entering a branch's manager goes to that branch's layer.
static func layer(n: Node, bit: int) -> void:
	if n is RigidBody2D:
		(n as CollisionObject2D).collision_layer = bit
		(n as CollisionObject2D).collision_mask = 1 | bit
	elif n is CharacterBody2D:
		(n as CollisionObject2D).collision_layer = 0
		(n as CollisionObject2D).collision_mask = 1 | bit
	elif n is RigidBody3D:
		(n as CollisionObject3D).collision_layer = bit
		(n as CollisionObject3D).collision_mask = 1 | bit
	elif n is CharacterBody3D:
		(n as CollisionObject3D).collision_layer = 0
		(n as CollisionObject3D).collision_mask = 1 | bit


func _managers(f: NetFixture, configure: Callable = Callable()) -> Dictionary:
	var managers := f.add_managers("World", configure)
	var branches: Array[Node] = [f.server]
	branches.append_array(f.clients)
	for k in branches.size():
		var bit := 1 << (1 + k)
		(managers[String(branches[k].name)] as Node).child_entered_tree.connect(func(n: Node) -> void: layer(n, bit))
	return managers


## A crate under `world`, with a `NetEntity` whose `physics` is `physics` when it is given.
func _crate(world: Node, crate_name: String, at: Vector2, physics: int = -1) -> RigidBody2D:
	var c: RigidBody2D = CRATE.instantiate()
	c.name = crate_name
	c.position = at
	if physics >= 0:
		var ne := NetEntity.new()
		ne.name = "NetEntity"
		ne.inherit_profile = true
		ne.physics = physics
		c.add_child(ne)
	world.add_child(c)
	return c


func _copy(f: NetFixture, branch: Node, path: String) -> Node:
	assert_true(await f.wait_until(func() -> bool: return branch.has_node(path)), "%s reached %s" % [path, branch.name])
	return branch.get_node(path)


# --- the option -----------------------------------------------------------------------------------


func test_option_resolves_entity_manager_project_and_changes_reach_every_peer() -> void:
	timeout_sec = 20.0
	add_child(FLOOR.instantiate())
	net = NetFixture.new(self, 2)
	net.start()
	assert_true(await net.wait_connected())
	var managers := _managers(net, func(m: NetManager) -> void: m.physics = NetEntity.PHYSICS_SIMULATED)
	var world: Node = managers["Server"]
	var by_manager := _crate(world, "ByManager", Vector2(0, 280))
	var by_entity := _crate(world, "ByEntity", Vector2(40, 280), NetEntity.PHYSICS_OWNER)
	var by_configure := _crate(world, "ByConfigure", Vector2(80, 280))
	Net.configure(by_configure, {"physics": "server"})
	# An entity without a body ignores it.
	var mover: Node2D = MOVER.instantiate()
	mover.name = "Mover"
	var mne := NetEntity.new()
	mne.name = "NetEntity"
	mne.inherit_profile = true
	mne.physics = NetEntity.PHYSICS_OWNER
	mover.add_child(mne)
	world.add_child(mover)
	for c in net.clients:
		for n in ["ByManager", "ByEntity", "ByConfigure", "Mover"]:
			await _copy(net, c, "World/" + n)
	await _frames(3)
	var expect := {"ByManager": "simulated", "ByEntity": "owner", "ByConfigure": "server", "Mover": ""}
	for n: String in expect:
		assert_eq(String(Net.physics_of(world.get_node(n))), expect[n], "server: %s" % n)
		for c in net.clients:
			assert_eq(String(Net.physics_of(c.get_node("World/" + n))), expect[n], "%s: %s" % [c.name, n])
	assert_false((mne as NetEntity).client_owned, "a body-less entity is not made client_owned")
	assert_eq(int(net.session(net.clients[0]).overlay_rows().filter(func(r: Dictionary) -> bool: return r["physics"] == "owner")[0]["simulated_by"]), 1, "an `owner` body the server owns is simulated by the server")
	# Who has a live body: the server all three (it owns them); a client only the simulated one.
	for n in ["ByManager", "ByEntity", "ByConfigure"]:
		assert_false((world.get_node(n) as RigidBody2D).freeze, "the server simulates %s" % n)
	for c in net.clients:
		assert_false((c.get_node("World/ByManager") as RigidBody2D).freeze, "a simulated replica is live")
		assert_true((c.get_node("World/ByEntity") as RigidBody2D).freeze, "a server-owned `owner` body is frozen on clients")
		assert_true((c.get_node("World/ByConfigure") as RigidBody2D).freeze, "a `server` body is frozen on clients")
	# A change on the server reaches every peer: Net.set_physics, the property, the manager's default.
	Net.set_physics(by_configure, "simulated")
	(by_entity.get_node("NetEntity") as NetEntity).physics = NetEntity.PHYSICS_SERVER
	ProjectSettings.set_setting("gdnet/physics/mode", "owner")
	(world as NetManager).physics = NetEntity.PHYSICS_DEFAULT
	var after := {"ByManager": "owner", "ByEntity": "server", "ByConfigure": "simulated"}
	assert_true(await net.wait_until(func() -> bool:
		for c in net.clients:
			for n: String in after:
				if String(Net.physics_of(c.get_node("World/" + n))) != after[n]:
					return false
		return true), "every client follows: %s" % [net.clients.map(func(c: Node) -> Array: return after.keys().map(func(n: String) -> String: return Net.physics_of(c.get_node("World/" + n))))])
	await _frames(2)
	for c in net.clients:
		assert_false((c.get_node("World/ByConfigure") as RigidBody2D).freeze, "now live on %s" % c.name)
		assert_true((c.get_node("World/ByManager") as RigidBody2D).freeze, "now frozen on %s" % c.name)
	# `handoff`: a body nobody touches is simulated by the server and frozen on the clients
	# (test_net_physics_handoff.gd covers the rest).
	Net.set_physics(by_manager, NetEntity.PHYSICS_HANDOFF)
	assert_true(await net.wait_until(func() -> bool: return net.clients.all(func(c: Node) -> bool: return Net.physics_of(c.get_node("World/ByManager")) == "handoff")), "handoff reached the clients")
	await _frames(2)
	assert_false(by_manager.freeze, "the server simulates a handoff body")
	for c in net.clients:
		assert_true((c.get_node("World/ByManager") as RigidBody2D).freeze, "frozen on %s, as `server`" % c.name)


# --- owner ------------------------------------------------------------------------------------------


## The vehicle under the server's manager, owned by `owner`, with `physics = owner` or, with
## `spelling` "client_owned", `client_owned = true` (D21: the same behaviour).
func _vehicle(world: Node, owner: int, spelling: String, vehicle_name: String = "V") -> RigidBody2D:
	var v: RigidBody2D = VEHICLE.instantiate()
	v.name = vehicle_name
	v.position = Vector2(0, 100)
	var ne := NetEntity.new()
	ne.name = "NetEntity"
	ne.inherit_profile = true
	ne.owner_peer = owner
	if spelling == "physics":
		ne.physics = NetEntity.PHYSICS_OWNER
	else:
		ne.client_owned = true
	v.add_child(ne)
	world.add_child(v)
	return v


func _owner_case(f: NetFixture, managers: Dictionary, spelling: String) -> Dictionary:
	var a: Node = f.clients[0]
	var b: Node = f.clients[1]
	var sv := _vehicle(managers["Server"], f.peer_id(a), spelling, spelling)
	var va: RigidBody2D = await _copy(f, a, "World/" + spelling)
	var vb: RigidBody2D = await _copy(f, b, "World/" + spelling)
	await _frames(30)
	assert_false(va.freeze, "%s: the owner's body is live" % spelling)
	assert_true(vb.freeze, "%s: another client's copy is frozen" % spelling)
	assert_true(sv.freeze, "%s: the server's copy is frozen" % spelling)
	assert_true(f.session(a).overlay_rows().any(func(r: Dictionary) -> bool: return r["physics"] == "owner" and int(r["simulated_by"]) == f.peer_id(a)), "%s: the overlay names the simulating peer" % spelling)
	var start := va.position
	Input.action_press("right")
	await get_tree().physics_frame
	assert_gt(va.linear_velocity.x, 0.0, "%s: the owner's body moves on the tick of the input" % spelling)
	var lat := {"server": -1, "other": -1}
	for t in 60:
		if lat["server"] < 0 and sv.position.x > start.x + 0.5:
			lat["server"] = t
		if lat["other"] < 0 and vb.position.x > start.x + 0.5:
			lat["other"] = t
		await get_tree().physics_frame
	var owner_moved := va.position.x - start.x
	assert_gt(owner_moved, 20.0, "%s: the owner drove" % spelling)
	Input.action_release("right")
	var settled := func() -> bool:
		return absf(va.position.x - sv.position.x) < SETTLED and absf(va.position.x - vb.position.x) < SETTLED and va.linear_velocity.length() < 1.0
	assert_true(await f.wait_until(settled, 8.0), "%s: the server and the other client converge on the owner (owner %s, server %s, other %s)" % [spelling, va.position, sv.position, vb.position])
	assert_true(sv.freeze and vb.freeze and not va.freeze, "%s: still simulated by the owner only" % spelling)
	sv.queue_free()
	assert_true(await f.wait_until(func() -> bool: return not a.has_node("World/" + spelling) and not b.has_node("World/" + spelling)), "%s: gone" % spelling)
	return {"server": lat["server"], "other": lat["other"], "moved": owner_moved}


func test_owner_body_moves_on_the_tick_of_input_converges_and_equals_client_owned() -> void:
	timeout_sec = 40.0
	var results := {}
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		for c in f.clients:
			f.set_emulation(c, _link(50.0))
		var managers := _managers(f)
		for spelling in ["physics", "client_owned"]:
			results["%d %s" % [f.host_mode, spelling]] = await _owner_case(f, managers, spelling))
	assert_true(ok)
	for k: String in results:
		print("owner mode (%s): first motion on the server %d ticks after the press, on the other client %d; owner moved %.1f px in 60 ticks" % [k, results[k]["server"], results[k]["other"], results[k]["moved"]])
	for mode in [0, 1]:
		var p: Dictionary = results["%d physics" % mode]
		var c: Dictionary = results["%d client_owned" % mode]
		assert_almost_eq(p["moved"], c["moved"], 0.5, "physics = owner and client_owned drive alike (mode %d)" % mode)


func test_owner_ownership_changes_move_the_simulation_and_a_dropped_seat_freezes() -> void:
	timeout_sec = 20.0
	net = NetFixture.new(self, 2)
	net.start()
	assert_true(await net.wait_connected())
	var managers := _managers(net)
	var a: Node = net.clients[0]
	var b: Node = net.clients[1]
	var aid := net.peer_id(a)
	var bid := net.peer_id(b)
	var sv := _vehicle(managers["Server"], aid, "physics")
	var va: RigidBody2D = await _copy(net, a, "World/V")
	var vb: RigidBody2D = await _copy(net, b, "World/V")
	await _frames(10)
	Input.action_press("right")
	await _frames(30)
	Input.action_release("right")
	# A to B while it moves: A's copy freezes, B's thaws with the velocity it was showing.
	assert_true(Net.set_owner(sv, bid))
	assert_true(await net.wait_until(func() -> bool: return va.freeze and not vb.freeze), "B simulates it now")
	assert_gt(vb.linear_velocity.x, 10.0, "B took over the velocity (%s)" % vb.linear_velocity)
	assert_true(sv.freeze, "the server's copy stays frozen")
	await _frames(20)
	# Back to the server: the server simulates it, every client has it frozen.
	assert_true(Net.set_owner(sv, 1))
	assert_true(await net.wait_until(func() -> bool: return not sv.freeze and va.freeze and vb.freeze), "the server simulates it")
	assert_true(await net.wait_until(func() -> bool: return sv.linear_velocity.length() < 1.0, 6.0), "it coasts to a stop on the server")
	assert_true(await net.wait_until(func() -> bool: return absf(vb.position.x - sv.position.x) < SETTLED), "B follows the server")
	# To A again, then A drops: the seat is held and the body freezes in place on the server.
	assert_true(Net.set_owner(sv, aid))
	assert_true(await net.wait_until(func() -> bool: return not va.freeze and sv.freeze), "A simulates it again")
	Input.action_press("right")
	await _frames(15)
	Input.action_release("right")
	net.drop_client(0)
	await _frames(20)
	var held := sv.position
	await _frames(30)
	assert_true(sv.freeze, "a dropped seat's body stays frozen on the server")
	assert_eq(sv.position, held, "and in place")
	assert_true(vb.freeze, "and on the other client")
