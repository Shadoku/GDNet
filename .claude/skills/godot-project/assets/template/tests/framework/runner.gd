extends SceneTree
## Headless unit-test runner. No addons required.
##
##   godot --headless --path . -s tests/framework/runner.gd [-- [--dir=<res path>] [--filter=<substr>] [--verbose] [--junit=<path>] [--jobs=<n>]]
##
## Discovers res://tests/**/test_*.gd (or only those under `--dir`, e.g. `--dir=res://tests/unit`), instantiates each (must extend TestCase) once per
## test_* method, runs before_each / test / after_each with a per-test timeout, and
## prints one line per failure plus a summary. Exit code 0 only if every test passed.
##
## How runtime errors are caught: the runner re-launches Godot as a child process
## (the "inner" run) and reads its combined stdout/stderr. Any ERROR / SCRIPT ERROR
## printed while a test is running is attributed to that test and fails it, unless the
## test sets `allow_errors = true`. This is why "RESULT: PASS" can be trusted.
##
## `--jobs=N` (N > 1) splits the test files over N inner processes running at once (whole files per
## process, so a file's tests keep their order and process). Each inner writes its log with Godot's
## `--log-file`, which keeps prints, markers and engine errors in one ordered stream; the outer drains
## the pipes and parses the logs. Files are balanced by the durations of the last run (kept in
## `user://test_durations.json`), else by size. Tests sleep on wall-clock time far more than they use
## the CPU, so two or three jobs roughly halve or third the stage. Needs Godot 4.4+ (older: one process).

const TEST_ROOT := "res://tests"

var _inner := false
var _dir := TEST_ROOT
var _filter := ""
var _verbose := false
var _junit_path := ""
var _jobs := 1
var _files: PackedStringArray = []
var _results: Array[Dictionary] = []
var _orphan_errors: Array[String] = []

func _initialize() -> void:
	_parse_args()
	if _inner:
		_run_inner()
	else:
		_run_outer()

func _parse_args() -> void:
	for arg in OS.get_cmdline_user_args():
		if arg == "--inner":
			_inner = true
		elif arg.begins_with("--dir="):
			_dir = arg.trim_prefix("--dir=").trim_suffix("/")
			if not _dir.begins_with("res://"):
				_dir = "res://" + _dir
		elif arg.begins_with("--filter="):
			_filter = arg.trim_prefix("--filter=")
		elif arg == "--verbose":
			_verbose = true
		elif arg.begins_with("--junit="):
			_junit_path = arg.trim_prefix("--junit=")
		elif arg.begins_with("--jobs="):
			_jobs = maxi(1, int(arg.trim_prefix("--jobs=")))
		elif arg.begins_with("--files="):
			_files = arg.trim_prefix("--files=").split(",", false)

# --- outer process: spawn inner, parse its output, report ---------------------------

func _run_outer() -> void:
	# Non-blocking pipes and exit codes of spawned processes need Godot 4.4.
	if _jobs > 1 and Engine.get_version_info().hex >= 0x040400:
		_run_outer_parallel()
		return
	var project_dir := ProjectSettings.globalize_path("res://")
	var args := ["--headless", "--path", project_dir, "-s", get_script().resource_path, "--", "--inner"]
	args.append_array(OS.get_cmdline_user_args())
	var output := []
	var code := OS.execute(OS.get_executable_path(), args, output, true, false)
	var text := ""
	for chunk in output:
		text += str(chunk)
	_parse_inner_output(text)
	_report(code)

const DURATIONS := "user://test_durations.json"

