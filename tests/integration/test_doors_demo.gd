extends TestCase
## WP13 and WP16 (docs/PLAN_NETCODE.md 2.11 and 12.2, gates G2, G8 and G9): the Doors demo
## (`demos/doors/`) under NetFixture, in both host modes: a 2 x 2 chunk overworld walked across without
## a transition, interiors behind doors placed apart by `gdnet/levels/spacing` (project.godot), torches
## with a radius, a chest whose state the cave keeps, the chat across rooms, a late joiner, and the
## README's listings against the files.

const WORLD := preload("res://demos/doors/world.gd")
const CHAT := preload("res://demos/doors/chat.tscn")
const C00 := "res://demos/doors/chunk_0_0.tscn"
const C10 := "res://demos/doors/chunk_1_0.tscn"
const C01 := "res://demos/doors/chunk_0_1.tscn"
const C11 := "res://demos/doors/chunk_1_1.tscn"
const CAVE := "res://demos/doors/cave.tscn"
const GARDEN := "res://demos/doors/garden.tscn"
const CHUNKS: Array[String] = ["chunk_0_0", "chunk_0_1", "chunk_1_0", "chunk_1_1"]
const TORCH_RADIUS := 400.0

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
	e.seed = 5
	return e


## A World (and, with `chat`, a Chat) in every branch; the server's World starts.
func _setup(f: NetFixture, branches: Array, chat: bool = false) -> void:
	for branch: Node in branches:
		var w: DoorsWorld = WORLD.new()
		w.name = "World"
		w.dedicated = f.host_mode == NetFixture.HostMode.DEDICATED
		branch.add_child(w)
		if chat:
			var c: Node = CHAT.instantiate()
			c.name = "Chat"
			branch.add_child(c)
	f.server.get_node("World").start()
	if chat:
		for branch: Node in branches:
			branch.get_node("Chat").start()


func _player(branch: Node, id: int) -> Node2D:
	return branch.get_node("World").find_child("Player%d" % id, true, false) as Node2D


func _level_of(branch: Node, id: int) -> String:
	var p := _player(branch, id)
	return "" if p == null else String(p.get_parent().get_parent().name)


## The level nodes under a branch's World, sorted.
func _levels(branch: Node) -> Array:
	var out := []
	for c in branch.get_node("World").get_children():
		if not c.is_queued_for_deletion():
			out.append(String(c.name))
	out.sort()
	return out


## True when `branch` holds the entity at `path` (under its World).
func _holds(branch: Node, path: String) -> bool:
	var w: Node = branch.get_node("World")
	return w.has_node(path) and Net.net_id(w.get_node(path)) > 0


## The server sends player `id` through the door `door` (a node path under the server's World).
func _walk(f: NetFixture, id: int, door: String) -> void:
	f.server.get_node("World/" + door).body_entered.emit(_player(f.server, id))


func _everyone_in(f: NetFixture, branches: Array, level: String) -> bool:
	for branch: Node in branches:
		for id in f.players():
			if _level_of(branch, id) != level:
				return false
	return true


