extends TestCase
## WP2 (docs/PLAN_NETCODE.md 2.1, 4): NetManager enrolment, entities and parts, atomic spawn, destroy,
## reparent, the late-join burst, spawn policy, owners, the bare-node fallback, the scene part check and
## gate G0. Headless over NetPipePeer; the spawning scenarios run in both host modes (G2).

const CRATE := preload("res://tests/fixtures/net/crate.tscn")
const NESTED := preload("res://tests/fixtures/net/nested.tscn")
const SpawnProbe := preload("res://tests/fixtures/net/spawn_probe.gd")

var net: NetFixture


func before_each() -> void:
	SpawnProbe.despawn_log.clear()


func after_each() -> void:
	if net != null:
		net.teardown()
		net = null
	Net.leave()


## Counts a session's entity signals into `counts` under `key`.
func _count_spawns(session: NetSession, counts: Dictionary, key: String) -> void:
	counts[key] = []
	session.entity_spawned.connect(func(node: Node) -> void: counts[key].append(String(node.name)))


func _crate(crate_name: String) -> Node2D:
	var c: Node2D = CRATE.instantiate()
	c.name = crate_name
	return c


func test_a_server_spawn_appears_on_the_client() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var client: Node = f.clients[0]
		var counts := {}
		_count_spawns(f.server_session, counts, "server")
		_count_spawns(f.session(client), counts, "client")
		var group := Node2D.new()
		group.name = "Group"
		Net.ignore(group)   # a plain container the test keeps local; the crate goes beside it
		managers["Server"].add_child(group)
		var on_server := _crate("Crate1")
		managers["Server"].add_child(on_server)
		assert_true(await f.wait_until(func() -> bool: return client.has_node("World/Crate1")), "the replica appeared")
		var replica: Node2D = client.get_node("World/Crate1")
		assert_eq(replica.scene_file_path, CRATE.resource_path, "instantiated from the same scene")
		assert_true(replica.has_node("Sprite") and replica.has_node("Marker"), "with its parts")
		assert_false(client.has_node("World/Group"), "an ignored node is not replicated")
		assert_eq(replica.spawn_calls, 1, "_net_spawned ran once on the client")
		assert_eq(replica.spawn_info["is_owner"], false)
		assert_eq(replica.spawn_info["is_authority"], false)
		assert_eq(replica.spawn_info["owner"], 1)
		assert_gt(replica.spawn_info["net_id"], 0)
		assert_eq(on_server.spawn_calls, 1, "_net_spawned ran once on the server")
		assert_eq(on_server.spawn_info["is_authority"], true)
		assert_eq(on_server.spawn_info["is_owner"], true, "the server owns it by default")
		assert_eq(on_server.spawn_info["net_id"], replica.spawn_info["net_id"], "one id on both sides")
		await _frames(5)
		assert_eq(counts["server"], ["Crate1"], "entity_spawned once on the server")
		assert_eq(counts["client"], ["Crate1"], "entity_spawned once on the client")
		assert_eq(managers["Server"].entities, 1)
		assert_eq(managers["ClientA"].entities, 1)
		assert_true(Net.is_authority(on_server))
		assert_false(Net.is_authority(replica))
		assert_false(Net.is_owner(replica))
		assert_eq(Net.net_id(replica.get_node("Marker")), replica.spawn_info["net_id"], "a part answers for its entity"))
	assert_true(ok, "both fixtures connected")


func test_nested_instances_are_parts_unless_split() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var client: Node = f.clients[0]
		var counts := {}
		_count_spawns(f.session(client), counts, "client")
		managers["Server"].add_child(NESTED.instantiate())
		assert_true(await f.wait_until(func() -> bool: return counts["client"].size() == 2), "two entities: Nested and its split child")
		assert_eq(counts["client"], ["Nested", "Split"])
		var nested: Node = client.get_node("World/Nested")
		var inner: Node2D = nested.get_node("Inner")
		var split: Node2D = nested.get_node("Split")
		assert_eq(nested.get_child_count(), 2, "the split child was adopted, not instantiated twice")
		var nested_id := Net.net_id(nested)
		assert_gt(nested_id, 0)
		assert_eq(inner.spawn_info.get("net_id"), nested_id, "Inner is a part of Nested")
		assert_ne(split.spawn_info.get("net_id"), nested_id, "Split is an entity of its own")
		assert_eq((split.get_node("NetEntity") as NetEntity).net_id, split.spawn_info["net_id"])
		assert_eq(f.server_session.entity_count(), 2)
		assert_eq(f.session(client).entity_count(), 2))
	assert_true(ok)