## `--jobs=N`: the files spread over N inner processes (longest first onto the least loaded one).
func _run_outer_parallel() -> void:
	var project_dir := ProjectSettings.globalize_path("res://")
	var scripts := _find_test_scripts(_dir)
	scripts.sort()
	var known: Dictionary = {}
	if FileAccess.file_exists(DURATIONS):
		var parsed = JSON.parse_string(FileAccess.get_file_as_string(DURATIONS))
		if parsed is Dictionary:
			known = parsed
	var weights := {}
	for path in scripts:
		var key := path.trim_prefix("res://")
		weights[path] = float(known[key]) if known.has(key) else float(FileAccess.get_file_as_bytes(path).size())
	var order := scripts.duplicate()
	order.sort_custom(func(a: String, b: String) -> bool: return weights[a] > weights[b])
	var jobs := mini(_jobs, maxi(1, scripts.size()))
	var shards: Array[PackedStringArray] = []
	var loads: Array[float] = []
	for i in jobs:
		shards.append(PackedStringArray())
		loads.append(0.0)
	for path in order:
		var best := 0
		for i in jobs:
			if loads[i] < loads[best]:
				best = i
		shards[best].append(path)
		loads[best] += weights[path]
	var user_args: PackedStringArray = []
	for a in OS.get_cmdline_user_args():
		if not a.begins_with("--jobs=") and not a.begins_with("--junit="):
			user_args.append(a)
	var running: Array[Dictionary] = []
	for i in jobs:
		if shards[i].is_empty():
			continue
		var log_path := OS.get_cache_dir().path_join("godot_test_shard_%d_%d.log" % [OS.get_process_id(), i])
		if FileAccess.file_exists(log_path):
			DirAccess.remove_absolute(log_path)
		var args := ["--headless", "--path", project_dir, "--log-file", log_path, "-s", get_script().resource_path, "--", "--inner", "--files=" + ",".join(shards[i])]
		args.append_array(user_args)
		var p: Dictionary = OS.call("execute_with_pipe", OS.get_executable_path(), args, false)
		if p.is_empty():
			_orphan_errors.append("could not start an inner test process")
			continue
		running.append({"pid": p["pid"], "stdio": p["stdio"], "stderr": p["stderr"], "log": log_path, "done": false, "code": 0})
	var left := running.size()
	while left > 0:
		for r in running:
			if r.done:
				continue
			# Drain the pipes (the log file holds the same text, in order) so the child never blocks.
			(r.stdio as FileAccess).get_buffer(1 << 16)
			(r.stderr as FileAccess).get_buffer(1 << 16)
			if not OS.is_process_running(r.pid):
				r.done = true
				r.code = OS.call("get_process_exit_code", r.pid)
				left -= 1
		OS.delay_msec(20)
	var code := 0
	for r in running:
		(r.stdio as FileAccess).close()
		(r.stderr as FileAccess).close()
		var text := FileAccess.get_file_as_string(r.log)
		DirAccess.remove_absolute(r.log)
		_parse_inner_output(text)
		if r.code != 0:
			code = r.code
	# Report in file order (each file's tests in their own order), and keep the durations.
	var by_file := {}
	for res in _results:
		var file: String = res.id.split("::")[0]
		if not by_file.has(file):
			by_file[file] = []
		by_file[file].append(res)
	var ordered: Array[Dictionary] = []
	var durations := known.duplicate()
	for path in scripts:
		var key := path.trim_prefix("res://")
		var total := 0
		for res in by_file.get(key, []):
			ordered.append(res)
			total += int(res.ms)
		if by_file.has(key) and _filter == "":
			durations[key] = total
	_results = ordered
	var f := FileAccess.open(DURATIONS, FileAccess.WRITE)
	if f != null:
		f.store_string(JSON.stringify(durations))
	_report(code)

var _cur: Dictionary = {}
var _err_block: PackedStringArray = []

func _parse_inner_output(text: String) -> void:
	var found_marker := false
	for line in text.split("\n"):
		if line.begins_with("::"):
			_flush_error()
			found_marker = true
			var parts := line.split(" ", true, 1)
			var kind := parts[0]
			var rest := parts[1] if parts.size() > 1 else ""
			match kind:
				"::begin":
					_cur = {"id": rest, "status": "error", "ms": 0, "messages": PackedStringArray(), "errors": PackedStringArray(), "output": PackedStringArray(), "allow_errors": false}
				"::msg":
					if not _cur.is_empty():
						_cur.messages.append(rest)
				"::end":
					var f := rest.split(" ")
					if not _cur.is_empty() and f.size() >= 4:
						_cur.status = f[1]
						_cur.ms = int(f[2])
						_cur.allow_errors = f[3] == "1"
						_results.append(_cur)
					_cur = {}
				"::shard":
					pass
				"::load_error":
					_results.append({"id": rest + "::<load>", "status": "error", "ms": 0, "messages": PackedStringArray(["script failed to load or does not extend TestCase (see errors above)"]), "errors": PackedStringArray(), "output": PackedStringArray(), "allow_errors": false})
		elif line.begins_with("SCRIPT ERROR:") or line.begins_with("ERROR:") or line.begins_with("USER ERROR:"):
			_flush_error()
			_err_block.append(line)
		elif not _err_block.is_empty() and (line.begins_with(" ") or line.begins_with("\t")):
			_err_block.append(line)
		else:
			_flush_error()
			if not _cur.is_empty() and line.strip_edges() != "":
				_cur.output.append(line)
	_flush_error()
	if not _cur.is_empty():
		_cur.status = "error"
		_cur.messages.append("test did not finish (inner process died or was killed)")
		_results.append(_cur)
		_cur = {}
	if not found_marker:
		_orphan_errors.append("Inner test process produced no results. Raw output:\n" + text.strip_edges())

