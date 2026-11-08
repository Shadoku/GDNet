extends TestCase
## WP1: NetEmulation and NetPipePeer as plain objects (no session).


func test_presets_set_both_directions() -> void:
	var e := NetEmulation.new()
	assert_true(e.is_off())
	assert_true(e.preset("Bad"))
	assert_eq(e.send_latency_ms, 100.0)
	assert_eq(e.send_variance_ms, 100.0)
	assert_almost_eq(e.send_loss, 0.05)
	assert_eq(e.receive_latency_ms, 100.0)
	assert_true(e.preset("average"))
	assert_eq(e.send_latency_ms, 30.0)
	assert_almost_eq(e.receive_loss, 0.01)
	assert_false(e.preset("dialup"), "unknown preset")
	assert_eq(e.send_latency_ms, 30.0, "an unknown preset changes nothing")
	assert_true(e.preset("off"))
	assert_true(e.is_off())


func test_pipe_pair_ids_and_status() -> void:
	var pair := NetPipePeer.create_pair()
	var server: NetPipePeer = pair[0]
	var client: NetPipePeer = pair[1]
	assert_eq(server.get_unique_id(), 1)
	assert_gt(client.get_unique_id(), 1)
	assert_eq(server.get_connection_status(), MultiplayerPeer.CONNECTION_CONNECTED)
	assert_eq(client.get_connection_status(), MultiplayerPeer.CONNECTION_CONNECTING)
	var seen: Array = []
	client.peer_connected.connect(func(id: int) -> void: seen.append(id))
	client.poll()
	assert_eq(client.get_connection_status(), MultiplayerPeer.CONNECTION_CONNECTED)
	assert_eq(seen, [1])
	var second := NetPipePeer.create_client(server)
	assert_ne(second.get_unique_id(), client.get_unique_id())


func test_pipe_moves_packets_both_ways() -> void:
	var pair := NetPipePeer.create_pair()
	var server: NetPipePeer = pair[0]
	var client: NetPipePeer = pair[1]
	client.poll()
	server.poll()
	assert_eq(client.put_packet(PackedByteArray([1, 2, 3])), OK)
	server.poll()
	assert_eq(server.get_available_packet_count(), 1)
	assert_eq(server.get_packet_peer(), client.get_unique_id())
	assert_eq(server.get_packet(), PackedByteArray([1, 2, 3]))
	server.set_target_peer(client.get_unique_id())
	server.transfer_channel = 2
	server.transfer_mode = MultiplayerPeer.TRANSFER_MODE_UNRELIABLE
	server.put_packet(PackedByteArray([9]))
	client.poll()
	assert_eq(client.get_packet_channel(), 2)
	assert_eq(client.get_packet_mode(), MultiplayerPeer.TRANSFER_MODE_UNRELIABLE)
	assert_eq(client.get_packet_peer(), 1)
	assert_eq(client.get_packet(), PackedByteArray([9]))


func test_closing_disconnects_the_other_side() -> void:
	var pair := NetPipePeer.create_pair()
	var server: NetPipePeer = pair[0]
	var client: NetPipePeer = pair[1]
	client.poll()
	server.poll()
	var gone: Array = []
	server.peer_disconnected.connect(func(id: int) -> void: gone.append(id))
	client.close()
	server.poll()
	assert_eq(gone, [client.get_unique_id()])
	assert_eq(client.get_connection_status(), MultiplayerPeer.CONNECTION_DISCONNECTED)


func test_an_end_with_emulated_latency_holds_packets() -> void:
	var pair := NetPipePeer.create_pair()
	var server: NetPipePeer = pair[0]
	var client: NetPipePeer = pair[1]
	client.poll()
	var emu := NetEmulation.new()
	emu.send_latency_ms = 10000.0
	client.set_emulation(emu)
	client.put_packet(PackedByteArray([5]))
	server.poll()
	assert_eq(server.get_available_packet_count(), 0, "held by the emulator")
	assert_eq(client.in_flight(), 1)
