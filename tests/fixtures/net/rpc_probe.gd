extends Node
## A stock @rpc target used by the netcode tests (gate G1): no gdnet API, just Godot's.

## [sender id, value, physics frame] per call received.
var received: Array = []


@rpc("any_peer", "call_remote", "reliable")
func ping(value: int) -> void:
	received.append([multiplayer.get_remote_sender_id(), value, Engine.get_physics_frames()])
