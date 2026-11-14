class_name NetFixture
extends RefCounted
## Headless multi-peer test fixture (docs/PLAN_NETCODE.md 4, "Headless two-peer testing").
##
## Builds /root-level branches `Server`, `ClientA`, `ClientB`, ... Each branch gets its own
## `MultiplayerAPI` (`SceneTree.set_multiplayer(api, "/root/<branch>")`) and its own gdnet session: a
## `NetSession` node, child of the branch, with the same surface as the `Net` singleton (`tick()`,
## `rtt()`, `peers()`, `is_online()`, the signals). The `Net` singleton is process-global and always
## serves the default (whole-tree) multiplayer, so branches never go through it; a test may still use
## `Net` beside the branches (it is a separate session on the tree's default API).
##
## The branches are connected in memory by `NetPipePeer` ends (the server end belongs to `Server`), with
## optional `NetEmulation` per branch. Typical use inside a `TestCase`:
##
##     var net := NetFixture.new(self, 2)
##     net.start()
##     assert_true(await net.wait_connected())
##     ...                      # net.server, net.clients[0], net.session(net.clients[0]) ...
##     net.teardown()           # also safe to call from after_each
##
## Host modes (gate G2): `HOST_PLAYER` (the server branch is also a player, peer 1) and `DEDICATED`
## (the server branch only serves). Later packages spawn players per `players()`;
## `NetFixture.run_both_modes(test, n, scenario)` runs one scenario under each.
##
## WP17 (seats): a DEDICATED server session has `dedicated = true` (it holds no seat). Each client
## session has its own random seat key unless the test sets `client_sessions[i].key` before starting.
## `drop_client(i)` frees a client's branch as if its process ended (returns its key);
## `rejoin_client(i, key)` connects a fresh branch, session and pipe end in that slot with the same key
## (a new peer id), and `add_manager_to(branch)` gives it its managers. `start_server(state)` starts the
## server from a saved session state.

enum HostMode { HOST_PLAYER, DEDICATED }

const CLIENT_NAMES: Array[String] = ["ClientA", "ClientB", "ClientC", "ClientD", "ClientE", "ClientF"]

var host_mode: HostMode = HostMode.HOST_PLAYER
## The server branch node (`/root/Server`).
var server: Node
## The client branch nodes, in order.
var clients: Array[Node] = []
var server_session: NetSession
var client_sessions: Array[NetSession] = []
## Peer ids each branch saw join, by branch name (filled from the sessions' `peer_joined`).
var joined: Dictionary = {}
## Peer ids each branch saw leave, by branch name.
var left: Dictionary = {}
## Disconnection reasons by branch name.
var disconnect_reasons: Dictionary = {}

var _test: Node
var _client_count: int
var _server_peer: NetPipePeer
var _client_peers: Array[NetPipePeer] = []
var _started: bool = false
## WP17: the seat key of each dropped client slot (`drop_client`), by index.
var _dropped_keys: Dictionary = {}


func _init(test: Node, client_count: int = 1, mode: HostMode = HostMode.HOST_PLAYER) -> void:
	_test = test
	_client_count = clampi(client_count, 0, CLIENT_NAMES.size())
	host_mode = mode
	build()


## Creates the branches, their sessions and the pipe ends, without starting anything. Called by
## `_init`; set session properties (e.g. `_debug_protocol_salt`) or emulation, then call `start()`.
func build() -> void:
	if server != null:
		return
	var root := _test.get_tree().root
	server = _make_branch(root, "Server")
	server_session = _make_session(server)
	# WP17: a dedicated server holds no seat (the first client is seat 1).
	server_session.dedicated = host_mode == HostMode.DEDICATED
	_server_peer = NetPipePeer.create_server()
	for i in _client_count:
		var branch := _make_branch(root, CLIENT_NAMES[i])
		clients.append(branch)
		client_sessions.append(_make_session(branch))
		_client_peers.append(NetPipePeer.create_client(_server_peer))


## Starts every session: the server first, then each client.
func start() -> void:
	start_server()
	start_clients()


## Starts only the server's session (late-join scenarios start the clients later). WP17: `state` (from
## `NetSession.save_state()`) is the session state the server starts from.
func start_server(state: PackedByteArray = PackedByteArray()) -> void:
	if _started:
		return
	_started = true
	server_session.start(_server_peer, server.get_path(), state)


## Starts the clients' sessions that are not running yet (after `start_server()`).
func start_clients() -> void:
	start_server()
	for i in clients.size():
		if client_sessions[i] != null and not client_sessions[i].is_online():
			client_sessions[i].start(_client_peers[i], clients[i].get_path())


## WP17: the client `index` drops, as if its process ended: its branch is freed (its session leaves
## and its pipe end closes; the server notices at its next poll). Returns the seat key it used, which
## `rejoin_client` sends again. Its slot in `clients` / `client_sessions` is null until then.
func drop_client(index: int) -> String:
	var key := ""
	var s: NetSession = client_sessions[index]
	if s != null:
		key = s.key
	if is_instance_valid(clients[index]):
		clients[index].free()
	clients[index] = null
	client_sessions[index] = null
	_client_peers[index] = null
	_dropped_keys[index] = key
	return key


