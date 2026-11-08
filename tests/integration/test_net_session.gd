extends TestCase
## WP1 (docs/PLAN_NETCODE.md 4): sessions, the tick, the pipe peer, the MultiplayerAPI wrapper (G1),
## emulation and the protocol handshake. Everything runs headless over NetPipePeer except one ENet test.

const RpcProbe := preload("res://tests/fixtures/net/rpc_probe.gd")
const SyncedValue := preload("res://tests/fixtures/net/synced_value.gd")
const ENET_PORT := 47913

var net: NetFixture


func after_each() -> void:
	if net != null:
		net.teardown()
		net = null
	Net.leave()
	Net.emulation = null
	Net._debug_protocol_salt = 0


func _physics_frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


func _probe(branch: Node) -> Node:
	var p: Node = RpcProbe.new()
	p.name = "Probe"
	branch.add_child(p)
	return p


func test_offline_ticks_once_per_physics_frame() -> void:
	var original := get_tree().get_multiplayer()
	assert_false(Net.is_online())
	assert_eq(Net.offline(), OK)
	assert_true(Net.is_online())
	assert_true(get_tree().get_multiplayer() is NetMultiplayerApi, "the wrapper is installed")
	assert_true(multiplayer.is_server(), "offline: this process hosts")
	assert_eq(multiplayer.get_unique_id(), 1)
	assert_eq(Net.tick_rate, Engine.physics_ticks_per_second)
	var count := [0]
	var last := [-1]
	Net.ticked.connect(func(t: int) -> void:
		count[0] += 1
		last[0] = t)
	await _physics_frames(2)
	var c0: int = count[0]
	var t0 := Net.tick()
	await _physics_frames(30)
	assert_eq(count[0] - c0, 30, "one tick signal per physics frame")
	assert_eq(Net.tick() - t0, 30, "Net.tick() advances once per physics frame")
	assert_eq(last[0], Net.tick(), "the signal carries the tick")
	Net.leave()
	assert_false(Net.is_online())
	assert_eq(Net.tick(), 0)
	assert_true(get_tree().get_multiplayer() == original, "leave() restores the original MultiplayerAPI")
	var c1: int = count[0]
	await _physics_frames(5)
	assert_eq(count[0], c1, "no ticks after leave()")


func test_two_branches_connect_through_a_pipe() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected(), "the client finished the handshake")
	var client: Node = net.clients[0]
	var cid := net.peer_id(client)
	assert_gt(cid, 1)
	assert_eq(net.joined["Server"], [cid], "the server saw the client join")
	assert_eq(net.joined["ClientA"], [1], "the client saw the server join")
	assert_eq(net.server_session.peers(), PackedInt32Array([cid]))
	assert_eq(net.session(client).peers(), PackedInt32Array([1]))
	assert_eq(net.session(client).unique_id(), cid)
	assert_true(get_tree().get_multiplayer(client.get_path()) is NetMultiplayerApi)
	assert_true(get_tree().get_multiplayer(client.get_path()) != get_tree().get_multiplayer(net.server.get_path()), "each branch has its own API")
	assert_true(await net.wait_until(func() -> bool: return net.server_session.rtt(cid) >= 0.0 and net.session(client).rtt(1) >= 0.0), "RTT samples arrive")
	var rtt_s := net.server_session.rtt(cid)
	var rtt_c := net.session(client).rtt(1)
	assert_true(is_finite(rtt_s) and rtt_s < 1000.0, "server RTT %s" % rtt_s)
	assert_true(is_finite(rtt_c) and rtt_c < 1000.0, "client RTT %s" % rtt_c)
	# The client's tick follows the server's.
	assert_lt(absf(net.session(client).server_tick() - float(net.server_session.tick())), 3.0)


func test_both_host_modes_connect() -> void:
	var seen: Array = []
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		seen.append([f.host_mode, f.players().size(), f.server_session.peers().size()]))
	assert_true(ok, "both fixtures connected")
	assert_eq(seen, [[NetFixture.HostMode.HOST_PLAYER, 3, 2], [NetFixture.HostMode.DEDICATED, 2, 2]])


func test_stock_rpc_works_under_the_wrapper() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var client: Node = net.clients[0]
	var on_server := _probe(net.server)
	var on_client := _probe(client)
	on_client.rpc_id(1, &"ping", 42)
	assert_true(await net.wait_until(func() -> bool: return on_server.received.size() == 1), "the rpc reached the server branch")
	assert_eq(on_server.received[0][0], net.peer_id(client), "remote sender id")
	assert_eq(on_server.received[0][1], 42)
	assert_eq(on_client.received.size(), 0, "call_remote: not run locally")


func test_stock_synchronizer_works_under_the_wrapper() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var on_server: Node = SyncedValue.new()
	on_server.name = "Synced"
	net.server.add_child(on_server)
	var on_client: Node = SyncedValue.new()
	on_client.name = "Synced"
	net.clients[0].add_child(on_client)
	on_server.value = 7
	assert_true(await net.wait_until(func() -> bool: return on_client.value == 7, 3.0), "MultiplayerSynchronizer replicated value (client has %s)" % on_client.value)
	on_server.value = -3
	assert_true(await net.wait_until(func() -> bool: return on_client.value == -3, 3.0), "and a change")


