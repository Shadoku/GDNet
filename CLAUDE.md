# Godot Project (Godot 4.6)

Godot 4 project set up for AI-assisted development: every change is verified by
`python3 tools/check.py`, which runs headless without the editor. Trust its
output over your own reasoning about whether code works.

The project is the sample game and test bed for **gdnet**, a Rust GDExtension for
multiplayer that stays close to single-player development. Its design and work
packages are in `docs/PLAN_NETCODE.md` (approved 2026-11-08; the status table at
its end says what is built); the research behind it is
`docs/reports/Rust GDExtension multiplayer design.md` and `docs/research_notes/`.
Follow the plan's packages in order and update its status table when one lands.

## Commands

| What | Command |
|---|---|
| Everything (build gdnet, import, validate, rust tests, tests, smoke) | `python3 tools/check.py` |
| Skip the Rust build (reuse `addons/gdnet/bin`) | `python3 tools/check.py --no-build` |
| Only the Rust unit tests | `python3 tools/check.py --only rust-test` |
| Only the unit tests | `python3 tools/check.py --only test` |
| One test file or method | `python3 tools/check.py --only test --filter=health` |
| Fastest loop (no new files since last import) | `python3 tools/check.py --only test --no-import` |
| Full engine output | `python3 tools/check.py --verbose` |
| Run the game (needs a display) | `godot --path .` |
| Open the editor | `godot --path . --editor` |

`check.py` finds Godot via `$GODOT`, then `godot` on PATH, then
`~/.cache/godot/<version>/godot` (installed by `tools/install_godot.sh`, which
the Claude Code session hook and CI both use). The pinned version is in
`.godot-version`. It finds cargo via `$CARGO`, PATH, then `~/.cargo/bin/cargo`;
without cargo the Rust stages are skipped with a notice. The `build` stage runs
`cargo build --release -p gdnet` in `rust/` and copies the library to
`addons/gdnet/bin/<platform>/`, where `addons/gdnet/gdnet.gdextension` expects it
(never committed). A first build compiles gdext and takes several minutes;
later ones are incremental.

## The development loop

1. Before changing behaviour, write or adjust a test in `tests/` that describes
   the intended behaviour. Run it and watch it fail for the right reason.
2. Implement the change.
3. Run `python3 tools/check.py`. Read the output from the top: the first failing
   stage usually explains the rest (a parse error makes tests fail too).
4. Fix and re-run until the last line is `CHECK: PASS`.
5. Only then report done, quoting the `TESTS:` summary line.

Never say a change works without a passing check. If you cannot run Godot
(no binary, no network), say so explicitly instead of guessing.

## Reading failures

| Output | Meaning | Fix |
|---|---|---|
| `[FAIL] validate` + `SCRIPT ERROR: Parse Error ... (res://x.gd:12)` | script does not compile | fix the line; static typing errors count |
| `Could not find type "Foo" in the current scope` | `class_name` not in the cache yet, or typo | `check.py` runs `--import` first; if it persists the name is wrong |
| `[FAIL] tests/unit/test_x.gd::test_y` + `assert_eq failed: expected A, got B [file:line]` | assertion failure | look at the cited test line |
| `engine errors were printed during this test` | `push_error`, null access, missing node... during the test | the block below it has the message and backtrace |
| `timed out after 10.0s` | the test awaits something that never fires | check the signal or use `await_signal(sig, timeout)` |
| `[FAIL] smoke` | main scene printed an error in its first frames | the block shows file:line; often a bad node path or missing resource |
| `test did not finish (inner process died)` | Godot crashed | run with `--verbose` and look for the last line before the crash |
| `[FAIL] build` + `error[E...]` | the Rust side does not compile | fix the cited `rust/...` line; `cargo build -p gdnet` in `rust/` for the full output |
| `[FAIL] rust-test` + `panicked at rust/netcore/...` | a netcore unit test failed | `cargo test -p netcore <name>` in `rust/` |
| `GDExtension dynamic library not found: 'res://addons/gdnet/gdnet.gdextension'` | the library was not built or copied | run without `--no-build`; check `addons/gdnet/bin/<platform>/` |
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
- Run without `check.py`: `godot --headless --path . -s tests/framework/runner.gd -- --filter=name --verbose`.
  With `--verbose` a passing test's `print()` output is shown too (use it for measurements and benchmarks).
- Networked behaviour: `NetFixture.new(self, n_clients)`, `start()`, `await wait_connected()`, then
  `teardown()` in `after_each`; never real sockets except the single ENet test. The per-tick signal
  is `ticked` (`Net.tick()` is the method). Entities: `net.add_managers()` puts a `NetManager` at the
  same path in every branch; spawn under the server's, assert on the clients' (`test_net_entities.gd`).
  State: change properties on the server, `wait_until` the client's copy matches; bandwidth and polling
  cost come from `NetSession.stats()` (`test_net_state.gd`). Gate G7's plain scene is
  `tests/fixtures/zero_change/`: keep its scripts free of `Net.` (a test checks).
  Rooms and travel (WP13): `tests/integration/test_doors_demo.gd` runs `demos/doors/` in every branch
  (a `World` per branch, `NetSession.levels()`, a door's `body_entered` emitted on the server).

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
                       test diffs against the files (WP13: doors/ = players travelling between levels)
tools/check.py         the one command to verify the project
tools/validate_scripts.gd   compiles every script; used by check.py
tools/install_godot.sh downloads the pinned Godot into ~/.cache/godot
.github/workflows/ci.yml    runs check.py on push and pull request
```

## Conventions

- GDScript: tabs, static types on every declaration, `class_name` for anything
  referenced from other scripts, `## doc comments` on exported properties.
- Commit `*.uid` and `*.import` sidecar files; never commit `.godot/`.
- After adding a `class_name` or a new resource, `check.py` re-imports
  automatically. If you run Godot by hand, run `godot --headless --import` first.
- Prefer editing `.tscn` files through small, reviewable diffs; they are text.
- Rust (`rust/`): `cargo fmt` and `cargo clippy -D warnings` clean; `netcore` never
  depends on `godot`, `std::net` or `std::thread` (a unit test enforces it) and must
  keep building for `wasm32-unknown-emscripten` (CI's `wasm-check` job); `gdnet` never
  enables gdext's `experimental-threads`, and every `Gd<T>` is touched on the main
  thread. Keep the `godot` crate pinned to its minor version. Register every new
  Godot-visible class in a module listed from `rust/gdnet/src/lib.rs`.

## This repository also hosts the skill

`.claude/skills/godot-project/` is the reusable skill that generated this
project. `assets/template/` in it is the source of truth for
`tests/framework/`, `tools/`, CI, and this file's general sections. When you
improve the runner, `check.py`, or the docs here, port the change to the
template too (or edit the template and re-run the scaffolder with `--force`),
so new projects get the fix. Install the skill for all projects with
`cp -r .claude/skills/godot-project ~/.claude/skills/`.