func _flush_error() -> void:
	if _err_block.is_empty():
		return
	var block := "\n".join(_err_block)
	if _cur.is_empty():
		_orphan_errors.append(block)
	else:
		_cur.errors.append(block)
	_err_block = PackedStringArray()

func _report(inner_code: int) -> void:
	var counts := {"pass": 0, "fail": 0, "skip": 0, "error": 0}
	for r in _results:
		if r.status in ["pass", "skip"] and not r.errors.is_empty() and not r.allow_errors:
			r.status = "fail"
			r.messages.append("engine errors were printed during this test (set allow_errors = true if intended)")
		counts[r.status] += 1
		var tag: String = {"pass": "PASS", "fail": "FAIL", "skip": "SKIP", "error": "ERROR"}[r.status]
		if r.status != "pass" or _verbose:
			print("[%s] %s (%d ms)" % [tag, r.id, r.ms])
			for m in r.messages:
				print("    " + m)
			if r.status != "pass":
				for e in r.errors:
					print("    " + _strip_runner_frames(e).replace("\n", "\n    "))
			# A failing test's output always; a passing one's with --verbose (measurements, benchmarks).
			for o in r.output:
				print("    | " + o)
	for e in _orphan_errors:
		print("[ERROR] outside any test:")
		print("    " + e.replace("\n", "\n    "))
	print("")
	print("TESTS: %d passed, %d failed, %d errors, %d skipped (%d total)" % [counts.pass, counts.fail, counts.error, counts.skip, _results.size()])
	if _junit_path != "":
		_write_junit()
	var ok: bool = counts.fail == 0 and counts.error == 0 and _orphan_errors.is_empty() and _results.size() > 0
	if _results.is_empty() and _filter != "":
		print("No test under %s matches --filter=%s" % [_dir, _filter])
	elif _results.is_empty():
		print("No tests found. Put test_*.gd files extending TestCase under %s" % _dir)
	if inner_code != 0 and ok:
		print("Inner process exited with code %d" % inner_code)
		ok = false
	print("RESULT: %s" % ("PASS" if ok else "FAIL"))
	quit(0 if ok else 1)

## Backtrace frames inside the runner itself only add noise for the reader.
func _strip_runner_frames(block: String) -> String:
	var kept: PackedStringArray = []
	for line in block.split("\n"):
		if line.strip_edges().begins_with("[") and line.contains("tests/framework/runner.gd"):
			continue
		kept.append(line)
	return "\n".join(kept)

# --- inner process: actually run the tests, emit markers on stderr ------------------

func _run_inner() -> void:
	await process_frame  # root is not in the tree until the first frame
	var scripts := _find_test_scripts(_dir)
	scripts.sort()
	printerr("::shard")
	for path in scripts:
		if not _files.is_empty() and not _files.has(path):
			continue
		var script = load(path)
		if script == null or not (script is GDScript) or not script.can_instantiate():
			printerr("::load_error %s" % path.trim_prefix("res://"))
			continue
		var methods: Array[String] = []
		for m in script.get_script_method_list():
			if m.name.begins_with("test_") and not methods.has(m.name):
				methods.append(m.name)
		for method in methods:
			var id := "%s::%s" % [path.trim_prefix("res://"), method]
			if _filter != "" and not id.contains(_filter):
				continue
			await _run_test(script, path, id, method)
	quit(0)