func test_emulated_latency_delays_by_whole_physics_ticks() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var client: Node = net.clients[0]
	var on_server := _probe(net.server)
	var on_client := _probe(client)
	var emu := NetEmulation.new()
	emu.send_latency_ms = 100.0
	net.set_emulation(client, emu)
	var sent := Engine.get_physics_frames()
	on_client.rpc_id(1, &"ping", 1)
	assert_true(await net.wait_until(func() -> bool: return on_server.received.size() == 1))
	var frames: int = on_server.received[0][2] - sent
	var per_tick_ms := 1000.0 / Engine.physics_ticks_per_second
	var expected := int(ceil(100.0 / per_tick_ms))
	assert_true(frames >= expected and frames <= expected + 2, "100 ms took %d physics frames, expected about %d" % [frames, expected])
	# Back to no delay: the next call arrives within a frame.
	emu.send_latency_ms = 0.0
	sent = Engine.get_physics_frames()
	on_client.rpc_id(1, &"ping", 2)
	assert_true(await net.wait_until(func() -> bool: return on_server.received.size() == 2))
	assert_lt(on_server.received[1][2] - sent, 2, "0 ms arrives at once")


func test_emulated_total_loss_delivers_nothing() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var client: Node = net.clients[0]
	var on_server := _probe(net.server)
	_probe(client).rpc_id(1, &"ping", 1)
	assert_true(await net.wait_until(func() -> bool: return on_server.received.size() == 1), "control: arrives without loss")
	var emu := NetEmulation.new()
	emu.send_loss = 1.0
	net.set_emulation(client, emu)
	client.get_node("Probe").rpc_id(1, &"ping", 2)
	await _physics_frames(30)
	assert_eq(on_server.received.size(), 1, "at loss 1.0 even a reliable rpc never arrives")


func test_protocol_mismatch_disconnects_with_a_named_reason() -> void:
	net = NetFixture.new(self, 1)
	net.client_sessions[0]._debug_protocol_salt = 5
	net.start()
	var client_session: NetSession = net.client_sessions[0]
	assert_true(await net.wait_until(func() -> bool: return net.disconnect_reasons.has("ClientA")), "the client was disconnected")
	assert_eq(net.disconnect_reasons["ClientA"], "protocol mismatch: protocol salt differs (server 0, client 5)")
	assert_false(client_session.is_online())
	assert_eq(net.joined["Server"], [], "the server never admitted it")
	assert_eq(net.server_session.peers().size(), 0)


func test_net_relays_a_mismatch_from_its_own_session() -> void:
	# A server branch, and Net itself joining it as a client over a pipe end with a salted protocol.
	net = NetFixture.new(self, 0)
	net.start()
	var client_end := NetPipePeer.create_client(net.peer(net.server))
	var reasons: Array = []
	Net.disconnected.connect(func(r: String) -> void: reasons.append(r), CONNECT_ONE_SHOT)
	Net._debug_protocol_salt = 9
	assert_eq(Net.use_peer(client_end), OK)
	assert_true(await net.wait_until(func() -> bool: return reasons.size() == 1), "Net.disconnected fired")
	assert_eq(reasons[0], "protocol mismatch: protocol salt differs (server 0, client 9)")
	assert_false(Net.is_online())
	assert_false(get_tree().get_multiplayer() is NetMultiplayerApi, "the original API is back")


func test_net_joins_a_branch_host_and_reports_peers() -> void:
	net = NetFixture.new(self, 0)
	net.start()
	var client_end := NetPipePeer.create_client(net.peer(net.server))
	var joined: Array = []
	Net.peer_joined.connect(func(id: int) -> void: joined.append(id), CONNECT_ONE_SHOT)
	assert_eq(Net.use_peer(client_end), OK)
	assert_true(await net.wait_until(func() -> bool: return Net.peers().has(1)), "Net joined the server branch")
	assert_eq(joined, [1])
	assert_eq(net.server_session.peers(), PackedInt32Array([client_end.get_unique_id()]))
	assert_true(await net.wait_until(func() -> bool: return Net.rtt(1) >= 0.0))
	Net.leave()
	assert_true(await net.wait_until(func() -> bool: return net.left["Server"].size() == 1), "the server saw it leave")
	assert_eq(net.left["Server"], [client_end.get_unique_id()])
	assert_true(net.server_session.peers().is_empty())


func test_host_and_join_over_enet_on_localhost() -> void:
	if Net.host(ENET_PORT) != OK:
		skip("could not bind UDP port %d" % ENET_PORT)
		return
	var joined: Array = []
	Net.peer_joined.connect(func(id: int) -> void: joined.append(id), CONNECT_ONE_SHOT)
	var branch := Node.new()
	branch.name = "EnetClient"
	get_tree().root.add_child(branch)
	var session := NetSession.new()
	branch.add_child(session)
	var enet := ENetMultiplayerPeer.new()
	assert_eq(enet.create_client("127.0.0.1", ENET_PORT, 3), OK)
	assert_eq(session.start(enet, branch.get_path()), OK)
	var deadline := Time.get_ticks_msec() + 5000
	while (joined.is_empty() or not session.peers().has(1)) and Time.get_ticks_msec() < deadline:
		await get_tree().physics_frame
	assert_eq(joined.size(), 1, "the host saw the ENet client join")
	assert_true(session.peers().has(1), "the ENet client finished the handshake")
	if not joined.is_empty():
		assert_eq(Net.peers(), PackedInt32Array([joined[0]]))
	session.leave()
	branch.free()
