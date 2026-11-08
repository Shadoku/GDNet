#!/usr/bin/env python3
"""Run every automated check for this Godot project and print a compact report.

Stages (in order):
  build     - cargo build the gdnet GDExtension (rust/) into addons/gdnet/bin/<platform>/
  import    - (re)import assets and rebuild the class_name cache  (godot --import)
  validate  - compile every .gd script and report parse/type errors
  rust-test - cargo test --workspace in rust/ (the netcore protocol engine's unit tests)
  test      - run the unit tests under tests/ with tests/framework/runner.gd
  smoke     - run the main scene headless for a few frames and fail on any error

Usage:
  python3 tools/check.py                # everything
  python3 tools/check.py --only test    # one stage (import always runs first)
  python3 tools/check.py --only test --no-import   # fastest loop when no files were added
  python3 tools/check.py --no-build      # skip the Rust build (the library in addons/gdnet/bin is used as is)
  python3 tools/check.py --filter=player  # pass a test filter to the runner
  python3 tools/check.py --verbose      # show full engine output for every stage
  python3 tools/check.py --jobs 1       # run the test files in one Godot process (default 2)

Exit code is 0 only when every stage passed. The last line is always
"CHECK: PASS" or "CHECK: FAIL (<stages>)" so the result is easy to spot.

Environment:
  CARGO   - path to cargo (default: `cargo` on PATH, then ~/.cargo/bin/cargo). The build and rust-test
            stages are skipped with a notice when rust/Cargo.toml is absent.
  GODOT   - path to the Godot 4 binary. If unset, PATH (godot, godot4) and
            ~/.cache/godot/<version>/godot are tried, where <version> comes from
            the .godot-version file next to project.godot.
"""
from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SMOKE_FRAMES = 30
STAGE_TIMEOUT = {"build": 1800, "import": 300, "validate": 120, "rust-test": 600, "test": 600, "smoke": 120}
RUST_DIR = ROOT / "rust"
GDNET_BIN = ROOT / "addons" / "gdnet" / "bin"

# Error lines matching any of these regexes are ignored (known-benign headless noise).
IGNORE_PATTERNS = [
    r"XDG_RUNTIME_DIR",
    r"ALSA lib",
]

ERROR_PREFIX = re.compile(r"^(SCRIPT ERROR|ERROR|USER ERROR):")
BANNER = re.compile(r"^Godot Engine v\d")


def godot_version() -> str:
    f = ROOT / ".godot-version"
    return f.read_text().strip() if f.exists() else "4.6-stable"


def find_godot() -> str:
    candidates = []
    if os.environ.get("GODOT"):
        candidates.append(os.environ["GODOT"])
    for name in ("godot", "godot4", "godot4-stable"):
        p = shutil.which(name)
        if p:
            candidates.append(p)
    candidates.append(str(Path.home() / ".cache" / "godot" / godot_version() / "godot"))
    candidates.append(str(Path.home() / ".cache" / "godot" / "godot"))
    for c in candidates:
        if os.path.isfile(c) and os.access(c, os.X_OK):
            return c
    sys.exit(
        "No Godot binary found. Set GODOT=/path/to/godot, put `godot` on PATH, or run "
        ".claude/hooks/session-start.sh to download " + godot_version() + "."
    )


def find_cargo() -> str | None:
    candidates = []
    if os.environ.get("CARGO"):
        candidates.append(os.environ["CARGO"])
    p = shutil.which("cargo")
    if p:
        candidates.append(p)
    candidates.append(str(Path.home() / ".cargo" / "bin" / "cargo"))
    for c in candidates:
        if os.path.isfile(c) and os.access(c, os.X_OK):
            return c
    return None


def gdnet_platform() -> tuple[str, str]:
    """(folder under addons/gdnet/bin, library file name) for this machine, matching gdnet.gdextension."""
    import platform

    machine = platform.machine().lower()
    arch = "arm64" if machine in ("arm64", "aarch64") else "x86_64"
    if sys.platform.startswith("linux"):
        return f"linux-{arch}", "libgdnet.so"
    if sys.platform == "darwin":
        return "macos", "libgdnet.dylib"
    if sys.platform in ("win32", "cygwin"):
        return f"windows-{arch}", "gdnet.dll"
    sys.exit(f"check.py: unsupported platform {sys.platform} for the gdnet build")


