extends Node2D
## Test fixture (WP3): a scene root with an exported value and a plain (non-exported) one, for
## `Net.configure()` tests.

## Exported: synced by default.
@export var hp: int = 10
## Not exported: synced only when `Net.configure(node, {"state": ["+phase"]})` adds it.
var phase: int = 0
