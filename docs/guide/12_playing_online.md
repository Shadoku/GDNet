# 12. Playing online: ENet, Steam join codes, offline

`Net.host(port)` and `Net.join(address, port)` work on one network. Over the internet they need the host's
public address and a forwarded port, which most testers cannot set up. This page covers gdnet's connect
layer (`docs/PLAN_NETCODE.md` 12.10, WP23): one object, and a title screen built on it, that starts a
session over ENet, over Steam with a six-character join code, or with no network at all. After the session
starts, the game's code is the same for every transport.

**Status of the Steam path.** The check covers the connect layer against a fake Steam, all the way to a
connected gdnet session. Nothing in the check can run a Steam client, and the GodotSteam calls were written
from its documentation without access to the release (2026-11-13). Until someone runs the two-machine
procedure below and it passes, treat the real Steam path as untested.

## The connect layer

`NetConnect` (`addons/gdnet/connect/net_connect.gd`) is a `Node`. Put it in the tree (it polls Steam once a
frame), call one of its methods, and wait for a signal:

```gdscript
var connect := NetConnect.new()
add_child(connect)
connect.hosted.connect(func(code: String) -> void: print("join code: ", code))   # "" for ENet and offline
connect.joined.connect(func() -> void: print("joined"))
connect.failed.connect(func(reason: String) -> void: print(reason))
connect.status.connect(func(text: String) -> void: $Status.text = text)

connect.host_enet(7350)               # ENet, like Net.host(7350)
connect.join_enet("10.0.0.2", 7350)   # ENet, like Net.join(...)
connect.host_steam()                  # a Steam lobby with a join code; hosted(code) when it exists
connect.join_steam("K7P2QX")          # find the lobby with that code, connect to its host
connect.offline()                     # like Net.offline()
connect.leave()                       # leaves the lobby and the session
```

| | |
|---|---|
| `available()` | the transports usable now: `"enet"`, `"steam"` (only with GodotSteam installed and a Steam client running), `"offline"` |
| `steam_reason()` | why Steam is not available, in words ("" when it is) |
| `code()` | the join code while hosting on Steam |
| `hosted(code)` | a session started as the host |
| `joined()` | a session started as a client (gdnet's `connected` follows after the handshake) |
| `failed(reason)` | starting failed, or a Steam game ended because its host left |
| `status(text)` | progress, for a status line |
| `timeout_s` | seconds to wait for each answer from Steam (default 10) |
| `max_players` | the lobby's size, host included (default 16) |

Every method returns an `Error` as well; the Steam ones return `OK` when the request went out and report
the outcome through the signals. `NetConnect.is_join_code(text)` and `NetConnect.parse_address(text,
port)` are the rules the title screen uses for its one field.

By default the session runs on the `Net` singleton (`Net.host`, `Net.join`, `Net.offline`, and
`Net.use_peer` for the Steam peer). Set `session` (and `session_root`) to start a particular `NetSession`
instead; the tests do this under `NetFixture`.

### The title screen

`addons/gdnet/connect/connect_menu.tscn` (`NetConnectMenu`) is the screen both demos use: **Host** (ENet on
the menu's `port`), **Host on Steam** (shown only when Steam is available; then the join code and a **Copy**
button), one field with **Join**, **Offline**, and a status line. The field decides by its shape: six
characters of the code alphabet join on Steam, anything else is an address (`host` or `host:port`). A host
name of six such letters needs a dot (`server.lan`). The menu emits `started` once a session runs, then
hides its buttons and keeps the join code in the bottom-left corner so the host can read it out.

```gdscript
@onready var menu: NetConnectMenu = $Menu    # an instance of connect_menu.tscn, title and port set

func _ready() -> void:
	menu.started.connect(_start)              # _start begins the game, whatever the transport
	if "--server" in OS.get_cmdline_user_args():
		menu.host()                           # ENet on menu.port, no click
```

## The three transports

* **ENet** (`host_enet`, `join_enet`): Godot's `ENetMultiplayerPeer` over UDP. Works on a LAN, or over the
  internet with the host's public address and a forwarded UDP port. No account, no extra files.
* **Steam** (`host_steam`, `join_steam`): Steam lobbies and Steam's relay (Steam Datagram Relay) through
  the GodotSteam GDExtension. No port forwarding, no server of your own; both players need Steam.
* **Offline** (`offline`): no network; this process hosts and plays as peer 1, as with `Net.offline()`.

### How a join code works

Hosting creates a **public** Steam lobby and writes four values into its lobby data: the code, the game's
name, the host's Steam id and the time it was created. The lobby is public because Steam's lobby search
returns only public lobbies; a lobby nobody can find by search would need an invite. Joining searches this
app's lobbies for the code and the game's name, joins the lobby, checks that its owner is still the host
that made it, and connects a `SteamMultiplayerPeer` to the owner.

* Codes are six characters from `ABCDEFGHJKMNPQRSTUVWXYZ23456789`: no 0/O, no 1/I/L. Case, spaces and
  dashes do not matter when typing one.
* Two lobbies with one code (very unlikely: 31^6 codes, filtered by game name): the newest wins.
* A code nobody hosts: `failed("no game with code ...")`. A full lobby: `failed("the game is full")`.
* The host leaves: Steam passes the lobby to another member, so a client that sees the host leave fails
  with "the host left the game" and leaves the lobby too; a joiner who finds a lobby whose owner is no
  longer the host that made it is refused ("the host has left this game").
* Steam not answering within `timeout_s`: `failed("Steam did not answer ...")`.
* A Steam friend can also join from the friends list ("Join game") or an invite: the lobby id comes with
  Steam's `join_requested`, and the connect layer joins that lobby without a code (while the menu is idle).

The game's name is the project setting `gdnet/connect/game_name` (default: the project's
`application/config/name`; this repository sets it to `gdnet demos`). Set it to something unique to your
game: app 480 is shared, and the name is what keeps your testers out of other people's lobbies.