func test_a_late_joiner_gets_all_fifty_spawns_in_order() -> void:
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		net = NetFixture.new(self, 1, mode)
		net.start_server()
		var managers := net.add_managers()
		var expected: Array[String] = []
		var parent: Node = managers["Server"]
		for i in 50:
			var c := _crate("C%d" % i)
			parent.add_child(c)
			expected.append(String(c.name))
			# Every fifth crate starts a new chain under the manager; the others nest under the
			# previous crate's Marker part.
			parent = managers["Server"] if i % 5 == 4 else c.get_node("Marker")
		await _frames(3)
		assert_eq(net.server_session.entity_count(), 50)
		var client: Node = net.clients[0]
		var counts := {}
		_count_spawns(net.session(client), counts, "client")
		net.start_clients()
		assert_true(await net.wait_connected())
		assert_true(await net.wait_until(func() -> bool: return counts["client"].size() == 50), "all 50 arrived (%d)" % counts["client"].size())
		assert_eq(counts["client"], expected, "in order")
		for c: Node in managers["Server"].find_children("C*", "", true, false):
			var rel := String(net.server.get_path_to(c))
			assert_true(client.has_node(rel), "%s exists on the client under the same parent" % rel)
		net.teardown()
		net = null


func test_level_content_on_every_peer_is_adopted_not_duplicated() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		# The fixture is already connected; build "level" managers that hold a crate when they enter.
		var managers := f.add_managers("Level", func(m: NetManager) -> void: m.add_child(_crate("Placed")))
		var client: Node = f.clients[0]
		var placed: Node2D = client.get_node("Level/Placed")
		assert_true(await f.wait_until(func() -> bool: return placed.spawn_calls == 1), "the client's own instance was bound to the server's entity")
		assert_eq(client.get_node("Level").get_child_count(), 1, "not instantiated a second time")
		assert_eq(Net.net_id(placed), Net.net_id(managers["Server"].get_node("Placed")))
		assert_eq(placed.spawn_info["is_authority"], false))
	assert_true(ok)


func test_queue_free_frees_the_replica_after_net_despawning() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var client: Node = f.clients[0]
		var despawned := []
		f.session(client).entity_despawning.connect(func(n: Node) -> void: despawned.append(n.is_inside_tree()))
		var c := _crate("Doomed")
		managers["Server"].add_child(c)
		assert_true(await f.wait_until(func() -> bool: return client.has_node("World/Doomed")))
		SpawnProbe.despawn_log.clear()
		c.queue_free()
		assert_true(await f.wait_until(func() -> bool: return not client.has_node("World/Doomed")), "the replica was freed")
		var client_path := str(client.get_path()) + "/World/Doomed"
		assert_true(SpawnProbe.despawn_log.has(client_path), "_net_despawning ran on the replica: %s" % [SpawnProbe.despawn_log])
		assert_eq(despawned, [true], "entity_despawning fired while the replica was still in the tree")
		assert_eq(f.session(client).entity_count(), 0))
	assert_true(ok)


func test_reparenting_on_the_server_moves_the_replica() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var client: Node = f.clients[0]
		var a := _crate("A")
		var b := _crate("B")
		managers["Server"].add_child(a)
		managers["Server"].add_child(b)
		assert_true(await f.wait_until(func() -> bool: return client.has_node("World/B")))
		var replica_b := client.get_node("World/B")
		b.reparent(a.get_node("Marker"))
		assert_true(await f.wait_until(func() -> bool: return client.has_node("World/A/Marker/B")), "moved under A's Marker")
		assert_true(client.get_node("World/A/Marker/B") == replica_b, "the same replica, not a new one")
		assert_eq(replica_b.spawn_calls, 1, "no second spawn")
		b.reparent(managers["Server"])
		assert_true(await f.wait_until(func() -> bool: return client.has_node("World/B")), "and back"))
	assert_true(ok)


func test_a_client_spawn_is_refused_under_server_only() -> void:
	allow_errors = true
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	var client: Node = net.clients[0]
	var refused := []
	net.session(client).spawn_refused.connect(func(n: Node) -> void: refused.append(String(n.name)))
	var c := _crate("Sneaky")
	managers["ClientA"].add_child(c)
	assert_true(await net.wait_until(func() -> bool: return refused.size() == 1), "refused")
	assert_eq(refused, ["Sneaky"])
	await _frames(5)
	assert_false(is_instance_valid(c), "the node was freed")
	assert_false(net.server.has_node("World/Sneaky"), "the server never saw it")
	assert_eq(net.server_session.entity_count(), 0)


