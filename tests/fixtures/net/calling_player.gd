extends "res://tests/fixtures/net/predicted_player.gd"
## Test fixture (WP6): the predicted player of WP4, which also calls `Net.sync(bump)` on the tick its
## owner presses "jump", from inside `_physics_process` (so a replay of that tick calls it again).

## Executions of `bump` on this peer.
var bumps: int = 0
## Calls of `Net.sync(bump)` that were not suppressed (sent or run).
var sent: int = 0
## Calls of `Net.sync(bump)` that were suppressed (inside a replay).
var suppressed: int = 0


func _count() -> void:
	super()
	if Net.is_owner(self) and Net.input(self).is_action_just_pressed("jump"):
		var c: NetCall = Net.sync(bump)
		if c.suppressed:
			suppressed += 1
		else:
			sent += 1


func bump() -> void:
	bumps += 1
