# Godot Project (Godot 4.6)

Godot 4 project set up for AI-assisted development: every change is verified by
`python3 tools/check.py`, which runs the stages listed in `check.toml` headless,
without the editor. Trust its output over your own reasoning about whether code works.

The project is the sample game and test bed for **gdnet**, a Rust GDExtension for
multiplayer that stays close to single-player development. Its design and work
packages are in `docs/PLAN_NETCODE.md` (approved 2026-11-08; the status table at
its end says what is built); the research behind it is
`docs/reports/Rust GDExtension multiplayer design.md` and `docs/research_notes/`.
Follow the plan's packages in order and update its status table when one lands.
Section 12 of the plan (v2, added 2026-11-13) holds the packages after v1 and their order. Priorities
(the owner's, plan goal 9): function, flexibility and keeping game code close to its single-player
version come first; cheat detection and prevention are not a priority inside the extension.

## Commands

| What | Command |
|---|---|
| Everything (build gdnet, import, validate, rust tests, unit and integration tests, lint, smoke, wasm) | `python3 tools/check.py` |
| Quick tier (build, import, validate, rust tests, unit tests; ~7 s; the inner loop) | `python3 tools/check.py --tier quick` |
| One test or test file, in every stage that has it | `python3 tools/check.py --filter <name>` (`<name>` is a substring of a GDScript test's `file::method` id, e.g. `test_health`, `test_net_prediction`, `g5_rate`, or of a Rust test's path) |
| One stage | `python3 tools/check.py --only <stage>` (`--only unit` and `--only integration` skip build and import: run `--tier quick` first after adding files or changing Rust) |
| One package's integration tests | `python3 tools/check.py --only integration --filter <name>` |
| A stage's full output (passing tests' `print()` too) | `python3 tools/check.py --only <stage> --verbose` |
| Run again although these exact files already passed | `python3 tools/check.py --fresh` |
| The stages and their tiers | `python3 tools/check.py --list` |
| Fewer commands side by side (also the integration stage's Godot processes, `{jobs}`) | `python3 tools/check.py --jobs 2` (default: the CPUs; 4 here, integration 40 s, the longest file's time) |
| One stage by hand, without the check's report | `python3 tools/godot.py <build\|import\|validate\|test\|smoke\|wasm>` (`test --dir tests/unit --filter X --jobs N`) |
| Run the game (needs a display) | `godot --path .` |
| Open the editor | `godot --path . --editor` |

`tools/check.py` is the language-agnostic check of the tdd-project kit: it runs the
stages listed in `check.toml`. The Godot and cargo work is in `tools/godot.py`, one
subcommand per stage, which judges Godot's output (Godot exits 0 even when scripts
print errors). It finds Godot via `$GODOT`, then `godot` on PATH, then
`~/.cache/godot/<version>/godot` (installed by `tools/install_godot.sh`, which
the Claude Code session hook and CI both use). The pinned version is in
`.godot-version`. It finds cargo via `$CARGO`, PATH, then `~/.cargo/bin/cargo`;
the `rust-test` and `lint` stages call cargo from PATH and are skipped
(`[SKIP] ... not installed`) without it, and `build` keeps an already installed
library. The `build` stage runs `cargo build --release -p gdnet` in `rust/` and copies
the library to `addons/gdnet/bin/<platform>/`, where `addons/gdnet/gdnet.gdextension`
expects it (never committed). A first build compiles gdext and takes several minutes;
later ones are incremental. The `wasm` stage (gate G3) passes with a `skipped` line
where the `wasm32-unknown-emscripten` target is not installed; CI installs it.

## The development loop

1. Before changing behaviour, write or adjust a test in `tests/` that describes the
   intended behaviour. Run it alone (`--filter <name>`) and watch it fail for the right
   reason: a test that has never failed has not shown that it can. After adding a
   script, a `class_name` or a resource, run `--tier quick` once first: `--filter` runs
   only the test stages, not build and import.
2. Implement the change, re-running that test (`--filter`) and `--tier quick`. Do not
   run the full check after every edit.
3. When the work is done, run `python3 tools/check.py` once. Read the output from the
   top: the first failing stage usually explains the rest (a parse error makes tests
   fail too).
4. Fix and re-run until the last line is `CHECK: PASS`.
5. Only then report done, quoting the `TESTS:` summary lines (unit and integration).

Never say a change works without a passing full check (`--tier quick` and `--filter`
runs do not count). If you cannot run Godot (no binary, no network), say so
explicitly instead of guessing.

## How the check stays fast