func _run_test(script: GDScript, path: String, id: String, method: String) -> void:
	printerr("::begin %s" % id)
	var tc = script.new()
	if not tc is TestCase:
		if tc is Node:
			tc.free()
		printerr("::load_error %s" % path.trim_prefix("res://"))
		printerr("::end %s error 0 0" % id)
		return
	tc.name = method
	root.add_child(tc)
	var t0 := Time.get_ticks_msec()
	var timed_out := not await _call_with_timeout(tc, "before_each", tc.timeout_sec)
	if not timed_out and tc._failures.is_empty() and tc._skip_reason == "":
		timed_out = not await _call_with_timeout(tc, method, tc.timeout_sec)
	if not timed_out:
		timed_out = not await _call_with_timeout(tc, "after_each", tc.timeout_sec)
	var elapsed := Time.get_ticks_msec() - t0
	var status := "pass"
	for f in tc._failures:
		printerr("::msg %s" % f)
	if timed_out:
		status = "fail"
		printerr("::msg timed out after %.1fs (still awaiting something, or an earlier error aborted the coroutine)" % tc.timeout_sec)
	elif tc._skip_reason != "":
		status = "skip"
		printerr("::msg %s" % tc._skip_reason)
	elif not tc._failures.is_empty():
		status = "fail"
	var allow_errors: bool = tc.allow_errors
	tc._teardown()
	root.remove_child(tc)
	tc.free()
	printerr("::end %s %s %d %s" % [id, status, elapsed, "1" if allow_errors else "0"])

## Calls a method (sync or coroutine); returns false if it did not finish in time.
func _call_with_timeout(obj: Object, method: String, timeout: float) -> bool:
	if not obj.has_method(method):
		return true
	var done := [false]
	var run := func() -> void:
		await obj.call(method)
		done[0] = true
	run.call()
	var t0 := Time.get_ticks_msec()
	while not done[0]:
		# Re-read the timeout each frame so a test may set `timeout_sec` in its own body.
		var limit: float = obj.get("timeout_sec") if obj.get("timeout_sec") != null else timeout
		if Time.get_ticks_msec() - t0 > int(limit * 1000.0):
			return false
		await process_frame
	return true

func _find_test_scripts(dir_path: String) -> Array[String]:
	var found: Array[String] = []
	var dir := DirAccess.open(dir_path)
	if dir == null:
		return found
	dir.list_dir_begin()
	var entry := dir.get_next()
	while entry != "":
		var full := dir_path.path_join(entry)
		if dir.current_is_dir():
			if not entry.begins_with("."):
				found.append_array(_find_test_scripts(full))
		elif entry.begins_with("test_") and entry.ends_with(".gd"):
			found.append(full)
		entry = dir.get_next()
	dir.list_dir_end()
	return found

func _write_junit() -> void:
	var f := FileAccess.open(_junit_path, FileAccess.WRITE)
	if f == null:
		push_error("Could not write JUnit file: %s" % _junit_path)
		return
	var failures := 0
	var skipped := 0
	for r in _results:
		if r.status in ["fail", "error"]:
			failures += 1
		elif r.status == "skip":
			skipped += 1
	f.store_line('<?xml version="1.0" encoding="UTF-8"?>')
	f.store_line('<testsuite name="godot" tests="%d" failures="%d" skipped="%d">' % [_results.size(), failures, skipped])
	for r in _results:
		var id_parts: PackedStringArray = r.id.split("::")
		f.store_line('  <testcase classname="%s" name="%s" time="%.3f">' % [id_parts[0].xml_escape(), id_parts[-1].xml_escape(), r.ms / 1000.0])
		var msg: String = "\n".join(r.messages)
		if not r.errors.is_empty():
			msg += "\n" + "\n".join(r.errors)
		if r.status in ["fail", "error"]:
			f.store_line('    <failure message="%s"/>' % msg.xml_escape(true))
		elif r.status == "skip":
			f.store_line('    <skipped message="%s"/>' % msg.xml_escape(true))
		f.store_line('  </testcase>')
	f.store_line('</testsuite>')
	f.close()
