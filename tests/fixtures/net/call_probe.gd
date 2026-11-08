extends Node2D
## Test fixture (WP6, gate G6): counts the calls it receives on this peer.

## The keys of every `hit` call that ran here, in order.
var hits: Array[int] = []


func hit(k: int) -> void:
	hits.append(k)
