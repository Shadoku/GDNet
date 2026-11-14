class_name NetConnect
extends Node
## gdnet's connect layer (WP23, docs/PLAN_NETCODE.md 12.10; docs/guide/12_playing_online.md): one object
## that starts a session over ENet (an address), Steam (a join code) or no network at all, so a game's code
## after that is the same for every transport. It builds the transport peer and hands it to gdnet; then
## `hosted(code)` or `joined()` fires and the game starts as it would after `Net.host` / `Net.join`.
##
##     var connect := NetConnect.new()
##     add_child(connect)                  # it polls Steam each frame, so it lives in the tree
##     connect.hosted.connect(func(code: String) -> void: print("tell your friend: ", code))
##     connect.failed.connect(func(reason: String) -> void: print(reason))
##     connect.host_steam()                # or host_enet(7350), join_enet("10.0.0.2", 7350),
##                                         # join_steam("K7P2QX"), offline()
##
## Steam goes through GodotSteam (`addons/godotsteam/`, `tools/install_godotsteam.sh`), reached only
## through `backend` (a NetSteamBackend), so without the addon or a running Steam client `available()`
## lacks "steam" and the Steam calls fail with the reason. A join code names a public Steam lobby whose
## data holds the code, the game's name (app 480 is shared by every developer testing on Steam) and the
## host's Steam id; the client searches by code and name and connects to the lobby's owner. Where two
## lobbies carry one code, the one created last wins.
##
## Where the session runs: by default the `Net` singleton (`Net.host`, `Net.join`, `Net.offline`,
## `Net.use_peer`). With `session` set, that `NetSession` (`session.start(peer, session_root)`; tests
## under NetFixture). With `start_peer` set, that Callable receives each peer instead (unit tests).
##
## Seats (WP17, docs/guide/13_seats_and_rejoining.md): over Steam a session joins with the player's Steam
## id as its seat key ("steam:<id>"), so a player who restarts the game takes their seat back; ENet and
## offline keep the session's own key unless `key` is set. A join the server refuses because the key's
## seat is still connected (it has not yet noticed the old connection drop) is tried again, up to
## `join_retries` times, `retry_delay_s` apart. `state` (from `Net.save_state()`) is the session state
## the host_* and offline calls start from.

## A session started as the host: `code` is the Steam join code, empty for ENet and offline.
signal hosted(code: String)
## A session started as a client (the peer went to gdnet; `connected` follows after the handshake).
signal joined()
## Starting failed, or a running Steam game ended because its host left. `reason` is for the player.
signal failed(reason: String)
## Progress, for a status line ("looking for K7P2QX...").
signal status(text: String)

## Join code characters: no 0/O, 1/I/L, so a code read aloud or off a screen is typed right.
const CODE_ALPHABET := "ABCDEFGHJKMNPQRSTUVWXYZ23456789"
const CODE_LENGTH := 6
## The words of the server's refusal when a key's seat is still connected (netcore's session).
const SEAT_IN_USE := "already in use"
## Lobby data keys.
const KEY_CODE := "gdnet_code"
const KEY_GAME := "gdnet_game"
const KEY_HOST := "gdnet_host"
const KEY_CREATED := "gdnet_created"
const DEFAULT_APP_ID := 480

enum Step { IDLE, CREATING, SEARCHING, JOINING, RUNNING }

## All Steam access. Replace it before the first call (tests hand in a fake).
var backend: NetSteamBackend:
	set = set_backend
## The session to start; null = the `Net` singleton.
var session: NetSession = null
## With `session`: the branch it serves (null = the whole tree).
var session_root: Node = null
## When valid, receives each peer instead of gdnet (`func(peer: MultiplayerPeer) -> Error`).
var start_peer: Callable = Callable()
## The random source of join codes (seed it for repeatable codes).
var rng: RandomNumberGenerator = RandomNumberGenerator.new()
## Unix seconds, stamped on a lobby so the newest of two with one code wins.
var clock: Callable = Callable(Time, "get_unix_time_from_system")
## Seconds to wait for each answer from Steam (lobby created, lobby list, lobby joined).
var timeout_s: float = 10.0
## Lobby size, the host included.
var max_players: int = 16
## The seat key the sessions this object starts use. Empty: the Steam id over Steam ("steam:<id>"), the
## session's own key (`Net.key`, or the `NetSession`'s) over ENet and offline.
var key: String = ""
## The session state `host_enet`, `host_steam` and `offline` start from (`Net.save_state()`'s bytes;
## empty = a new session).
var state: PackedByteArray = PackedByteArray()
## How many times a join refused because its seat is still connected is tried again.
var join_retries: int = 5
## Seconds between those tries (on the deltas passed to `advance()`).
var retry_delay_s: float = 2.0
## The Steam app id (`gdnet/connect/steam_app_id`, default 480: Spacewar, Valve's shared test app).
var app_id: int = DEFAULT_APP_ID
## The game's name in lobby data (`gdnet/connect/game_name`, default the project's name).
var game_name: String = ""