## Installing GodotSteam

GodotSteam is not part of gdnet and is never committed. `tools/install_godotsteam.sh` downloads the version
pinned in `.godotsteam-version` into `addons/godotsteam/` (ignored by git), and verifies the archive
against `.godotsteam-sha256` when that file holds a checksum:

```sh
tools/install_godotsteam.sh          # once per checkout; again after changing .godotsteam-version
```

The archive holds `addons/godotsteam/`: a `.gdextension` file and a library per platform, with Valve's
`steam_api` redistributable beside each. Open the project once in the editor (or run
`godot --headless --import`) so Godot registers the extension. Without it, or without a running Steam
client, the menu shows **Host**, **Join** and **Offline** only, and nothing else changes; the check and CI
never install it.

If the download fails, the release URL may have changed: open the GodotSteam releases page on Codeberg
(`codeberg.org/godotsteam/godotsteam/releases`), copy the link of the GDExtension plugin archive for the
pinned version and run `GODOTSTEAM_URL=<link> tools/install_godotsteam.sh`.

## App id 480

Every Steam game has an app id. The demos use **480**, Spacewar, the test app Valve gives every developer.
It costs nothing and needs no Steamworks account. What it means:

* Both testers need the Steam client running and logged in, each with their own account. They do not need
  to own anything or be friends to join by code.
* Steam shows them as playing "Spacewar".
* App 480 is shared by everyone testing on Steam, so lobby searches always filter by the game's name.
* The app id is the project setting `gdnet/connect/steam_app_id` (default 480).

Steam reads the app id when it starts. If Steam refuses to start outside the editor, put a file named
`steam_appid.txt` containing `480` beside the executable (or beside `project.godot` when running from the
editor). It is ignored by git.

## Sending a build to a tester

1. Install Godot's export templates (*Editor > Manage Export Templates*) and add an export preset for the
   tester's platform (*Project > Export > Add... > Windows Desktop / Linux / macOS*).
2. gdnet's library must exist for that platform in `addons/gdnet/bin/<platform>/`. The check builds it for
   the machine it runs on only; for another platform, build `rust/` there (`python3 tools/godot.py build`)
   or cross-compile, and copy the library into the matching folder.
3. Run `tools/install_godotsteam.sh` so `addons/godotsteam/` holds the libraries for every platform.
4. Export. Godot copies each GDExtension's library for the target platform beside the executable. Check
   that the export folder holds:
   * the executable and its `.pck` (unless embedded),
   * gdnet's library (`gdnet.dll` / `libgdnet.so` / `libgdnet.dylib`),
   * GodotSteam's library (its name contains `godotsteam`),
   * Valve's `steam_api64.dll` (Windows), `libsteam_api.so` (Linux) or `libsteam_api.dylib` (macOS),
   * `steam_appid.txt` containing `480` (copy it in by hand).
