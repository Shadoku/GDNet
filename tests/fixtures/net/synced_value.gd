extends Node
## A node whose `value` a stock MultiplayerSynchronizer replicates from the server (gate G1).

@export var value: int = 0


func _init() -> void:
	var sync := MultiplayerSynchronizer.new()
	sync.name = "Sync"
	var config := SceneReplicationConfig.new()
	config.add_property(NodePath(".:value"))
	config.property_set_replication_mode(NodePath(".:value"), SceneReplicationConfig.REPLICATION_MODE_ALWAYS)
	sync.replication_config = config
	sync.root_path = NodePath("..")
	add_child(sync)
