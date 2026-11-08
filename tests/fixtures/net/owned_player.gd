extends "res://tests/fixtures/net/predicted_player.gd"
## Test fixture (WP7): rung 1's player (predicted_player.gd) plus the authority hooks the tests read.

## Every `_net_owner_changed(old, new)` call, as [old, new].
var owner_changes: Array = []
## What `_net_grant_owner` answers when this copy's owner is asked for the entity.
var grant: bool = false
## The peers that asked.
var asked_by: Array[int] = []


func _net_owner_changed(old: int, new: int) -> void:
	owner_changes.append([old, new])


func _net_grant_owner(requester: int) -> bool:
	asked_by.append(requester)
	return grant
