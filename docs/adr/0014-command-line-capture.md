# ADR 0014: Captures started from the command line

## Status

Proposed, 2026-10-21. Governs Milestone 10, part of 0.5.0. The user-facing choices (that the command
asks the running daemon and captures on its own when there is none, and that every such capture is
announced) were made by the maintainer on 2026-10-21.

## Context

The maintainer wants an agent, a coding agent running commands in a terminal for example, to be able
to take a capture and use it. Today it cannot:

- `captastic.exe` has been a windowless program since 0.3.0 (PR #101). It writes no output a program
  can read, and the only command it takes from another process is the named stop event of
  [ADR 0007](0007-daemon-control-trust.md), which carries no request and no answer.
- `captastic-dev` has `capture` and `record` commands that print JSON, but it is a developer tool and
  is not shipped. Its commands run a one-shot capture engine of their own, defaulting to the fake
  backend, rather than asking the running daemon. So a capture taken through it skips the user's
  running instance: its history, its notifications and its current state.

An agent needs four things: a command it can find on `PATH`, a way to say what to capture without a
hand on the mouse, an answer it can parse saying where the capture went, and exit codes that tell
success, refusal and failure apart. The user needs one thing: to know when their screen has been
captured by something other than them.

## Decision

**A console executable ships beside `captastic.exe`: `captastic-cli.exe`.** It is built for the
console subsystem, so it writes to the terminal that ran it and returns an exit code. `captastic.exe`
stays windowless, rather than attaching to a parent console, which works badly when a shell does not
wait for a windowless program. The ZIP and the Chocolatey package carry both. Chocolatey shims
`captastic-cli` as a console command beside the GUI shim for `captastic`. `captastic-dev` stays the
unshipped developer tool. The shipped executable carries only the commands below, sharing their code
with `captastic-dev` rather than duplicating it.

**Commands, for 0.5.0:**

| command | what it does |
| --- | --- |
| `captastic-cli capture` | take a still capture (below) and print where it went |
| `captastic-cli displays` | list the displays, with the ids `--display` takes |
| `captastic-cli windows` | list the capturable top-level windows: handle, title, process, rectangle |
| `captastic-cli status` | say whether Captastic is running, its version and PID |

Recordings from the command line are left for later. A recording runs for seconds to minutes, and it
needs its own answer to how it is stopped and who sees the indicator.

**What `capture` can capture.** Exactly one target per command:

- `--display <primary|pointer|virtual|id>`: a whole display, or the virtual desktop;
- `--rect <x,y,width,height>`: a rectangle in desktop pixels;
- `--window <handle>`, `--window-title <text>` or `--process <name>`: a window, by handle as
  `windows` prints it, or as the one frontmost window whose title contains the text or whose process
  has the name. More than one match is a refusal that lists them, never a guess;
- `--last-region`: the region Captastic last captured;
- `--interactive`: open the overlay, and wait for the user to choose and confirm, or cancel.

Window targets use the same capture path as a window chosen on the overlay, WGC included.

**Through the running daemon, and on its own when there is none.** When Captastic is running,
`captastic-cli` sends one request over a named pipe and waits for one answer. The daemon captures
through its own engine, with its configuration, its history and its notifications, exactly as
for a capture the user started, except where this ADR says otherwise. The pipe's name carries the
session id (`\\.\pipe\captastic-<session>-v1`), and its DACL admits only the user running the
daemon. Pipe names are machine-wide, unlike ADR 0007's `Local\` event, so the DACL is what keeps the
boundary the session. Requests and answers are single lines of JSON, versioned, and bounded in size.
When no daemon is running, `captastic-cli` captures with a one-shot engine on the real backend,
reading the same configuration and writing to the same output directory and history. This is the
maintainer's choice of 2026-10-21. `--interactive` needs the daemon's overlay host and is refused
when Captastic is not running.

**The result is a file, and only a file unless asked.** A capture from the command line is always
written to a file, even when **Save Captures to Disk** is off, because the file is the answer. By
default it goes to the output directory under the filename template; `--output <path>` names it
instead. The format follows the configuration unless `--format` overrides it. The clipboard is left
alone unless `--clipboard` is given: a capture the user did not start must not replace what they
copied. The capture is recorded in history with `origin = "command-line"`.

**The answer.** `capture` prints one JSON object on standard output: the path, the format, the pixel
size, what was captured (the display, rectangle or window, with its title and process), whether the
daemon or the one-shot engine took it, and its capture id. Errors also go to standard output as one
JSON object, with a stable `reason` code, so an agent never has to parse prose. Exit codes:

| code | meaning |
| --- | --- |
| 0 | captured; the JSON says where |
| 1 | failed: the capture was attempted and could not be taken |
| 2 | refused: a recording is running, the user cancelled `--interactive`, the target matched no window or several, or the window cannot be captured |
| 64 | the command line itself was wrong |

**Every command-line capture is announced, and that cannot be turned off.** The user sees a
notification for each one: "Captured by a command-line request", the target, and the file, with a
click showing the file in its folder. This holds even when ordinary capture notifications are
quiet. When no daemon is running, the one-shot engine shows the same notification through a
notification-area icon of its own that it removes a few seconds later. There is no configuration
setting to silence it. This is the maintainer's choice of 2026-10-21. A capture that reveals the
screen to another program should never be invisible, and a setting would be the first thing an
unwelcome program changes.

**The trust boundary stays the session.** As ADR 0007 argues, a process running as the user can
already read their screen in other ways. So the pipe authenticates nothing beyond the user and the
session, and the announcement is the protection: the user can always see that it happened.

**Documentation is written for agents too.** A page, `docs/command-line.md`, gives each command, its
JSON and its exit codes, in a form that can be pasted into an agent's instructions. The README links
it. A Model Context Protocol server is out of scope for 0.5.0. The command and its JSON are what one
would wrap, so it can follow without changing them.

## Consequences

- A second shipped executable. The package scripts, `artifacts.json`, `VERIFICATION.txt`, the
  Chocolatey install and uninstall hooks, the portable `install.ps1` and the VM checklist all gain
  it.
- The daemon gains a second way in from outside, its first that carries a request. Its listener runs
  on its own thread and hands captures to the existing capture path, so a slow or hostile client
  cannot stall the overlay or the hotkey. One request is served at a time, and others queue up to a
  small limit and are then refused.
- History entries gain an `origin` field. Entries without one are the user's.
- The one-shot engine moves out of the `captastic-dev`-only code into the shared library, with the
  fake backend still the default for `captastic-dev` and never available to `captastic-cli`.
- A capture refused while a recording runs, as stills already are (ADR 0011), is exit code 2 with a
  `recording_running` reason.

## Slices

As listed under Milestone 10 in `ROADMAP.md`:

1. This ADR.
2. The request channel: the pipe, its DACL, the request and answer types, and the daemon's listener,
   tested through FakeBackend.
3. `captastic-cli.exe` with its four commands, the one-shot path, JSON and exit codes, and
   packaging.
4. The announcement and the history origin, `docs/command-line.md` and the README.