var _step: Step = Step.IDLE
var _elapsed: float = 0.0
var _lobby: int = 0
var _owner: int = 0
var _code: String = ""
var _wanted: String = ""
var _running: bool = false
var _steam_checked: bool = false
var _steam_reason: String = ""
var _key_used: String = ""
var _transport: String = ""
var _is_client: bool = false
var _rejoin: Callable = Callable()
var _retries: int = 0
var _retry_in: float = -1.0


func _init() -> void:
	rng.randomize()
	app_id = int(ProjectSettings.get_setting("gdnet/connect/steam_app_id", DEFAULT_APP_ID))
	game_name = String(ProjectSettings.get_setting("gdnet/connect/game_name", ""))
	if game_name.is_empty():
		game_name = String(ProjectSettings.get_setting("application/config/name", "gdnet game"))
	backend = NetSteamBackend.new()


func _process(delta: float) -> void:
	advance(delta)


func _ready() -> void:
	Net.disconnected.connect(_on_disconnected)


func _exit_tree() -> void:
	if _lobby != 0:
		backend.leave_lobby(_lobby)
		_lobby = 0


## A random join code from `random`.
static func make_code(random: RandomNumberGenerator) -> String:
	var out := ""
	for i in CODE_LENGTH:
		out += CODE_ALPHABET[random.randi_range(0, CODE_ALPHABET.length() - 1)]
	return out


## `text` as a code: upper case, without spaces and dashes.
static func normalise_code(text: String) -> String:
	return text.strip_edges().to_upper().replace(" ", "").replace("-", "")


## True when `text` reads as a join code (six characters of CODE_ALPHABET, after `normalise_code`).
## Anything else is an address; a host name of six such letters needs a dot (`server.lan`).
static func is_join_code(text: String) -> bool:
	var c := normalise_code(text)
	if c.length() != CODE_LENGTH:
		return false
	for ch in c:
		if not CODE_ALPHABET.contains(ch):
			return false
	return true


## `{address, port}` from "host", "host:port" or "" (127.0.0.1). An IPv6 address keeps its colons.
static func parse_address(text: String, default_port: int) -> Dictionary:
	var t := text.strip_edges()
	if t.is_empty():
		return {"address": "127.0.0.1", "port": default_port}
	if t.count(":") == 1 and t.get_slice(":", 1).is_valid_int():
		return {"address": t.get_slice(":", 0), "port": t.get_slice(":", 1).to_int()}
	return {"address": t, "port": default_port}


func set_backend(b: NetSteamBackend) -> void:
	if backend != null:
		for pair: Array in _backend_signals(backend):
			var old: Signal = pair[0]
			if old.is_connected(pair[1]):
				old.disconnect(pair[1])
	backend = b
	_steam_checked = false
	for pair: Array in _backend_signals(b):
		var sig: Signal = pair[0]
		sig.connect(pair[1])


## The transports this build can use now: "enet" and "offline" always, "steam" when GodotSteam is
## installed, initialised with `app_id` and a Steam client is running.
func available() -> PackedStringArray:
	var out := PackedStringArray(["enet"])
	if _steam_ready():
		out.append("steam")
	out.append("offline")
	return out


## Why Steam is not available ("" when it is).
func steam_reason() -> String:
	_steam_ready()
	return _steam_reason


## The join code while hosting on Steam ("" otherwise).
func code() -> String:
	return _code


## How the running session was started: "enet", "steam" or "offline" ("" when none runs).
func transport() -> String:
	return _transport


## The seat key the last session this object started joined with: the Steam id key, `key`, or "" when
## the session kept its own.
func session_key() -> String:
	return _key_used


## Tell this object that its session ended with `reason` (the session's `disconnected`). It listens to
## `Net.disconnected` itself; with `session` or `start_peer` set, the owner connects or calls it. A
## client refused because its seat is still connected joins again after `retry_delay_s`.
func session_ended(reason: String) -> void:
	if not _running or not _is_client or not _rejoin.is_valid():
		return
	if not reason.contains(SEAT_IN_USE):
		return
	var again := _rejoin
	var tries := _retries
	leave()
	if tries >= join_retries:
		_fail(reason)
		return
	_rejoin = again
	_retries = tries
	_retry_in = retry_delay_s
	status.emit("your seat is still held by your last connection; trying again in %d s" % ceili(retry_delay_s))


