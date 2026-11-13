class_name NetConnectMenu
extends VBoxContainer
## The title screen the demos share (WP23, docs/PLAN_NETCODE.md 12.10; docs/guide/12_playing_online.md):
## Host (ENet on `port`), Host on Steam (shown only where Steam is available; then the join code with a
## Copy button), Join (one field: a join code joins on Steam, anything else is an address, `host` or
## `host:port`), Offline, and a status line. `started` fires once a session runs; the menu then hides its
## buttons and keeps only the join code (when hosting on Steam) and the status line, in the bottom-left
## corner. All the work is done by its `NetConnect` child, `connector`.

## A session runs (hosted, joined or offline). The game starts here, as after `Net.host` / `Net.join`.
signal started()

## The heading.
@export var title: String = "gdnet"
## The ENet port to host on, and to join when the field names no port.
@export var port: int = 7350

## The connect layer behind the buttons (replace its `backend` before the menu enters the tree to test).
@onready var connector: NetConnect = $Connect
@onready var _address: LineEdit = $Address
@onready var _status: Label = $Status
@onready var _code_row: HBoxContainer = $CodeRow


func _ready() -> void:
	($Title as Label).text = title
	($Host as Button).pressed.connect(host)
	($HostSteam as Button).pressed.connect(connector.host_steam)
	($Join as Button).pressed.connect(join)
	_address.text_submitted.connect(func(_text: String) -> void: join())
	($Offline as Button).pressed.connect(connector.offline)
	($CodeRow/Copy as Button).pressed.connect(func() -> void: DisplayServer.clipboard_set(connector.code()))
	connector.status.connect(func(text: String) -> void: _status.text = text)
	connector.hosted.connect(func(_code: String) -> void: _on_started())
	connector.joined.connect(_on_started)
	connector.failed.connect(func(reason: String) -> void: print("gdnet: ", reason))
	var steam := connector.available().has("steam")
	$HostSteam.visible = steam
	_address.placeholder_text = "address or join code" if steam else "address"
	if not steam:
		_status.text = "Steam: " + connector.steam_reason()


## Hosts over ENet on `port` (the demos' `--server` path calls this).
func host() -> Error:
	return connector.host_enet(port)


## Joins what the field holds: a join code on Steam, otherwise an address over ENet.
func join() -> Error:
	if NetConnect.is_join_code(_address.text):
		return connector.join_steam(_address.text)
	var a := NetConnect.parse_address(_address.text, port)
	return connector.join_enet(a["address"], a["port"])


func _on_started() -> void:
	for child: Control in [$Title, $Host, $HostSteam, _address, $Join, $Offline]:
		child.hide()
	_status.text = ""
	_code_row.visible = not connector.code().is_empty()
	($CodeRow/Code as Label).text = "Join code: %s" % connector.code()
	set_anchors_and_offsets_preset(Control.PRESET_BOTTOM_LEFT, Control.PRESET_MODE_MINSIZE, 8)
	started.emit()
