class_name NetSteamBackend
extends RefCounted
## Every call gdnet's connect layer makes into Steam (WP23, docs/PLAN_NETCODE.md 12.10). Steam is reached
## through the GodotSteam GDExtension (`addons/godotsteam/`, installed by `tools/install_godotsteam.sh`,
## never committed), only by name: `Engine.get_singleton("Steam")`, `ClassDB.instantiate(
## "SteamMultiplayerPeer")` and `Object.call()`. Nothing here refers to a GodotSteam class statically, so
## the project parses, imports and runs without the addon; then `start()` returns the reason and
## `NetConnect.available()` lacks "steam".
##
## NetConnect holds one of these in its `backend` field; the tests hand it a fake
## (`tests/fixtures/connect/fake_steam_backend.gd`) that overrides every method below. Answers from Steam
## arrive as this object's signals, emitted while `poll()` runs Steam's callbacks.
##
## Checked 2026-11-13 against the GodotSteam GDExtension 4.24 library loaded in Godot 4.6 (`ClassDB`
## lists every method and signal below with these arguments, and `steamInitEx` returns the dictionary
## described); not yet run against a Steam client. Each name is used only in this file. The names: `steamInitEx(app_id, embed_callbacks)` (older builds:
## `steamInitEx(retrieve_stats, app_id, embed_callbacks)`, detected below) returning
## `{"status": 0 on success, "verbal": String}`; `run_callbacks`, `isSteamRunning`, `loggedOn`,
## `getSteamID`; `createLobby(type, max_members)` -> signal `lobby_created(result, lobby_id)` (result 1 =
## OK); `setLobbyData`, `getLobbyData`, `setLobbyJoinable`; `addRequestLobbyListStringFilter(key, value,
## comparison)` (0 = equal), `addRequestLobbyListDistanceFilter(3)` (worldwide), `requestLobbyList()` ->
## signal `lobby_match_list(lobbies: Array)`; `joinLobby(lobby_id)` -> signal `lobby_joined(lobby_id,
## permissions, locked, response)` (response 1 = success); `leaveLobby`, `getLobbyOwner`; signals
## `lobby_chat_update(lobby_id, changed_id, making_change_id, chat_state)` and `join_requested(lobby_id,
## steam_id)`; `SteamMultiplayerPeer.create_host(virtual_port)` and `create_client(steam_id,
## virtual_port)` returning an `Error`.

## Steam created our lobby (`error` empty) or could not (`lobby_id` 0, `error` says why).
signal lobby_created(lobby_id: int, error: String)
## The answer to `find_lobbies`: the ids of the matching lobbies (maybe none).
signal lobby_list(lobby_ids: Array)
## We are in the lobby (`error` empty), or Steam refused (`error` says why: full, gone...).
signal lobby_joined(lobby_id: int, error: String)
## A member left (or lost its connection to) a lobby we are in.
signal lobby_member_left(lobby_id: int, member_id: int)
## The user accepted an invite or chose "Join game" on a friend in the Steam overlay.
signal join_requested(lobby_id: int)

## Steam's lobby types (`ELobbyType`). Join codes need a public lobby: only public lobbies are returned
## by a lobby search.
const LOBBY_PUBLIC := 2
const COMPARISON_EQUAL := 0
const DISTANCE_WORLDWIDE := 3
## `EChatMemberStateChange` bits meaning "this member is gone".
const MEMBER_GONE := 2 | 4 | 8 | 16

## The engine singleton GodotSteam registers. Tests point it at a name that does not exist.
var singleton_name: String = "Steam"
## Why the last `host_peer()` / `client_peer()` returned null.
var last_error: String = ""

var _steam: Object = null
var _embedded_callbacks: bool = false


## Initialises Steam with `app_id`. Returns "" when Steam is usable (GodotSteam present, initialised, a
## Steam client running and logged in), otherwise the reason, for the menu's status line.
func start(app_id: int) -> String:
	if not Engine.has_singleton(singleton_name):
		return "the GodotSteam addon is not installed (run tools/install_godotsteam.sh)"
	if not ClassDB.class_exists("SteamMultiplayerPeer"):
		return "this GodotSteam build has no SteamMultiplayerPeer"
	var steam: Object = Engine.get_singleton(singleton_name)
	_embedded_callbacks = bool(ProjectSettings.get_setting("steam/initialization/embed_callbacks", false))
	var result: Variant = steam.callv("steamInitEx", _init_args(steam, app_id))
	if result is Dictionary and int(result.get("status", 0)) != 0:
		return "Steam did not start: %s (is the Steam client running?)" % String(result.get("verbal", "unknown error"))
	if not bool(steam.call("isSteamRunning")):
		return "the Steam client is not running"
	if not bool(steam.call("loggedOn")):
		return "the Steam client is not logged in"
	_steam = steam
	_steam.connect("lobby_created", _on_lobby_created)
	_steam.connect("lobby_match_list", _on_lobby_match_list)
	_steam.connect("lobby_joined", _on_lobby_joined)
	_steam.connect("lobby_chat_update", _on_lobby_chat_update)
	_steam.connect("join_requested", _on_join_requested)
	return ""