## True from a host/join call until it failed or `leave()`.
func is_active() -> bool:
	return _step != Step.IDLE


## Hosts over ENet on `port`.
func host_enet(port: int) -> Error:
	leave()
	_use_key(false)
	var err: Error
	if _uses_net():
		err = Net.host(port, state)
	else:
		var peer := ENetMultiplayerPeer.new()
		err = peer.create_server(port, 32, 3)
		if err == OK:
			err = _start(peer)
	if err != OK:
		return _fail_now(err, "could not host on port %d: %s" % [port, error_string(err)])
	_started("enet", "hosting on port %d" % port)
	hosted.emit("")
	return OK


## Joins an ENet host. The session reports the handshake (`connected`) or a refusal (`disconnected`).
func join_enet(address: String, port: int) -> Error:
	_retries = 0
	return _join_enet(address, port)


func _join_enet(address: String, port: int) -> Error:
	leave()
	_use_key(false)
	var err: Error
	if _uses_net():
		err = Net.join(address, port)
	else:
		var peer := ENetMultiplayerPeer.new()
		err = peer.create_client(address, port, 3)
		if err == OK:
			err = _start(peer)
	if err != OK:
		return _fail_now(err, "could not join %s:%d: %s" % [address, port, error_string(err)])
	_started("enet", "joining %s:%d" % [address, port])
	_is_client = true
	_rejoin = _join_enet.bind(address, port)
	joined.emit()
	return OK


## A session without a network (this process hosts and plays as peer 1).
func offline() -> Error:
	leave()
	_use_key(false)
	var err: Error = Net.offline(state) if _uses_net() else _start(NetPipePeer.new())
	if err != OK:
		return _fail_now(err, "could not start offline: %s" % error_string(err))
	_started("offline", "playing offline")
	hosted.emit("")
	return OK


## Hosts on Steam: creates a lobby with a new join code; `hosted(code)` fires once it exists. Returns
## ERR_UNAVAILABLE (and emits `failed`) at once when Steam is not available.
func host_steam() -> Error:
	leave()
	if not _steam_ready():
		return _fail_now(ERR_UNAVAILABLE, "Steam is not available: " + _steam_reason)
	_to(Step.CREATING, "creating a Steam lobby...")
	backend.create_lobby(max_players)
	return OK


## Joins the Steam game with join code `join_code`; `joined()` fires once the peer to its host is up.
func join_steam(join_code: String) -> Error:
	_retries = 0
	return _join_steam(join_code)


func _join_steam(join_code: String) -> Error:
	leave()
	var c := normalise_code(join_code)
	if not is_join_code(c):
		return _fail_now(ERR_INVALID_PARAMETER, "%s is not a join code" % join_code)
	if not _steam_ready():
		return _fail_now(ERR_UNAVAILABLE, "Steam is not available: " + _steam_reason)
	_wanted = c
	_to(Step.SEARCHING, "looking for %s..." % c)
	backend.find_lobbies({KEY_GAME: game_name, KEY_CODE: c})
	return OK


## Ends what this object started: leaves the Steam lobby and the session. Emits nothing.
func leave() -> void:
	if _lobby != 0:
		backend.leave_lobby(_lobby)
	_lobby = 0
	_owner = 0
	_code = ""
	_wanted = ""
	_key_used = ""
	_transport = ""
	_is_client = false
	_rejoin = Callable()
	_retry_in = -1.0
	if _running:
		_running = false
		if start_peer.is_valid():
			pass
		elif session != null:
			session.leave()
		else:
			Net.leave()
	_step = Step.IDLE
	_elapsed = 0.0


## Runs Steam's callbacks and the time-outs; `_process` calls it each frame. Tests call it with their
## own deltas instead of waiting.
func advance(delta: float) -> void:
	backend.poll()
	if _retry_in >= 0.0:
		_retry_in -= delta
		if _retry_in <= 0.0:
			_retry_in = -1.0
			var again := _rejoin
			var tries := _retries + 1
			again.call()
			_retries = tries
	if _step == Step.CREATING or _step == Step.SEARCHING or _step == Step.JOINING:
		_elapsed += delta
		if _elapsed > timeout_s:
			_fail("Steam did not answer in %d s; check your connection and try again" % int(timeout_s))


func _steam_ready() -> bool:
	if not _steam_checked:
		_steam_checked = true
		_steam_reason = backend.start(app_id)
	return _steam_reason.is_empty()


func _uses_net() -> bool:
	return session == null and not start_peer.is_valid()