func test_doors_lead_to_placed_interiors_and_players_meet_again() -> void:
	var ok := await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var cb: Node = f.clients[1]
		var a := f.peer_id(ca)
		var b := f.peer_id(cb)
		_setup(f, [f.server, ca, cb], true)
		assert_true(await f.wait_until(func() -> bool: return _everyone_in(f, [f.server, ca, cb], "chunk_0_0")), "every player starts at the crossroads everywhere")
		assert_true(await f.wait_until(func() -> bool: return _levels(ca) == CHUNKS), "a client holds its chunk and the three neighbours")
		var hops := []
		f.session(ca).travelled.connect(func(_p: Node, from: String, to: String) -> void: hops.append([from, to]))
		_walk(f, a, "chunk_0_0/CaveDoor")
		assert_true(await f.wait_until(func() -> bool: return _level_of(ca, a) == "cave" and _levels(ca) == ["cave"]), "A is in the cave and holds only the cave")
		assert_eq(_level_of(f.server, a), "cave")
		assert_has(f.server_session.levels(), CAVE)
		assert_has(f.server_session.levels(), C00, "B keeps the overworld loaded on the server")
		assert_eq(hops, [[C00, CAVE]])
		assert_eq(f.server_session.level_origin(CAVE), Vector2(4000, 0), "the cave takes the first cell of the spacing grid")
		assert_eq(f.session(ca).level_origin(CAVE), Vector2(4000, 0), "the same on the client")
		assert_true(await f.wait_until(func() -> bool:
			return _player(ca, a).global_position.distance_to(Vector2(4000, 100)) < 0.5), "A arrived at the cave's Entrance, placed")
		assert_true(await f.wait_until(func() -> bool:
			return _holds(ca, "cave/NetManager/CaveTorch") and _holds(ca, "cave/NetManager/Chest")), "the cave's entities reached A")
		assert_true(await f.wait_until(func() -> bool: return _player(cb, a) == null and _player(ca, b) == null), "A and B left each other's scope")
		for row: Dictionary in f.server_session.overlay_rows():
			assert_true(row["room"] in [C00, C10, C01, C11, CAVE], "overlay row room %s" % row["room"])
		ca.get_node("Chat").send("hello from the cave")
		assert_true(await f.wait_until(func() -> bool: return cb.get_node("Chat/Box/Log").get_parsed_text().contains("hello from the cave")), "chat crosses rooms")
		_walk(f, b, "chunk_0_0/CaveDoor")
		assert_true(await f.wait_until(func() -> bool: return _level_of(ca, b) == "cave" and _level_of(cb, a) == "cave"), "A and B meet in the cave")
		assert_eq(_levels(cb), ["cave"])
		_walk(f, a, "cave/ReturnDoor")
		assert_true(await f.wait_until(func() -> bool: return _level_of(ca, a) == "chunk_0_0" and _levels(ca) == CHUNKS), "back out: the overworld again")
		assert_eq(_player(f.server, a).global_position, (f.server.get_node("World/chunk_0_0/FromCave") as Node2D).global_position)
	)
	assert_true(ok, "fixtures connected")


## Gate G9 (rooms): every player walks right from the crossroads into the next chunk over a 30 ms link.
## Every peer has both chunks, so every copy is moved, not freed, and nothing rolls back.
func test_g9_crossing_a_chunk_edge_frees_no_replica() -> void:
	var ok := await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		for c: Node in f.clients:
			f.set_emulation(c, _link(30.0))
		var ca: Node = f.clients[0]
		var a := f.peer_id(ca)
		var branches: Array = [f.server] + f.clients
		_setup(f, branches)
		assert_true(await f.wait_until(func() -> bool: return _everyone_in(f, branches, "chunk_0_0") and _levels(ca) == CHUNKS), "every player at the crossroads everywhere")
		await _frames(60)
		var ids := {}
		for branch: Node in branches:
			for id in f.players():
				ids["%s/%d" % [branch.name, id]] = _player(branch, id).get_instance_id()
		var rolled := []
		for c: Node in f.clients:
			f.session(c).rolled_back.connect(func(n: Node, from_tick: int, to_tick: int) -> void: rolled.append([c.name, n.name, from_tick, to_tick]))
		var freed := []
		for c: Node in f.clients:
			f.session(c).entity_despawning.connect(func(n: Node) -> void:
				if n is DoorsPlayer:
					freed.append([c.name, n.name]))
		var hops := []
		var own_a := _player(ca, a)
		f.session(ca).travelled.connect(func(p: Node, from: String, to: String) -> void:
			if p == own_a:
				hops.append([from, to]))
		var last := own_a.global_position
		var max_step := 0.0
		Input.action_press("right")
		for i in 36:
			await get_tree().physics_frame
			max_step = maxf(max_step, own_a.global_position.distance_to(last))
			last = own_a.global_position
		Input.action_release("right")
		assert_true(await f.wait_until(func() -> bool: return _everyone_in(f, branches, "chunk_1_0")), "every player crossed into chunk_1_0 on every peer")
		for branch: Node in branches:
			for id in f.players():
				assert_eq(_player(branch, id).get_instance_id(), ids["%s/%d" % [branch.name, id]], "%s's copy of %d was moved, not recreated" % [branch.name, id])
		assert_eq(freed, [], "no player replica was freed")
		assert_eq(rolled, [], "the crossing caused no rollback")
		assert_eq(hops, [[C00, C10]], "A's client saw its player travel once")
		assert_lt(max_step, DoorsPlayer.SPEED / Engine.physics_ticks_per_second * 2.0 + 0.5, "the owner's position is continuous")
		assert_eq(_levels(ca), CHUNKS, "the four chunks stay loaded")
		assert_eq(f.server_session.room_of(a), C10)
	)
	assert_true(ok, "fixtures connected")