## Runs Steam's callbacks (call once per frame; NetConnect does). A no-op when GodotSteam runs them itself
## (the project setting `steam/initialization/embed_callbacks`).
func poll() -> void:
	if _steam != null and not _embedded_callbacks:
		_steam.call("run_callbacks")


## This user's 64-bit Steam id (0 before `start()` succeeded).
func my_id() -> int:
	return 0 if _steam == null else int(_steam.call("getSteamID"))


## Creates a public lobby for `max_members`; answers with `lobby_created`.
func create_lobby(max_members: int) -> void:
	_steam.call("createLobby", LOBBY_PUBLIC, max_members)


func set_lobby_data(lobby_id: int, key: String, value: String) -> void:
	_steam.call("setLobbyData", lobby_id, key, value)


func lobby_data(lobby_id: int, key: String) -> String:
	return String(_steam.call("getLobbyData", lobby_id, key))


## Searches lobbies of this app whose data equals every `key: value` of `filters`, worldwide; answers
## with `lobby_list`.
func find_lobbies(filters: Dictionary) -> void:
	for key: String in filters:
		_steam.call("addRequestLobbyListStringFilter", key, String(filters[key]), COMPARISON_EQUAL)
	_steam.call("addRequestLobbyListDistanceFilter", DISTANCE_WORLDWIDE)
	_steam.call("requestLobbyList")


## Joins a lobby; answers with `lobby_joined`.
func join_lobby(lobby_id: int) -> void:
	_steam.call("joinLobby", lobby_id)


func leave_lobby(lobby_id: int) -> void:
	if _steam != null:
		_steam.call("leaveLobby", lobby_id)


## The lobby owner's Steam id (0 when unknown).
func lobby_owner(lobby_id: int) -> int:
	return int(_steam.call("getLobbyOwner", lobby_id))


## A `SteamMultiplayerPeer` hosting (unique id 1), or null with `last_error` set.
func host_peer() -> MultiplayerPeer:
	var peer: MultiplayerPeer = ClassDB.instantiate("SteamMultiplayerPeer")
	var err: int = int(peer.call("create_host", 0))
	if err != OK:
		last_error = error_string(err)
		return null
	return peer


## A `SteamMultiplayerPeer` connecting to the Steam user `owner_id`, or null with `last_error` set.
func client_peer(owner_id: int) -> MultiplayerPeer:
	var peer: MultiplayerPeer = ClassDB.instantiate("SteamMultiplayerPeer")
	var err: int = int(peer.call("create_client", owner_id, 0))
	if err != OK:
		last_error = error_string(err)
		return null
	return peer


## steamInitEx's arguments: GodotSteam 4.x dropped a leading `retrieve_stats` argument at some point, so
## the method's own argument list decides.
func _init_args(steam: Object, app_id: int) -> Array:
	for m: Dictionary in steam.get_method_list():
		if m["name"] == "steamInitEx":
			var args: Array = m["args"]
			if not args.is_empty() and String(args[0]["name"]) == "retrieve_stats":
				return [false, app_id, false]
			break
	return [app_id, false]


func _on_lobby_created(result: int, lobby_id: int) -> void:
	if result == 1:
		_steam.call("setLobbyJoinable", lobby_id, true)
		lobby_created.emit(lobby_id, "")
	else:
		lobby_created.emit(0, "Steam could not create a lobby (result %d)" % result)


func _on_lobby_match_list(lobbies: Array) -> void:
	lobby_list.emit(lobbies)


func _on_lobby_joined(lobby_id: int, _permissions: int, _locked: bool, response: int) -> void:
	lobby_joined.emit(lobby_id, "" if response == 1 else _join_response(response))


func _on_lobby_chat_update(lobby_id: int, changed_id: int, _making_change_id: int, chat_state: int) -> void:
	if chat_state & MEMBER_GONE:
		lobby_member_left.emit(lobby_id, changed_id)


func _on_join_requested(lobby_id: int, _friend_id: int) -> void:
	join_requested.emit(lobby_id)


## `EChatRoomEnterResponse` in words.
static func _join_response(response: int) -> String:
	match response:
		2:
			return "the game no longer exists"
		3:
			return "the game does not let you in"
		4:
			return "the game is full"
		6:
			return "you are banned from this game"
		_:
			return "Steam refused to join the game (response %d)" % response
