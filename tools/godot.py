#!/usr/bin/env python3
"""The Godot (and gdnet/cargo) side of this project's check, one subcommand per stage.

check.toml calls these; tools/check.py knows nothing about Godot. Every subcommand exits 0 on
a pass and non-zero on a failure, and prints what the stage's report should show: the engine
banner is stripped, and on a failure the ERROR / SCRIPT ERROR blocks come first.

  python3 tools/godot.py version            # Godot, cargo and rustc versions (the pass cache's toolchain)
  python3 tools/godot.py build              # cargo build --release -p gdnet, copy into addons/gdnet/bin/<platform>/
  python3 tools/godot.py import             # godot --headless --import (assets, class_name cache)
  python3 tools/godot.py validate           # compile every .gd script (tools/validate_scripts.gd)
  python3 tools/godot.py test [--dir tests/unit] [--filter X] [--jobs N] [--junit PATH] [--verbose]
  python3 tools/godot.py smoke [--frames N] # run the main scene headless; fail on any error printed
  python3 tools/godot.py wasm               # cargo check for wasm32-unknown-emscripten (skipped without the target)

Godot exits 0 even when scripts print errors, so validate and smoke judge the output: any
ERROR / SCRIPT ERROR / USER ERROR block not matching IGNORE_PATTERNS fails the stage.

Environment:
  GODOT   the Godot 4 binary. If unset: PATH (godot, godot4), then ~/.cache/godot/<version>/godot,
          <version> from .godot-version.
  CARGO   cargo (default: PATH, then ~/.cargo/bin/cargo). Without rust/Cargo.toml the cargo
          subcommands pass with a notice.
"""
from __future__ import annotations

import argparse
import os
import platform
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RUST_DIR = ROOT / "rust"
GDNET_BIN = ROOT / "addons" / "gdnet" / "bin"
WASM_TARGET = "wasm32-unknown-emscripten"
SMOKE_FRAMES = 30
TIMEOUT = {"build": 1800, "import": 300, "validate": 120, "test": 900, "smoke": 120, "wasm": 900}

# Error lines matching any of these regexes are ignored (known-benign headless noise).
IGNORE_PATTERNS = [
    r"XDG_RUNTIME_DIR",
    r"ALSA lib",
]

ERROR_PREFIX = re.compile(r"^(SCRIPT ERROR|ERROR|USER ERROR):")
BANNER = re.compile(r"^Godot Engine v\d")
NO_TESTS_RAN = re.compile(r"^TESTS: 0 passed, 0 failed, 0 errors, 0 skipped \(0 total\)$", re.M)


# --- finding the tools ---------------------------------------------------------------

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
        "tools/install_godot.sh to download " + godot_version() + "."
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


def has_rust() -> bool:
    return (RUST_DIR / "Cargo.toml").exists()


def gdnet_platform() -> tuple[str, str]:
    """(folder under addons/gdnet/bin, library file name) for this machine, matching gdnet.gdextension."""
    machine = platform.machine().lower()
    arch = "arm64" if machine in ("arm64", "aarch64") else "x86_64"
    if sys.platform.startswith("linux"):
        return f"linux-{arch}", "libgdnet.so"
    if sys.platform == "darwin":
        return "macos", "libgdnet.dylib"
    if sys.platform in ("win32", "cygwin"):
        return f"windows-{arch}", "gdnet.dll"
    sys.exit(f"godot.py: unsupported platform {sys.platform} for the gdnet build")


# --- running -------------------------------------------------------------------------

def run(cmd: list[str], timeout: int, cwd: Path = ROOT) -> tuple[int, str]:
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, errors="replace", timeout=timeout, cwd=cwd)
        return proc.returncode, proc.stdout + proc.stderr
    except subprocess.TimeoutExpired as e:
        out = (e.stdout or "") + (e.stderr or "")
        if isinstance(out, bytes):
            out = out.decode(errors="replace")
        return 124, out + f"\n[godot.py] TIMEOUT after {timeout}s: {' '.join(cmd)}\n"


def run_godot(args: list[str], timeout: int) -> tuple[int, str]:
    return run([find_godot(), "--headless", "--path", str(ROOT), *args], timeout)


def error_blocks(output: str) -> list[str]:
    """ERROR / SCRIPT ERROR blocks (first line + indented continuation lines), noise removed."""
    blocks: list[list[str] | None] = []
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


def say(text: str) -> None:
    if text:
        print(text, flush=True)


# --- subcommands ---------------------------------------------------------------------

def cmd_version(_: argparse.Namespace) -> int:
    code, out = run([find_godot(), "--version"], 60)
    print(f"godot {out.strip()}")
    cargo = find_cargo()
    if cargo:
        print(run([cargo, "--version"], 60)[1].strip())
        rustc = Path(cargo).with_name("rustc")
        print(run([str(rustc) if rustc.exists() else "rustc", "--version"], 60)[1].strip())
    else:
        print("cargo: not found")
    return code


