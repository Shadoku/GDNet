class_name NetMeter
extends CanvasLayer
## The gdnet network meter (docs/PLAN_NETCODE.md 1 "What the developer sees", WP8): plain `Label`s
## refreshed four times a second from a session's `stats()`. Shown by `Net.show_meter(true)` (or the
## setting `gdnet/debug/meter`, or the action `gdnet_toggle_meter` / F9 while `Net` has a session).

## The session shown; null = `Net`'s default session, else the first session in the tree.
@export var session: NetSession
## Seconds between refreshes.
@export var interval: float = 0.25

## The `stats()` Dictionary of the last refresh (empty while no session runs).
var last_stats: Dictionary = {}

const LINES: Array[String] = [
	"rtt", "tick", "buffer", "in", "out", "entities", "state", "budget", "calls", "prediction",
	"poll", "hashes", "authority",
]

var _labels: Dictionary = {}
var _since: float = 0.0

@onready var _box: VBoxContainer = $Panel/Lines


func _ready() -> void:
	layer = 128
	for line in LINES:
		var l := Label.new()
		l.name = line.capitalize().replace(" ", "")
		l.add_theme_font_size_override("font_size", 12)
		_box.add_child(l)
		_labels[line] = l
	refresh()


func _process(delta: float) -> void:
	_since += delta
	if _since >= interval:
		_since = 0.0
		refresh()


## The session the meter reads now.
func current_session() -> NetSession:
	if session != null and is_instance_valid(session):
		return session
	var s: NetSession = Net.get_session()
	if s != null:
		return s
	for n in get_tree().get_nodes_in_group("__gdnet_sessions"):
		if n is NetSession and (n as NetSession).is_online():
			return n
	return null


## Reads the session's stats and updates every label.
func refresh() -> void:
	var s := current_session()
	if s == null or not s.is_online():
		last_stats = {}
		_put("rtt", "gdnet: no session")
		for line in LINES.slice(1):
			_put(line, "")
		return
	var d: Dictionary = s.stats()
	last_stats = d
	var bi: PackedInt64Array = d.get("bytes_received_per_s", PackedInt64Array([0, 0, 0]))
	var bo: PackedInt64Array = d.get("bytes_sent_per_s", PackedInt64Array([0, 0, 0]))
	_put("rtt", "RTT %.1f ms  jitter %.1f ms" % [d.get("rtt_ms", -1.0), d.get("jitter_ms", -1.0)])
	_put("tick", "tick %d  server offset %+.1f  %s" % [d.get("tick", 0), d.get("server_tick_offset", 0.0), "server" if s.is_server() else "client"])
	_put("buffer", "jitter buffer %d  render lag %.1f ticks" % [d.get("jitter_buffer_ticks", 0), d.get("render_lag_ticks", 0.0)])
	_put("in", "in  %6d B/s  (ch0 %d  ch1 %d  ch2 %d)" % [bi[0] + bi[1] + bi[2], bi[0], bi[1], bi[2]])
	_put("out", "out %6d B/s  (ch0 %d  ch1 %d  ch2 %d)" % [bo[0] + bo[1] + bo[2], bo[0], bo[1], bo[2]])
	_put("entities", "entities %d  in scope %d" % [d.get("entities", 0), d.get("entities_in_scope", 0)])
	_put("state", "snapshots %d B/s (%d total)  keyed %d B/s" % [d.get("snapshot_bytes_per_s", 0), d.get("snapshot_bytes", 0), d.get("keyed_bytes_per_s", 0)])
	_put("budget", "budget %d%% of %d B/s per peer" % [roundi(100.0 * float(d.get("budget_use", 0.0))), d.get("budget_bytes_per_s", 0)])
	_put("calls", "calls out %d/s  in %d/s  refused %d" % [d.get("calls_sent_per_s", 0), d.get("calls_received_per_s", 0), d.get("calls_refused", 0)])
	_put("prediction", "rollbacks %d/s  replayed %d  inputs repeated %d" % [d.get("rollbacks_per_s", 0), d.get("replayed_ticks", 0), d.get("inputs_repeated", 0)])
	_put("poll", "poll %.3f ms (avg %.3f)" % [d.get("poll_ms", 0.0), d.get("poll_ms_avg", 0.0)])
	_put("hashes", "hashes out %d  in %d  divergences %d (%d/s)%s" % [d.get("hashes_sent", 0), d.get("hashes_received", 0), d.get("divergences", 0), d.get("divergences_per_s", 0), "" if d.get("hash_telemetry", true) else "  (off)"])
	_put("authority", "owner changes %d  rejected writes %d" % [d.get("owner_changes", 0), d.get("writes_rejected", 0)])


## Every label's text, one per line (for tests and tools).
func text() -> String:
	var out: PackedStringArray = []
	for line in LINES:
		out.append((_labels[line] as Label).text)
	return "\n".join(out)


func _put(line: String, value: String) -> void:
	var l: Label = _labels.get(line)
	if l != null:
		l.text = value
