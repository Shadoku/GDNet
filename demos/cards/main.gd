extends Control
## The title screen is gdnet's connect menu (`addons/gdnet/connect/`): Host, Host on Steam, Join (an
## address or a join code) or Offline (against the house). Above it, a name to sit down with and, when a
## saved game exists, New game. `--server` after `--` on the command line starts a dedicated server:
## `godot --headless --path . demos/cards/main.tscn -- --server`.

const SAVE := "user://cards_save.bin"

@onready var table: CardsTable = $Table
@onready var menu: NetConnectMenu = $Menu
@onready var _name: LineEdit = $Seat/Name
@onready var _new_game: Button = $Seat/NewGame

var _saved: Dictionary = {}


func _ready() -> void:
	menu.started.connect(_start)
	_name.text_changed.connect(_use_name)
	_new_game.pressed.connect(_forget_save)
	_saved = CardsDealer.read_save(SAVE)
	_new_game.visible = not _saved.is_empty()
	if not _saved.is_empty():
		menu.connector.state = _saved["session"]  # hosting goes on with the saved game
	if "--server" in OS.get_cmdline_user_args():
		Net.dedicated = true
		menu.host()


## A name is the seat key: type the same one after a restart to sit down again. Empty: over Steam the
## Steam id, over ENet a key that lasts until the game closes.
func _use_name(text: String) -> void:
	menu.connector.key = "" if text.strip_edges().is_empty() else "name:" + text.strip_edges()


func _forget_save() -> void:
	DirAccess.remove_absolute(ProjectSettings.globalize_path(SAVE))
	_saved = {}
	menu.connector.state = PackedByteArray()
	_new_game.hide()


func _start() -> void:
	$Seat.hide()
	table.show()
	table.dealer.save_path = SAVE
	table.dealer.house = menu.connector.transport() == "offline"
	table.dealer.start(_saved.get("game", {}))
	table.start()
