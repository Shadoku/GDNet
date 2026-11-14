extends Node2D
## Test fixture (WP6): rung 2's door of docs/PLAN_NETCODE.md section 1, plus records of what ran on
## this peer. Nothing here is declared for the network: every public method is callable.

## Opened by `open_door` (an @export, so it also replicates as state).
@export var open: bool = false
@export var opened_id: int = -1

## Every execution on this peer: [method, argument...].
var calls: Array = []
## The NetCallInfo each `note` call received: {text, sender, tick, is_local}.
var infos: Array = []
## The `_net_allow` questions asked on this peer: [method, sender].
var asked: Array = []
## How many calls had run here when `_net_spawned` ran (-1 before it ran).
var calls_at_spawn: int = -1


func _net_spawned(_info: Dictionary) -> void:
	calls_at_spawn = calls.size()


func open_door(id: int) -> void:
	open = true
	opened_id = id
	calls.append(["open_door", id])


func try_buy(item_id: int) -> bool:
	calls.append(["try_buy", item_id])
	return item_id == 3


func note(text: String, info: NetCallInfo) -> void:
	infos.append({"text": text, "sender": info.sender, "tick": info.tick, "is_local": info.is_local})


func take(res: Resource) -> void:
	calls.append(["take", res.resource_path])


func knock(k: int) -> void:
	calls.append(["knock", k])


func _net_allow(method: StringName, sender: int) -> bool:
	asked.append([String(method), sender])
	return allows_others()


## What `_net_allow` answers for a sender that does not own the door (process-wide; tests reset it). Kept
## in an Engine meta, not a `static var`: a script with static variables can be reported as a resource
## still in use at exit, depending on which test files share a runner process.
static func allows_others() -> bool:
	return bool(Engine.get_meta(&"_door_allow_others", false))


static func set_allow_others(on: bool) -> void:
	Engine.set_meta(&"_door_allow_others", on)


func _secret() -> void:
	calls.append(["secret"])