A check that takes minutes gets skipped or run rarely, and then it protects nothing.
These are the rules that keep it at seconds:

- **Tiers.** `--filter` (one test, about a second) for red and green; `--tier quick`
  (seconds; budget in `check.toml`) while implementing; the full check once, before
  reporting done. CI always runs the full check. A test belongs in `tests/unit` (the
  quick tier) only if it needs no scene tree session, physics, real clock or network
  fixture; everything that instantiates scenes or a `NetFixture` is `tests/integration`.
- **No waiting.** Tests must not sleep or wait on the real clock, a real network or a
  real service where a fake one will do: pass the clock, the random source and the
  services into the code and hand tests fake ones. The check flags a stage that takes
  long while using little CPU (`note: ... this stage mostly waits`): fix that first, it
  is usually worth more than any other speed-up. The integration stage spends most of
  its 40 s awaiting physics frames in real time (about 17 s of CPU, so under the flag's
  threshold); simulated time is its known next step.
- **Deterministic.** The same files must give the same result: seed randomness, fake
  time, and do not depend on test order or on state another test left behind (reset
  globals in teardown). A flaky test makes every pass doubtful and the pass cache unsafe.
- **Parallel.** Stages marked `parallel` run side by side (validate beside rust-test,
  integration beside lint), and the integration stage splits its files over `{jobs}`
  Godot processes. Tests therefore must not share ports, fixed temp paths or other
  machine-wide state. Two cargo stages never run side by side (`rust/target`'s lock), nor
  two Godot stages (the runner's `user://test_durations.json`, and CPU).
- **Pass cache.** A passing run stores a fingerprint of every project file (tracked or
  untracked, not ignored) and of the toolchain (`python3 tools/godot.py version`: Godot,
  cargo, rustc) under `~/.cache/check-pass/`. Running the check again on exactly the same
  files prints `[CACHED]`, the stored report and `CHECK: PASS` without running anything;
  one changed byte and it runs for real. Failures are never cached. `--fresh` ignores the
  cache: use it only when you doubt the cache itself (something outside the project
  changed that `check.toml`'s `[cache]` does not name, and then add it there), not to
  double-check.

### Subagents

- A subagent that changes files finishes with `python3 tools/check.py` and reports the
  summary and `CHECK:` lines; tell it so in its brief. While it works it uses `--filter`
  and `--tier quick` like anyone else.
- When its work comes back, run `python3 tools/check.py` yourself, without `--fresh`. If
  the files are what the subagent verified, the answer comes from the cache at once; if
  anything changed since (your own edits, a merge, another agent's work), the check runs
  for real, which is exactly when it is needed. Do not re-run tests "to be sure" in any
  other way.
- The cache is keyed by content, not by directory, so a subagent's pass in a worktree
  answers for the main checkout once it holds the same files.
- Several subagents editing the same checkout at once invalidate each other's runs (a
  pass is stored only if no file changed while the check ran): give them worktrees, or
  have them use `--filter` / `--tier quick` and run the full check once yourself.

## Reading the report

| Output | Meaning | What to do |
|---|---|---|
| `[FAIL] <stage> (..., exit N)` + output | the stage's command failed; the last lines of its output follow | read them; `--only <stage> --verbose` shows all of it |
| `[FAIL] <stage> (..., timed out after Ns)` | the command hung or is too slow | find what it waits for; raise `timeout` in `check.toml` only for a real reason |
| `[SKIP] <stage> (<other> failed)` | an earlier stage marked `stop_on_fail` (build, import) failed | fix that one first |
| `[SKIP] <stage> (not installed: tool)` | an optional tool (cargo, clippy, rustup) is missing here | fine locally; the pass is not a full one, so CI has the tool |
| `[NONE] <stage>: no test matches --filter=...` | that stage has no such test | fine if another stage matched; if none did the check fails: check the name |
| `[CACHED] these exact files passed ...` | nothing ran: the same files already passed | nothing; it is a pass. `--fresh` runs it again |
| `note: ... this stage mostly waits` | long wall time, little CPU | remove sleeps, fake the clock or the service |
| `note: the quick tier took ...; its budget is ...` | the inner loop is getting slow (expected once after a cold Rust build) | move slow tests to the full tier or make them faster |
| `note: this pass was not stored ...: files changed while the check ran (...)` | the check writes files that are neither ignored nor committed | add them to `.gitignore`, or commit them if they belong to the project |
| a test passes alone (`--filter`) but fails in the full run, or the other way round | it depends on order or on shared state | make it set up and reset its own state; `--jobs 1` runs one command (and one integration process) at a time |
| `[FAIL] validate` + `SCRIPT ERROR: Parse Error ... (res://x.gd:12)` | script does not compile | fix the line; static typing errors count |
| `Could not find type "Foo" in the current scope` | `class_name` not in the cache yet, or typo | the `import` stage rebuilds the cache (`--tier quick`, or `--only import`); if it persists the name is wrong |
| `[FAIL] tests/unit/test_x.gd::test_y` + `assert_eq failed: expected A, got B [file:line]` | assertion failure | look at the cited test line |
| `engine errors were printed during this test` | `push_error`, null access, missing node... during the test | the block below it has the message and backtrace |
| `timed out after 10.0s` | the test awaits something that never fires | check the signal or use `await_signal(sig, timeout)` |
| `[FAIL] smoke` | main scene printed an error in its first frames | the block shows file:line; often a bad node path or missing resource |
| `test did not finish (inner process died)` | Godot crashed | `--only <stage> --verbose` and look for the last line before the crash |
| `[FAIL] build` + `error[E...]` | the Rust side does not compile | fix the cited `rust/...` line; `cargo build -p gdnet` in `rust/` for the full output |
| `[FAIL] rust-test` + `panicked at rust/netcore/...` | a netcore unit test failed | `--filter <name>`, or `cargo test -p netcore <name>` in `rust/` |
| `[FAIL] lint` | `cargo fmt --check` or clippy (`-D warnings`) found something | `cargo fmt` in `rust/`; fix the cited clippy lint |
| `[FAIL] wasm` | the workspace no longer builds for `wasm32-unknown-emscripten` (gate G3) | a new dependency needs sockets, threads or wasm-bindgen: keep it out of netcore |
| `GDExtension dynamic library not found: 'res://addons/gdnet/gdnet.gdextension'` | the library was not built or copied | run the build stage (`--tier quick`); check `addons/gdnet/bin/<platform>/` |
| `NetVersion is missing` in `test_gdnet_loads` | the extension loaded but registered nothing, or an older library is installed | rebuild; check `rust/gdnet/src/lib.rs` registers the class |

## Writing tests

- Files: `tests/**/test_*.gd`, `extends TestCase`, methods named `test_*`.
- A fresh instance runs per test method: `before_each` / `after_each` for fixtures.
- Assertions: `assert_eq`, `assert_ne`, `assert_true`, `assert_false`, `assert_null`,
  `assert_not_null`, `assert_almost_eq`, `assert_gt`, `assert_lt`, `assert_has`, `fail`, `skip`.
- Tests are Nodes in the tree: `add_child(node)` to test scenes; children are
  freed automatically. `await wait_frames(n)`, `await wait_seconds(s)`,
  `await await_signal(sig, timeout)` for async behaviour.
- Any `ERROR`/`SCRIPT ERROR` printed while a test runs fails it. Set
  `allow_errors = true` in a test that deliberately triggers one.
- Run without `check.py`: `python3 tools/godot.py test --dir tests/unit --filter name --verbose`, or
  the runner itself: `godot --headless --path . -s tests/framework/runner.gd -- --dir=res://tests/unit --filter=name --verbose`
  (no `--dir` = every `tests/**/test_*.gd`).
  With `--verbose` a passing test's `print()` output is shown too (use it for measurements and benchmarks).
  `--jobs=N` splits the test files over N Godot processes (whole files, so a file keeps its order;
  balanced by the last run's durations in `user://test_durations.json`). Tests in different files must
  not depend on each other's side effects (process-wide caches such as inference tables are per process).
- Networked behaviour: `NetFixture.new(self, n_clients)`, `start()`, `await wait_connected()`, then
  `teardown()` in `after_each`; never real sockets except the single ENet test. The per-tick signal
  is `ticked` (`Net.tick()` is the method). Entities: `net.add_managers()` puts a `NetManager` at the
  same path in every branch; spawn under the server's, assert on the clients' (`test_net_entities.gd`).
  State: change properties on the server, `wait_until` the client's copy matches; bandwidth and polling
  cost come from `NetSession.stats()` (`test_net_state.gd`). Gate G7's plain scene is
  `tests/fixtures/zero_change/`: keep its scripts free of `Net.` (a test checks).
  Rooms and travel (WP13): `tests/integration/test_doors_demo.gd` runs `demos/doors/` in every branch
  (a `World` per branch, `NetSession.levels()`, a door's `body_entered` emitted on the server).
  Physics bodies (WP14): `tests/integration/test_ballpit_demo.gd` runs `demos/ballpit/` (a pit per branch;
  the clients' pits shift their collision layers with `layer_shift`, since the branches share one physics
  space; set `timeout_sec` for scenes that need seconds to settle).

## Design for testability

- Put rules and state in plain `RefCounted` / `Resource` classes under `src/`
  (see `src/health.gd`). They need no scene tree and test in milliseconds.
- Keep node scripts thin: read input, call the logic class, update visuals.
- Communicate upward with signals; only call down into children directly.
- Headless mode has no rendering, audio output, or window: do not assert on
  pixels or sounds. Physics, timers, signals, input events
  (`Input.parse_input_event`), resources, and scene instantiation all work.

## Layout

```
project.godot          engine settings; main scene is scenes/main.tscn
.godot-version         pinned Godot release used by CI and the session hook
scenes/                .tscn files (one folder per feature as the game grows)
src/                   GDScript: logic classes and node scripts
tests/framework/       test runner + TestCase base (no addons required); net_fixture.gd (NetFixture:
                       /root/Server + /root/ClientA.. branches joined by NetPipePeer, both host modes)
tests/fixtures/        scripts and scenes that tests load (never named test_*)
tests/unit/            fast tests of plain classes
tests/integration/     tests that instantiate scenes
rust/                  Cargo workspace: netcore (sans-IO protocol engine, no godot dep),
                       gdnet (the GDExtension); rust/target/ is ignored
addons/gdnet/          gdnet.gdextension + bin/<platform>/ (built, ignored) + addon assets (WP8: meter.tscn/.gd)
docs/guide/            the gdnet user guide, one page per topic (01_sessions.md, ...)
demos/                 instructional demos, each a worked example with a README whose code listings a
                       test diffs against the files (WP13: doors/ = players travelling between levels;
                       WP14: ballpit/ = hundreds of RigidBody2D balls, priority and the byte budget)
check.toml             the check's stages, tiers, budget and cache settings
tools/check.py         the one command to verify the project (the tdd-project kit's, language-agnostic;
                       reads check.toml; never edit it here without porting the fix to the kit)
tools/godot.py         the Godot and cargo side of the stages: version, build, import, validate,
                       test (--dir/--filter/--jobs/--junit), smoke, wasm
tools/validate_scripts.gd   compiles every script; used by tools/godot.py validate
tools/install_godot.sh downloads the pinned Godot into ~/.cache/godot
.check/                the check's per-checkout state (ignored)
.claude/skills/tdd-project/   the check kit (tools/check.py's source of truth, check.toml reference)
.github/workflows/ci.yml    runs the full check on push and pull request (with the Emscripten target)
```

## Conventions

- GDScript: tabs, static types on every declaration, `class_name` for anything
  referenced from other scripts, `## doc comments` on exported properties.
- Commit `*.uid` and `*.import` sidecar files; never commit `.godot/`.
- After adding a `class_name` or a new resource, the `import` stage re-imports
  (`--tier quick` or the full check; `--filter` and `--only unit` do not). If you run
  Godot by hand, run `godot --headless --import` first.
- A new stage (a linter, a build, an end-to-end run) is added to `check.toml`, not run
  by hand: what is not in the check is not verified. Never weaken a test, skip it or
  raise a tolerance to get a pass.
- Prefer editing `.tscn` files through small, reviewable diffs; they are text.
- Rust (`rust/`): `cargo fmt` and `cargo clippy -D warnings` clean; `netcore` never
  depends on `godot`, `std::net` or `std::thread` (a unit test enforces it) and must
  keep building for `wasm32-unknown-emscripten` (the check's `wasm` stage, gate G3); `gdnet` never
  enables gdext's `experimental-threads`, and every `Gd<T>` is touched on the main
  thread. Keep the `godot` crate pinned to its minor version. Register every new
  Godot-visible class in a module listed from `rust/gdnet/src/lib.rs`.

## This repository also hosts the skill

`.claude/skills/godot-project/` is the reusable skill that generated this
project. `assets/template/` in it is the source of truth for
`tests/framework/`, `tools/godot.py`, `check.toml`'s Godot stages, CI, and this file's
general sections; `.claude/skills/tdd-project/` is the source of truth for
`tools/check.py`. When you improve the runner, `tools/godot.py`, `tools/check.py`, or
the docs here, port the change to the matching template too (or edit the template
and re-run the scaffolder with `--force`), so new projects get the fix. Install the skill for all projects with
`cp -r .claude/skills/godot-project ~/.claude/skills/`.