def run_cargo(cargo: str, args: list[str], timeout: int) -> tuple[int, str]:
    cmd = [cargo, *args]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, cwd=RUST_DIR)
        return proc.returncode, proc.stdout + proc.stderr
    except subprocess.TimeoutExpired as e:
        out = (e.stdout or "") + (e.stderr or "")
        if isinstance(out, bytes):
            out = out.decode(errors="replace")
        return 124, out + f"\n[check.py] TIMEOUT after {timeout}s: {' '.join(cmd)}\n"


def build_gdnet(cargo: str, timeout: int) -> tuple[int, str]:
    """cargo build --release -p gdnet, then copy the library where gdnet.gdextension expects it."""
    code, out = run_cargo(cargo, ["build", "--release", "-p", "gdnet"], timeout)
    if code != 0:
        return code, out
    folder, lib = gdnet_platform()
    src = RUST_DIR / "target" / "release" / lib
    if not src.exists():
        return 1, out + f"\n[check.py] built, but {src} does not exist\n"
    dest = GDNET_BIN / folder / lib
    dest.parent.mkdir(parents=True, exist_ok=True)
    if not dest.exists() or dest.stat().st_mtime < src.stat().st_mtime:
        shutil.copy2(src, dest)
        out += f"\n[check.py] installed {dest.relative_to(ROOT)}\n"
    return 0, out


def run(godot: str, args: list[str], timeout: int) -> tuple[int, str]:
    cmd = [godot, "--headless", "--path", str(ROOT), *args]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, cwd=ROOT)
        out = proc.stdout + proc.stderr
        return proc.returncode, out
    except subprocess.TimeoutExpired as e:
        out = (e.stdout or "") + (e.stderr or "")
        if isinstance(out, bytes):
            out = out.decode(errors="replace")
        return 124, out + f"\n[check.py] TIMEOUT after {timeout}s: {' '.join(cmd)}\n"


def error_blocks(output: str) -> list[str]:
    """Extract ERROR / SCRIPT ERROR blocks (first line + indented continuation lines)."""
    blocks: list[list[str]] = []
    for line in output.splitlines():
        if ERROR_PREFIX.match(line):
            blocks.append([line])
        elif blocks and line[:1] in (" ", "\t") and blocks[-1] is not None:
            blocks[-1].append(line)
        else:
            blocks.append(None)  # break continuation
    result = []
    for b in blocks:
        if b is None:
            continue
        # Backtrace frames inside our own tooling are noise for the reader.
        b = [l for l in b if not ("validate_scripts.gd" in l and l.strip().startswith("["))]
        text = "\n".join(b)
        if any(re.search(p, text) for p in IGNORE_PATTERNS):
            continue
        result.append(text)
    return result


def clean(output: str) -> str:
    return "\n".join(l for l in output.splitlines() if not BANNER.match(l)).strip()