func _start(peer: MultiplayerPeer) -> Error:
	if start_peer.is_valid():
		return start_peer.call(peer)
	if session != null:
		if not session.disconnected.is_connected(session_ended):
			session.disconnected.connect(session_ended)
		return session.start(peer, NodePath() if session_root == null else session_root.get_path(), state)
	return Net.use_peer(peer, state)


## Sets the seat key for the session about to start: `key`, else over Steam the Steam id.
func _use_key(steam: bool) -> void:
	_key_used = key
	if _key_used.is_empty() and steam:
		_key_used = "steam:%d" % backend.my_id()
	if _key_used.is_empty() or start_peer.is_valid():
		return
	if session != null:
		session.key = _key_used
	else:
		Net.key = _key_used


func _on_disconnected(reason: String) -> void:
	if _uses_net():
		session_ended(reason)


func _started(how: String, text: String) -> void:
	_transport = how
	_running = true
	_step = Step.RUNNING
	status.emit(text)


func _to(step: Step, text: String) -> void:
	_step = step
	_elapsed = 0.0
	status.emit(text)


func _fail_now(err: Error, reason: String) -> Error:
	_fail(reason)
	return err


func _fail(reason: String) -> void:
	leave()
	status.emit(reason)
	failed.emit(reason)


func _backend_signals(b: NetSteamBackend) -> Array:
	return [
		[b.lobby_created, _on_lobby_created],
		[b.lobby_list, _on_lobby_list],
		[b.lobby_joined, _on_lobby_joined],
		[b.lobby_member_left, _on_member_left],
		[b.join_requested, _on_join_requested],
	]


func _on_lobby_created(lobby_id: int, error: String) -> void:
	if _step != Step.CREATING:
		if lobby_id != 0:
			backend.leave_lobby(lobby_id)
		return
	if not error.is_empty():
		_fail(error)
		return
	_lobby = lobby_id
	_code = make_code(rng)
	backend.set_lobby_data(lobby_id, KEY_CODE, _code)
	backend.set_lobby_data(lobby_id, KEY_GAME, game_name)
	backend.set_lobby_data(lobby_id, KEY_HOST, str(backend.my_id()))
	backend.set_lobby_data(lobby_id, KEY_CREATED, str(int(clock.call())))
	var peer := backend.host_peer()
	if peer == null:
		_fail("could not host on Steam: " + backend.last_error)
		return
	_use_key(true)
	var err := _start(peer)
	if err != OK:
		_fail("could not start the session: " + error_string(err))
		return
	_started("steam", "hosting on Steam; join code %s" % _code)
	hosted.emit(_code)


func _on_lobby_list(lobby_ids: Array) -> void:
	if _step != Step.SEARCHING:
		return
	var best := 0
	var best_time := -1
	for id: int in lobby_ids:
		# Steam filtered already; check again, so a lax filter never joins someone else's game.
		if backend.lobby_data(id, KEY_GAME) != game_name or backend.lobby_data(id, KEY_CODE) != _wanted:
			continue
		var t := backend.lobby_data(id, KEY_CREATED).to_int()
		if t > best_time:
			best = id
			best_time = t
	if best == 0:
		_fail("no game with code %s (check the code; the host may have closed the game)" % _wanted)
		return
	_join_lobby(best, "joining %s..." % _wanted)


func _join_lobby(lobby_id: int, text: String) -> void:
	_lobby = lobby_id
	_to(Step.JOINING, text)
	backend.join_lobby(lobby_id)


func _on_lobby_joined(lobby_id: int, error: String) -> void:
	if _step != Step.JOINING or lobby_id != _lobby:
		return
	if not error.is_empty():
		_lobby = 0
		_fail(error)
		return
	var owner := backend.lobby_owner(lobby_id)
	var host := backend.lobby_data(lobby_id, KEY_HOST).to_int()
	if owner == 0 or owner != host or owner == backend.my_id():
		_fail("the host has left this game")
		return
	var peer := backend.client_peer(owner)
	if peer == null:
		_fail("could not connect to the host over Steam: " + backend.last_error)
		return
	var wanted := _wanted
	_use_key(true)
	var err := _start(peer)
	if err != OK:
		_fail("could not start the session: " + error_string(err))
		return
	_owner = owner
	_started("steam", "joined over Steam")
	_is_client = true
	if not wanted.is_empty():
		_rejoin = _join_steam.bind(wanted)
	joined.emit()


func _on_member_left(lobby_id: int, member_id: int) -> void:
	if lobby_id == _lobby and _owner != 0 and member_id == _owner:
		_fail("the host left the game")


func _on_join_requested(lobby_id: int) -> void:
	if _step != Step.IDLE:
		status.emit("a Steam join request arrived while busy; leave first")
		return
	if not _steam_ready():
		return
	_join_lobby(lobby_id, "joining a friend's game...")
