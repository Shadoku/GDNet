extends CharacterBody2D
## Test fixture (WP4): custom input values. On the owner `_net_input()` writes "aim" into the frame and
## the `NetEntity` lists `cursor` as an input property; the tick reads both on the server.

## What the owner's `_net_input()` sends (the test sets it on the owner's copy).
var aim_source: Vector2 = Vector2.ZERO
## What the tick read with `Net.input(self).get("aim")`.
var aim_seen: Vector2 = Vector2.ZERO
## An input property: read on the owner after `_net_input()`, written on the server before the tick.
var cursor: Vector2 = Vector2.ZERO
var net_input_calls: int = 0


func _net_input() -> void:
	net_input_calls += 1
	Net.input(self).set("aim", aim_source)


func _physics_process(_delta: float) -> void:
	var v: Variant = Net.input(self).get("aim")
	if v != null:
		aim_seen = v