## WP17: a new connection for the client slot `index` (normally one `drop_client` emptied): a fresh
## branch with the same name, a fresh session and pipe end, started with `key` (empty: the key the
## dropped client used). Add its managers (`add_manager_to`) before or right after; returns the branch.
func rejoin_client(index: int, key: String = "") -> Node:
	if clients[index] != null:
		drop_client(index)
	var branch := _make_branch(_test.get_tree().root, CLIENT_NAMES[index])
	var s := _make_session(branch)
	s.key = key if key != "" else String(_dropped_keys.get(index, ""))
	var p := NetPipePeer.create_client(_server_peer)
	clients[index] = branch
	client_sessions[index] = s
	_client_peers[index] = p
	s.start(p, branch.get_path())
	return branch


## Adds a `NetManager` named `manager_name` to one branch (a rejoined client's).
func add_manager_to(branch: Node, manager_name: String = "World", configure: Callable = Callable()) -> NetManager:
	var m := NetManager.new()
	m.name = manager_name
	if configure.is_valid():
		configure.call(m)
	branch.add_child(m)
	return m


## Adds a `NetManager` named `manager_name` (default `World`) to every branch, at the same path, and
## returns them by branch name. `configure` (optional) is called with each manager before it is added.
func add_managers(manager_name: String = "World", configure: Callable = Callable()) -> Dictionary:
	var out := {}
	for branch: Node in [server] + clients:
		if branch == null:
			continue
		out[String(branch.name)] = add_manager_to(branch, manager_name, configure)
	return out


## Awaits physics frames until every client finished the handshake and the server sees all of them.
## Returns false on timeout.
func wait_connected(timeout: float = 5.0) -> bool:
	return await wait_until(func() -> bool: return _all_connected(), timeout)


## Awaits physics frames until `condition` returns true; false on timeout.
func wait_until(condition: Callable, timeout: float = 5.0) -> bool:
	var deadline := Time.get_ticks_msec() + int(timeout * 1000.0)
	while not condition.call():
		if Time.get_ticks_msec() > deadline:
			return false
		await _test.get_tree().physics_frame
	return true


## The session of a branch node (`server` or one of `clients`).
func session(branch: Node) -> NetSession:
	if branch == server:
		return server_session
	var i := clients.find(branch)
	return client_sessions[i] if i >= 0 else null


## The pipe end of a branch node.
func peer(branch: Node) -> NetPipePeer:
	if branch == server:
		return _server_peer
	var i := clients.find(branch)
	return _client_peers[i] if i >= 0 else null


## The peer id of a branch (1 for the server; a client's transport id).
func peer_id(branch: Node) -> int:
	var p := peer(branch)
	return p.get_unique_id() if p != null else 0


## Applies network emulation to one branch's pipe end (its send_* values to what it sends, receive_* to
## what it receives). Null removes it. Works before or after `start()`.
func set_emulation(branch: Node, emulation: NetEmulation) -> void:
	session(branch).set_emulation(emulation)
	peer(branch).set_emulation(emulation)


## The peer ids that act as players: in HOST_PLAYER mode the server (1) and every client, in DEDICATED
## mode the clients only.
func players() -> PackedInt32Array:
	var ids := PackedInt32Array()
	if host_mode == HostMode.HOST_PLAYER:
		ids.append(1)
	for branch in clients:
		if branch != null:
			ids.append(peer_id(branch))
	return ids


## Frees the branches, clients first. Each branch's nodes leave the tree while gdnet's API is still
## installed (so stock synchronizers and spawners unregister from the API they registered with); the
## session node, the branch's first child, exits last and leaves its session, restoring the tree's
## multiplayer for the branch.
func teardown() -> void:
	for branch in clients:
		if is_instance_valid(branch):
			branch.free()
	if is_instance_valid(server):
		server.free()
	clients.clear()
	client_sessions.clear()
	_client_peers.clear()
	_server_peer = null
	server = null
	server_session = null


## Runs `scenario` (a Callable taking a started, connected NetFixture and awaiting as it likes) once per
## host mode, tearing down in between. Returns false when a fixture failed to connect.
static func run_both_modes(test: Node, client_count: int, scenario: Callable) -> bool:
	for mode: HostMode in [HostMode.HOST_PLAYER, HostMode.DEDICATED]:
		# `new()`, not `NetFixture.new()`: a script naming its own class_name in a static function can
		# be reported as a resource still in use at exit, depending on which scripts the process loaded.
		var f: NetFixture = new(test, client_count, mode)
		f.start()
		var ok: bool = await f.wait_connected()
		if ok:
			await scenario.call(f)
		f.teardown()
		if not ok:
			return false
	return true


func _all_connected() -> bool:
	if not is_instance_valid(server_session) or not server_session.is_online():
		return false
	var live := client_sessions.filter(func(s: NetSession) -> bool: return s != null)
	if server_session.peers().size() != live.size():
		return false
	for s: NetSession in live:
		if not s.is_online() or not s.peers().has(1):
			return false
	return true


func _make_branch(root: Node, branch_name: String) -> Node:
	var stale := root.get_node_or_null(branch_name)
	if stale != null:
		stale.free()
	var branch := Node.new()
	branch.name = branch_name
	root.add_child(branch)
	return branch


func _make_session(branch: Node) -> NetSession:
	var s := NetSession.new()
	s.name = "NetSession"
	branch.add_child(s)
	var key := String(branch.name)
	joined[key] = []
	left[key] = []
	s.peer_joined.connect(func(id: int) -> void: joined[key].append(id))
	s.peer_left.connect(func(id: int) -> void: left[key].append(id))
	s.disconnected.connect(func(reason: String) -> void: disconnect_reasons[key] = reason)
	return s