5. Zip the folder and send it. The tester unzips it, starts Steam, then the game.

## Switching to a real app id

When the game has its own app id from Steamworks: set `gdnet/connect/steam_app_id` to it, set
`gdnet/connect/game_name` to the game's name, and put the new id in `steam_appid.txt` for builds started
outside Steam (a build launched by Steam does not need the file). Nothing else changes.

## Manual test on two machines

The check cannot run Steam. Run this once after installing GodotSteam, and again after updating it. Two
computers (A and B), each with Steam installed and logged in to a different account, on different networks
if possible (a phone hotspot for one is enough). Record the result in the WP23 row of the plan's status
table.

1. On A, in the repository: `tools/install_godotsteam.sh`. Expected: "GodotSteam ... installed in
   addons/godotsteam/".
2. On A: `python3 tools/check.py --tier quick`. Expected: `CHECK: PASS` (the project imports with the addon
   present). Run `git status`: expected, no changed files (`addons/godotsteam/` is ignored).
3. On A, open `demos/doors/main.tscn` in the editor and press F6. Expected: the menu shows **Host on
   Steam**, and the status line is empty. If it shows "Steam: ...", that text says what is missing.
4. Press **Host on Steam**. Expected: within a few seconds the menu disappears, the Doors overworld shows
   your player, and "Join code: XXXXXX" with a **Copy** button stays in the bottom-left corner. Steam's
   overlay shows you playing Spacewar.
5. Export a build for B as in "Sending a build to a tester" and copy it to B (or run the project from a
   checkout on B after step 1 there).
6. On B, start Steam, then the build. Expected: the menu shows **Host on Steam**.
7. On B, type A's code into the field (lower case is fine) and press **Join**. Expected: the status line
   shows "looking for ...", then "joining ...", then the overworld with both players; each sees the other
   move.
8. On B, type in the chat box. Expected: A sees the message.
9. On B, type a wrong code (e.g. `ZZZZZZ`) after restarting the build. Expected: "no game with code
   ZZZZZZ ...", the menu stays.
10. On A, close the game while B is connected. Expected: on B, gdnet's session ends (the players stop) and
    the status line in the corner says "the host left the game".
11. Repeat steps 3-7 with `demos/ballpit/main.tscn`. Expected: B's paddle pushes the balls on both screens.
12. Optional, if A and B are Steam friends: A hosts on Steam; B, in the Steam friends list, chooses "Join
    game" on A while B's game shows the menu. Expected: B joins without typing the code.

If a step fails with a GodotSteam error such as "Invalid call. Nonexistent function", the method name in
`addons/gdnet/connect/steam_backend.gd` is wrong for this GodotSteam version: every GodotSteam call is in
that one file, and its doc comment lists the names it assumes.

## Known limits

* The Steam path is untested until the procedure above passes (see the top of this page).
* The GodotSteam calls in `steam_backend.gd` and the release URL and checksum in
  `tools/install_godotsteam.sh` were checked against GodotSteam 4.24 loaded in Godot 4.6 (2026-11-13),
  without a Steam client: the calls exist with the arguments used; what Steam answers is still untested.
* Desktop only: GodotSteam does not run in a browser.
* A dedicated server on Steam (`--server`) is not offered: `--server` hosts over ENet. A Steam-hosted game
  needs a logged-in Steam client on the host.
* Joining by code needs a public lobby; anyone who knows the game's name and guesses a code could join.
  gdnet does not check who joins (12.1); a game that cares checks on the server.
* After `joined()`, if the Steam connection to the host never comes up, gdnet's session simply stays
  unconnected; watch `Net.connected` and `Net.disconnected` as with ENet.
* Steam ids are not yet passed to gdnet as rejoin keys; that comes with WP17's seats (plan 12.10).

## Later: WebRTC

For browsers and players without Steam, the plan's next transport is WebRTC (WP24, `docs/PLAN_NETCODE.md`
12.10): `WebRTCMultiplayerPeer`, a small signalling server that is also the lobby (the same join codes) and
a relay for pairs that cannot connect directly. It needs a hosted server, so it waits; it will be one more
method on `NetConnect`, and game code will not change.
