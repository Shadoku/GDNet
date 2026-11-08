extends PanelContainer
## A chat box on the keyed state. Every message is a path under `chat/`, and the keyed state is
## global, so a message reaches every room.

@onready var log_label: RichTextLabel = $Box/Log
@onready var input: LineEdit = $Box/Input

var _state: NetKeyedState


## Called once the session runs.
func start() -> void:
	_state = Net.session_for(self).state
	_state.changed.connect(_on_changed)
	for path in _state.keys("chat/"):
		_on_changed(path, _state.get(path))
	input.text_submitted.connect(send)


func send(text: String) -> void:
	if text.strip_edges().is_empty():
		return
	var path := "chat/%06d" % _state.keys("chat/").size()
	_state.set(path, "%d: %s" % [multiplayer.get_unique_id(), text])
	input.clear()


func _on_changed(path: String, value: Variant) -> void:
	if path.begins_with("chat/"):
		log_label.append_text("%s\n" % value)