def cmd_build(_: argparse.Namespace) -> int:
    """cargo build --release -p gdnet, then copy the library where gdnet.gdextension expects it."""
    if not has_rust():
        print("no rust/Cargo.toml: nothing to build")
        return 0
    cargo = find_cargo()
    folder, lib = gdnet_platform()
    dest = GDNET_BIN / folder / lib
    if cargo is None:
        if dest.exists():
            print(f"cargo not found: using the library already in {dest.relative_to(ROOT)} "
                  "(install Rust from https://rustup.rs or set CARGO to rebuild it)")
            return 0
        print("cargo not found and no gdnet library installed: install Rust from https://rustup.rs or set CARGO")
        return 1
    code, out = run([cargo, "build", "--release", "-p", "gdnet"], TIMEOUT["build"], RUST_DIR)
    say(out.strip())
    if code != 0:
        return code
    src = RUST_DIR / "target" / "release" / lib
    if not src.exists():
        print(f"built, but {src} does not exist")
        return 1
    dest.parent.mkdir(parents=True, exist_ok=True)
    if not dest.exists() or dest.stat().st_mtime < src.stat().st_mtime:
        shutil.copy2(src, dest)
        print(f"installed {dest.relative_to(ROOT)}")
    return 0


def cmd_import(_: argparse.Namespace) -> int:
    code, out = run_godot(["--import"], TIMEOUT["import"])
    if code != 0:
        say(clean(out)[-3000:])
    return code


def cmd_validate(args: argparse.Namespace) -> int:
    godot_args = ["-s", "res://tools/validate_scripts.gd"]
    if args.include_addons:
        godot_args += ["--", "--include-addons"]
    code, out = run_godot(godot_args, TIMEOUT["validate"])
    summary = [l for l in out.splitlines() if l.startswith(("[FAIL]", "SCRIPTS:"))]
    if code == 0:
        say("\n".join(summary))
    else:
        say("\n".join(error_blocks(out) + summary) or clean(out)[-3000:])
    return code


def cmd_test(args: argparse.Namespace) -> int:
    runner_args = ["-s", "res://tests/framework/runner.gd", "--"]
    if args.dir:
        d = args.dir if args.dir.startswith("res://") else "res://" + args.dir.strip("/")
        runner_args.append(f"--dir={d}")
    if args.verbose:
        runner_args.append("--verbose")
    if args.filter:
        runner_args.append(f"--filter={args.filter}")
    if args.junit:
        runner_args.append(f"--junit={os.path.abspath(args.junit)}")
    if args.jobs > 1:
        runner_args.append(f"--jobs={args.jobs}")
    code, out = run_godot(runner_args, TIMEOUT["test"])
    text = clean(out)
    say(text)
    # A filter that matches nothing here is not a failure of this stage (another may match):
    # the runner reports 0 tests and fails; tools/check.py's filter_match sees that none ran.
    if code != 0 and args.filter and NO_TESTS_RAN.search(text) and "[ERROR]" not in text:
        return 0
    return code


def cmd_smoke(args: argparse.Namespace) -> int:
    code, out = run_godot(["--quit-after", str(args.frames)], TIMEOUT["smoke"])
    blocks = error_blocks(out)
    if code == 0 and not blocks:
        print(f"main scene ran {args.frames} frames without errors")
        return 0
    details = "\n".join(blocks) if blocks else clean(out)[-3000:]
    if code != 0:
        details = f"exit code {code}\n" + details
    say(details)
    return code or 1


def cmd_wasm(_: argparse.Namespace) -> int:
    """Gate G3 (docs/PLAN_NETCODE.md): the workspace keeps compiling for Godot's web export target."""
    if not has_rust():
        print("no rust/Cargo.toml: nothing to check")
        return 0
    cargo = find_cargo()
    if cargo is None:
        print("skipped: cargo not found (install Rust from https://rustup.rs or set CARGO)")
        return 0
    rustup = shutil.which("rustup") or str(Path(cargo).with_name("rustup"))
    code, installed = run([rustup, "target", "list", "--installed"], 60)
    if code != 0 or WASM_TARGET not in installed.split():
        print(f"skipped: the {WASM_TARGET} target is not installed here "
              f"(`rustup target add {WASM_TARGET}` to run it; CI installs it and runs this for real)")
        return 0
    code, out = run([cargo, "check", "--workspace", "--quiet", "--target", WASM_TARGET], TIMEOUT["wasm"], RUST_DIR)
    if code == 0:
        print(f"cargo check --workspace --target {WASM_TARGET}: ok")
    else:
        say(out.strip())
    return code


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="command", required=True)
    sub.add_parser("version", help="print the Godot, cargo and rustc versions").set_defaults(fn=cmd_version)
    sub.add_parser("build", help="build gdnet and install it into addons/gdnet/bin").set_defaults(fn=cmd_build)
    sub.add_parser("import", help="godot --import").set_defaults(fn=cmd_import)
    p = sub.add_parser("validate", help="compile every script")
    p.add_argument("--include-addons", action="store_true", help="also validate scripts under addons/")
    p.set_defaults(fn=cmd_validate)
    p = sub.add_parser("test", help="run tests/framework/runner.gd")
    p.add_argument("--dir", help="only the test files under this folder (tests/unit or res://tests/unit)")
    p.add_argument("--filter", help="only tests whose 'file::method' id contains this substring")
    p.add_argument("--jobs", type=int, default=1, help="split the test files over this many Godot processes")
    p.add_argument("--junit", help="write JUnit XML to this path")
    p.add_argument("--verbose", action="store_true", help="also show passing tests and their output")
    p.set_defaults(fn=cmd_test)
    p = sub.add_parser("smoke", help="run the main scene headless and fail on any error")
    p.add_argument("--frames", type=int, default=SMOKE_FRAMES)
    p.set_defaults(fn=cmd_smoke)
    sub.add_parser("wasm", help=f"cargo check for {WASM_TARGET}").set_defaults(fn=cmd_wasm)
    args = ap.parse_args()
    return args.fn(args)


if __name__ == "__main__":
    sys.exit(main())