## A client at the crossroads holds the four chunks (the boulder of the next chunk included, so it
## collides with it before entering) but not an interior another player is in; of the torches, only
## those within 1.2 times their radius.
func test_a_client_holds_its_neighbours_and_only_the_near_torches() -> void:
	var ok := await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var a := f.peer_id(ca)
		var b := f.peer_id(f.clients[1])
		_setup(f, [f.server, ca, f.clients[1]])
		assert_true(await f.wait_until(func() -> bool: return _levels(ca) == CHUNKS and _level_of(ca, a) == "chunk_0_0"))
		_walk(f, b, "chunk_1_1/GardenDoor")
		assert_true(await f.wait_until(func() -> bool: return _level_of(f.clients[1], b) == "garden"), "B is in the garden")
		await _frames(10)
		assert_has(f.server_session.levels(), GARDEN)
		assert_eq(f.server_session.level_origin(GARDEN), Vector2(4000, 0), "placed in the first free cell")
		assert_eq(_levels(ca), CHUNKS, "A holds the four chunks and not the garden")
		assert_true(ca.has_node("World/chunk_1_0/Boulder"), "the next chunk's geometry is there before A enters it")
		var me: Vector2 = _player(f.server, a).global_position
		var near := 0
		var far := 0
		for chunk in CHUNKS:
			for t: Node2D in f.server.get_node("World/%s/NetManager" % chunk).get_children():
				if not t.name.begins_with("Torch"):
					continue
				var d := t.global_position.distance_to(me)
				var held := _holds(ca, "%s/NetManager/%s" % [chunk, t.name])
				if d <= TORCH_RADIUS:
					assert_true(held, "%s/%s at %.0f px is held" % [chunk, t.name, d])
					near += 1
				elif d > TORCH_RADIUS * 1.2:
					assert_false(held, "%s/%s at %.0f px is not held" % [chunk, t.name, d])
					far += 1
		assert_eq(near, 4, "four torches near the crossroads")
		assert_eq(far, 20, "and twenty beyond 1.2 r")
	)
	assert_true(ok, "fixtures connected")