func test_a_client_spawn_becomes_its_entity_under_owner_allowed() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var managers := f.add_managers("World", func(m: NetManager) -> void: m.spawn_policy = 1)
		var a: Node = f.clients[0]
		var b: Node = f.clients[1]
		var cid := f.peer_id(a)
		var c := _crate("Mine")
		managers["ClientA"].add_child(c)
		assert_true(await f.wait_until(func() -> bool: return f.server.has_node("World/Mine") and b.has_node("World/Mine")), "visible on the server and the other client")
		var on_server: Node = f.server.get_node("World/Mine")
		assert_eq(Net.get_owner_of(on_server), cid, "owned by the client that spawned it")
		assert_eq(Net.get_owner_of(b.get_node("World/Mine")), cid)
		assert_true(Net.is_owner(c), "the creator owns it")
		assert_false(Net.is_owner(b.get_node("World/Mine")))
		assert_eq(c.spawn_info["is_owner"], true)
		assert_eq(Net.net_id(on_server), Net.net_id(c))
		assert_true(is_instance_valid(c) and c.is_inside_tree(), "the creator keeps its own node")
		assert_eq(a.get_node("World").get_child_count(), 1, "no echo back to the creator")
		c.queue_free()
		assert_true(await f.wait_until(func() -> bool: return not f.server.has_node("World/Mine") and not b.has_node("World/Mine")), "its destroy is relayed"))
	assert_true(ok)


func test_spawn_for_and_set_owner() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	var client: Node = net.clients[0]
	var cid := net.peer_id(client)
	var node: Node = Net.spawn_for(cid, CRATE, managers["Server"], "Player")
	assert_eq(String(node.name), "Player")
	assert_true(await net.wait_until(func() -> bool: return client.has_node("World/Player")))
	var replica: Node2D = client.get_node("World/Player")
	assert_eq(replica.spawn_info["owner"], cid)
	assert_eq(replica.spawn_info["is_owner"], true, "the client owns its player")
	assert_true(Net.is_owner(replica))
	assert_false(Net.is_owner(node), "the server does not")
	assert_true(Net.set_owner(node, 1))
	assert_true(await net.wait_until(func() -> bool: return Net.get_owner_of(replica) == 1), "the owner change reached the client")
	assert_false(Net.is_owner(replica))


func test_a_bare_node_replicates_as_its_class_with_a_warning() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	var client: Node = net.clients[0]
	var s := Sprite2D.new()
	s.name = "Loose"
	managers["Server"].add_child(s)
	var c := _crate("OnTop")
	s.add_child(c)
	assert_true(await net.wait_until(func() -> bool: return client.has_node("World/Loose/OnTop")), "the bare node and an entity under it replicate")
	assert_true(client.get_node("World/Loose") is Sprite2D, "as its class")
	assert_eq(net.server_session.entity_count(), 2)


func test_entities_belong_to_the_manager_they_are_under() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var world := net.add_managers("World")
	var other := net.add_managers("Other")
	var client: Node = net.clients[0]
	world["Server"].add_child(_crate("InWorld"))
	other["Server"].add_child(_crate("InOther"))
	assert_true(await net.wait_until(func() -> bool: return client.has_node("World/InWorld") and client.has_node("Other/InOther")))
	assert_false(client.has_node("World/InOther") or client.has_node("Other/InWorld"))
	assert_eq(world["ClientA"].entities, 1)
	assert_eq(other["ClientA"].entities, 1)


func test_net_manage_enrols_a_node_outside_any_manager() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var anchors := {}
	for branch: Node in [net.server, net.clients[0]]:
		var anchor := Node2D.new()
		anchor.name = "Anchor"
		branch.add_child(anchor)
		Net.manage(anchor)
		anchors[String(branch.name)] = anchor
	anchors["Server"].add_child(_crate("Managed"))
	assert_true(await net.wait_until(func() -> bool: return net.clients[0].has_node("Anchor/Managed")))


func test_a_different_part_table_disconnects_naming_the_scene() -> void:
	net = NetFixture.new(self, 1)
	net.client_sessions[0]._debug_part_table_salt = 3
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	managers["Server"].add_child(_crate("Mismatch"))
	assert_true(await net.wait_until(func() -> bool: return net.disconnect_reasons.has("ClientA")), "the client was disconnected")
	var reason: String = net.disconnect_reasons["ClientA"]
	assert_true(reason.contains("part table of scene res://tests/fixtures/net/crate.tscn differs"), reason)


func test_g0_no_manager_no_session_no_packets() -> void:
	var before := NetSession.packets_sent_total()
	var main: Node = load("res://scenes/main.tscn").instantiate()
	add_child(main)
	await _frames(20)
	assert_eq(NetSession.packets_sent_total(), before, "nothing was sent")
	assert_false(Net.is_online())
	assert_null(Net.get_session())
	assert_null(get_tree().root.get_node_or_null("__NetSession"), "no session node")
	assert_true(get_tree().get_nodes_in_group("__gdnet_managers").is_empty(), "no manager")


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame
