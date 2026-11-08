extends TestCase
## WP13 (docs/PLAN_NETCODE.md 2.11, gates G2 and G8): the Doors demo (`demos/doors/`) under NetFixture,
## in both host modes: travel between levels, interest by room, the chat across rooms, a late joiner,
## the empty-level timeout, and the README's listings against the files.

const WORLD := preload("res://demos/doors/world.gd")
const CHAT := preload("res://demos/doors/chat.tscn")
const HUB := "res://demos/doors/hub.tscn"
const CAVE := "res://demos/doors/cave.tscn"

var net: NetFixture


func after_each() -> void:
	if net != null:
		net.teardown()
		net = null


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


func _player(branch: Node, id: int) -> Node:
	return branch.get_node("World").find_child("Player%d" % id, true, false)


func _level_of(branch: Node, id: int) -> String:
	var p := _player(branch, id)
	return "" if p == null else String(p.get_parent().get_parent().name)


func _levels(branch: Node) -> Array:
	var out := []
	for c in branch.get_node("World").get_children():
		if not c.is_queued_for_deletion():
			out.append(String(c.name))
	return out


## The server sends player `id` through the door `door` (a node path under the server's World).
func _walk(f: NetFixture, id: int, door: String) -> void:
	f.server.get_node("World/" + door).body_entered.emit(_player(f.server, id))


func _everyone_in_hub(f: NetFixture, branches: Array) -> bool:
	for branch: Node in branches:
		for id in f.players():
			if _level_of(branch, id) != "hub":
				return false
	return true


func test_travel_moves_scope_and_players_meet_again() -> void:
	var ok := await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var cb: Node = f.clients[1]
		var a := f.peer_id(ca)
		var b := f.peer_id(cb)
		_setup(f, [f.server, ca, cb], true)
		assert_true(await f.wait_until(func() -> bool: return _everyone_in_hub(f, [f.server, ca, cb])), "every player is in the hub everywhere")
		assert_eq(_levels(ca), ["hub"])
		var hops := []
		f.session(ca).travelled.connect(func(_p: Node, from: String, to: String) -> void: hops.append([from, to]))
		_walk(f, a, "hub/CaveDoor")
		assert_true(await f.wait_until(func() -> bool: return _level_of(ca, a) == "cave" and _levels(ca) == ["cave"]), "A is in the cave and holds only the cave")
		assert_eq(_level_of(f.server, a), "cave")
		assert_has(f.server_session.levels(), HUB)
		assert_has(f.server_session.levels(), CAVE)
		assert_eq(hops, [[HUB, CAVE]])
		assert_true(ca.has_node("World/cave/NetManager/CaveTorch"), "the cave's own entity reached A")
		assert_true(await f.wait_until(func() -> bool: return _player(cb, a) == null and _player(ca, b) == null), "A and B left each other's scope")
		assert_ne(f.session(ca).stats()["entities_in_scope"], f.session(cb).stats()["entities_in_scope"])
		for row: Dictionary in f.server_session.overlay_rows():
			assert_true(row["room"] in [HUB, CAVE], "overlay row room %s" % row["room"])
		ca.get_node("Chat").send("hello from the cave")
		assert_true(await f.wait_until(func() -> bool: return cb.get_node("Chat/Box/Log").get_parsed_text().contains("hello from the cave")), "chat crosses rooms")
		_walk(f, b, "hub/CaveDoor")
		assert_true(await f.wait_until(func() -> bool: return _level_of(ca, b) == "cave" and _level_of(cb, a) == "cave"), "A and B meet in the cave")
		assert_eq(_levels(cb), ["cave"])
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
		assert_true(await net.wait_until(func() -> bool: return _level_of(ca, a) == "hub"))
		_walk(net, a, "hub/CaveDoor")
		assert_true(await net.wait_until(func() -> bool: return _level_of(ca, a) == "cave"))
		(net.server.get_node("World") as DoorsWorld).start_level = CAVE
		net.client_sessions[2].start(net.peer(cc), cc.get_path())
		var c := net.peer_id(cc)
		assert_true(await net.wait_until(func() -> bool: return _level_of(cc, c) == "cave" and _level_of(cc, a) == "cave"), "C starts in the cave and sees A")
		await wait_frames(10)
		assert_eq(_levels(cc), ["cave"])
		assert_true(cc.has_node("World/cave/NetManager/CaveTorch"))
		assert_null(_player(cc, net.peer_id(net.clients[1])), "B (in the hub) is not in C's scope")
		assert_null(_player(cc, 1), "nor is the host's player")
		assert_eq(net.session(cc).entity_count(), 3, "A, C and the cave torch")
		net.teardown()
		net = null


func test_an_empty_level_unloads_after_the_timeout_and_reloads() -> void:
	var ok := await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var a := f.peer_id(ca)
		f.server_session.empty_level_timeout = 0.5
		_setup(f, [f.server, ca])
		assert_true(await f.wait_until(func() -> bool: return _everyone_in_hub(f, [f.server, ca])))
		for id in f.players():
			_walk(f, id, "hub/CaveDoor")
		assert_true(await f.wait_until(func() -> bool: return Array(f.server_session.levels()) == [CAVE]), "the hub unloads once empty")
		assert_false(f.server.has_node("World/hub"))
		_walk(f, a, "cave/ReturnDoor")
		assert_true(await f.wait_until(func() -> bool: return _level_of(ca, a) == "hub" and _level_of(f.server, a) == "hub"), "the hub reloads on the next travel")
		assert_has(f.server_session.levels(), HUB)
		assert_true(await f.wait_until(func() -> bool: return ca.has_node("World/hub/NetManager/TorchLeft") and _levels(ca) == ["hub"]))
	)
	assert_true(ok, "fixtures connected")


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