## The chest in the cave stays open after the cave unloads and loads again (the level keeps its
## state), on the server and for the client that comes back.
func test_the_chest_stays_open_across_an_unload_and_reload() -> void:
	var ok := await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var a := f.peer_id(ca)
		f.server_session.empty_level_timeout = 0.5
		_setup(f, [f.server, ca])
		assert_true(await f.wait_until(func() -> bool: return _everyone_in(f, [f.server, ca], "chunk_0_0")))
		for id in f.players():
			_walk(f, id, "chunk_0_0/CaveDoor")
		assert_true(await f.wait_until(func() -> bool: return _holds(ca, "cave/NetManager/Chest")))
		var chest: DoorsChest = f.server.get_node("World/cave/NetManager/Chest")
		chest.body_entered.emit(_player(f.server, a))
		assert_true(chest.open, "a player opened the chest")
		assert_true(await f.wait_until(func() -> bool: return ca.get_node("World/cave/NetManager/Chest").open), "the client sees it open")
		assert_true(await f.wait_until(func() -> bool: return not Array(f.server_session.levels()).has(C00)), "the empty overworld unloads")
		for id in f.players():
			_walk(f, id, "cave/ReturnDoor")
		assert_true(await f.wait_until(func() -> bool: return not Array(f.server_session.levels()).has(CAVE)), "now the cave unloads")
		assert_has(Array(f.server_session.kept_levels()), CAVE, "and the server keeps its state")
		assert_true(await f.wait_until(func() -> bool: return _levels(ca) == CHUNKS))
		_walk(f, a, "chunk_0_0/CaveDoor")
		assert_true(await f.wait_until(func() -> bool: return _holds(ca, "cave/NetManager/Chest")), "the cave loads again")
		var again: DoorsChest = f.server.get_node("World/cave/NetManager/Chest")
		assert_ne(again, chest, "a new chest node")
		assert_true(again.open, "still open on the server")
		var copy: DoorsChest = ca.get_node("World/cave/NetManager/Chest")
		assert_true(copy.open, "and in the client's Create")
		assert_eq((copy.get_node("Lid") as Polygon2D).color, DoorsChest.OPENED, "its lid shows it")
	)
	assert_true(ok, "fixtures connected")


func test_a_late_joiner_into_the_cave_gets_only_the_cave() -> void:
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		net = NetFixture.new(self, 3, mode)
		net.start_server()
		for i in 2:
			net.client_sessions[i].start(net.peer(net.clients[i]), net.clients[i].get_path())
		assert_true(await net.wait_until(func() -> bool: return net.server_session.peers().size() == 2 and net.client_sessions[1].is_online()))
		var ca: Node = net.clients[0]
		var cc: Node = net.clients[2]
		var a := net.peer_id(ca)
		_setup(net, [net.server, ca, net.clients[1], cc])
		assert_true(await net.wait_until(func() -> bool: return _level_of(ca, a) == "chunk_0_0"))
		_walk(net, a, "chunk_0_0/CaveDoor")
		assert_true(await net.wait_until(func() -> bool: return _level_of(ca, a) == "cave"))
		(net.server.get_node("World") as DoorsWorld).start_level = CAVE
		net.client_sessions[2].start(net.peer(cc), cc.get_path())
		var c := net.peer_id(cc)
		assert_true(await net.wait_until(func() -> bool: return _level_of(cc, c) == "cave" and _level_of(cc, a) == "cave"), "C starts in the cave and sees A")
		await wait_frames(10)
		assert_eq(_levels(cc), ["cave"])
		assert_true(_holds(cc, "cave/NetManager/CaveTorch") and _holds(cc, "cave/NetManager/Chest"))
		assert_null(_player(cc, net.peer_id(net.clients[1])), "B (in the overworld) is not in C's scope")
		assert_null(_player(cc, 1), "nor is the host's player")
		assert_eq(net.session(cc).entity_count(), 4, "A, C, the cave torch and the chest")
		net.teardown()
		net = null


func test_g8_the_readme_listings_are_the_files() -> void:
	var readme := FileAccess.get_file_as_string("res://demos/doors/README.md")
	assert_false(readme.is_empty(), "the README exists")
	var listed := {}
	var lines := readme.split("\n")
	var i := 0
	while i < lines.size():
		var line := lines[i]
		if line.begins_with("```") and line.contains(" file="):
			var path := line.get_slice("file=", 1).strip_edges()
			var body := PackedStringArray()
			i += 1
			while i < lines.size() and not lines[i].begins_with("```"):
				body.append(lines[i])
				i += 1
			var on_disk := FileAccess.get_file_as_string("res://" + path).strip_edges(false, true)
			assert_eq("\n".join(body), on_disk, "the README's listing of %s" % path)
			listed[path] = true
		i += 1
	for f in DirAccess.get_files_at("res://demos/doors"):
		if f.ends_with(".gd") or f.ends_with(".tscn"):
			assert_true(listed.has("demos/doors/" + f), "the README lists %s" % f)