def default_jobs() -> int:
    """Two test processes when there are two CPUs to run them: the tests mostly wait on the clock."""
    return 2 if (os.cpu_count() or 1) >= 2 else 1


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--only", choices=["build", "import", "validate", "rust-test", "test", "smoke"], help="run a single stage (build and import always run first unless skipped)")
    ap.add_argument("--no-import", action="store_true", help="skip the import stage (safe only when no scripts, scenes or assets were added or renamed)")
    ap.add_argument("--no-build", action="store_true", help="skip the Rust build stage and use the library already in addons/gdnet/bin")
    ap.add_argument("--filter", help="only run tests whose 'file::method' id contains this substring")
    ap.add_argument("--verbose", action="store_true", help="print full engine output for every stage")
    ap.add_argument("--junit", help="write JUnit XML for the test stage to this path")
    ap.add_argument("--jobs", type=int, default=default_jobs(), help="test files run in this many Godot processes at once (default: 2 with 2+ CPUs; 1 = one process, the old way)")
    ap.add_argument("--smoke-frames", type=int, default=SMOKE_FRAMES)
    ap.add_argument("--skip-smoke", action="store_true")
    ap.add_argument("--include-addons", action="store_true", help="also validate scripts under addons/")
    args = ap.parse_args()

    godot = find_godot()
    print(f"godot: {godot}")
    failed: list[str] = []
    cargo = find_cargo() if RUST_DIR.joinpath("Cargo.toml").exists() else None
    if RUST_DIR.joinpath("Cargo.toml").exists() and cargo is None:
        print("cargo: not found (install Rust from https://rustup.rs or set CARGO); build and rust-test are skipped")
    elif cargo:
        print(f"cargo: {cargo}")

    def cargo_stage(name: str, fn) -> None:
        """A stage that runs cargo instead of Godot. fn() -> (code, output)."""
        if cargo is None:
            return
        # Like import, build runs for every --only except rust-test (cargo test builds on its own).
        if args.only and args.only != name and name != "build":
            return
        t0 = time.time()
        code, out = fn()
        ok = code == 0
        dt = time.time() - t0
        print(f"[{'PASS' if ok else 'FAIL'}] {name} ({dt:.1f}s)")
        if args.verbose:
            print("    " + out.strip().replace("\n", "\n    "))
        elif not ok:
            # cargo prints the useful lines near the end; keep the errors and the tail.
            lines = out.splitlines()
            keep = [l for l in lines if l.startswith(("error", "warning: unused", "test result", "failures:")) or l.strip().startswith(("-->", "panicked"))]
            print("    " + "\n".join(keep or lines[-40:]).replace("\n", "\n    "))
        else:
            for line in out.splitlines():
                if line.startswith("test result") or "[check.py] installed" in line:
                    print("    " + line.replace("[check.py] ", ""))
        if not ok:
            failed.append(name)

    def stage(name: str, godot_args: list[str], judge) -> None:
        if args.only and name != "import" and args.only != name:
            return
        t0 = time.time()
        code, out = run(godot, godot_args, STAGE_TIMEOUT[name])
        ok, details = judge(code, out)
        dt = time.time() - t0
        print(f"[{'PASS' if ok else 'FAIL'}] {name} ({dt:.1f}s)")
        if args.verbose:
            print("    " + clean(out).replace("\n", "\n    "))
        elif not ok:
            print("    " + details.replace("\n", "\n    "))
        else:
            # Always show the one-line summary so a report can quote it.
            for line in out.splitlines():
                if line.startswith(("TESTS:", "SCRIPTS:")):
                    print("    " + line)
        if not ok:
            failed.append(name)

    # 0. build: the gdnet GDExtension. Runs before import so Godot can load the library.
    if not args.no_build and not (args.only and args.only in ("rust-test",)):
        cargo_stage("build", lambda: build_gdnet(cargo, STAGE_TIMEOUT["build"]))

    # 1. import: rebuilds .godot/, imports assets, refreshes class_name cache.
    if not args.no_import:
        stage("import", ["--import"], lambda code, out: (code == 0, clean(out)[-3000:]))

    # 2. validate: every script compiles. Details = the SCRIPT ERROR lines.
    def judge_validate(code, out):
        blocks = error_blocks(out)
        summary = [l for l in out.splitlines() if l.startswith(("[FAIL]", "SCRIPTS:"))]
        return code == 0, "\n".join(blocks + summary) or clean(out)[-3000:]
    validate_args = ["-s", "res://tools/validate_scripts.gd"]
    if args.include_addons:
        validate_args += ["--", "--include-addons"]
    stage("validate", validate_args, judge_validate)

    # 2b. rust-test: the protocol engine's own unit tests.
    if not args.only or args.only == "rust-test":
        cargo_stage("rust-test", lambda: run_cargo(cargo, ["test", "--workspace", "--quiet"], STAGE_TIMEOUT["rust-test"]))

    # 3. test: the runner already prints only failures + summary; show that verbatim.
    test_args = ["-s", "res://tests/framework/runner.gd", "--"]
    if args.verbose:
        test_args.append("--verbose")
    if args.filter:
        test_args.append(f"--filter={args.filter}")
    if args.junit:
        test_args.append(f"--junit={os.path.abspath(args.junit)}")
    if args.jobs > 1:
        test_args.append(f"--jobs={args.jobs}")
    stage("test", test_args, lambda code, out: (code == 0, clean(out)))

    # 4. smoke: main scene boots and runs N frames without printing any error.
    if not args.skip_smoke:
        def judge_smoke(code, out):
            blocks = error_blocks(out)
            ok = code == 0 and not blocks
            details = "\n".join(blocks) if blocks else clean(out)[-3000:]
            if code != 0:
                details = f"exit code {code}\n" + details
            return ok, details
        stage("smoke", ["--quit-after", str(args.smoke_frames)], judge_smoke)

    print()
    if failed:
        print(f"CHECK: FAIL ({', '.join(failed)})")
        return 1
    print("CHECK: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
