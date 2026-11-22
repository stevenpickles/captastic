# Captures from the command line: `captastic-cli`

`captastic-cli` lets a script, a terminal user, or an agent such as a coding assistant take a
screenshot and get back, as one line of JSON, what was captured and where the file is. It ships
beside `captastic.exe` from 0.5.0: Chocolatey puts it on `PATH`; the portable install puts it in
the install folder. The design is [ADR 0014](adr/0014-command-line-capture.md).

Everything below can be pasted into an agent's instructions.

## What to know first

- **Every answer is one line of JSON on standard output**, success or not. Read the `status`
  field, or the exit code. A person-readable explanation of a wrong command line also goes to
  standard error; nothing else does.
- **Exit codes:** `0` captured or answered; `1` failed (tried and could not); `2` refused (the
  request was declined, see `reason`); `64` the command line itself was wrong.
- **The user always sees it.** Every capture raises a *Captured by a command-line request*
  notification naming the file, and clicking it shows the file. This cannot be turned off.
- **It works whether or not Captastic is running.** With Captastic running, it asks Captastic,
  which captures with its own settings (`"taken_by":"daemon"`). Without it, it captures by itself
  with the same settings, output folder and history (`"taken_by":"one_shot"`). Only
  `--interactive` needs Captastic running.
- **The result is a file.** It goes to Captastic's output folder under its filename template,
  even when *Save Captures to Disk* is off, or to `--output`. It is never written over an
  existing file.
- **The clipboard is left alone** unless `--clipboard` is given.
- **Every capture is recorded in Captastic's history** with `origin = "command-line"`.

## Commands

### `captastic-cli status`

Whether Captastic is running in this session.

```json
{"version":1,"status":"ok","running":true,"captastic_version":"0.5.0","pid":12345}
{"version":1,"status":"ok","running":false}
```

### `captastic-cli displays`

The displays, with the ids `--display` takes. Rectangles are in desktop pixels.

```json
{"version":1,"status":"ok","displays":[{"id":"windows-monitor-DEL41B4-dp0","name":"DELL U2720Q",
 "bounds":{"x":0,"y":0,"width":3840,"height":2160},"scale_factor":1.5,"rotation_degrees":0,
 "is_primary":true}]}
```

### `captastic-cli windows`

The windows that can be captured, frontmost first: the same ones Captastic's overlay offers.
`handle` is what `--window` takes. Titles are set by each window's own application.

```json
{"version":1,"status":"ok","windows":[{"handle":2821696,"title":"a.txt - Notepad",
 "process":"Notepad.exe","rect":{"x":75,"y":116,"width":1799,"height":1160},
 "display":"windows-monitor-DEL41B4-dp0"}]}
```

### `captastic-cli capture <target> [options]`

Takes one still capture. Exactly one target:

| target | captures |
| --- | --- |
| `--display <primary\|pointer\|virtual\|id>` | a whole display; `virtual` is every display together |
| `--rect <x,y,width,height>` | a rectangle in desktop pixels; x and y may be negative |
| `--window <handle>` | the window with that handle, from `windows`; decimal or `0x` hex |
| `--window-title <text>` | the window whose title contains the text, ignoring case |
| `--process <name>` | the window of that executable, such as `notepad` or `notepad.exe` |
| `--last-region` | the region last chosen on Captastic's overlay, on the display it was chosen on |
| `--interactive` | opens Captastic's overlay for the user to choose, and waits |

Options:

| option | effect |
| --- | --- |
| `--output <path>` | where to write the file; a relative path is taken from the current folder |
| `--format <png\|jpeg\|bmp>` | the format, instead of the configured one |
| `--clipboard` | also put the capture on the clipboard |

A window is captured as itself, even where other windows cover it. A title or process that matches
more than one window is refused with the matches listed, never guessed: pick one by `--window`.

`--interactive` opens the overlay on the region tool. **Capture** answers with the selection,
**Pin** answers and pins it on screen, **Edit** answers with the edited image when the user
presses Done, **Record** is refused, and Esc is refused as `cancelled`. If nobody chooses within
two minutes, the overlay closes and the request is refused as `timed_out`. While it waits, other
`captastic-cli` requests wait behind it.

A capture answers:

```json
{"version":1,"status":"ok","path":"C:\\Users\\you\\Pictures\\Captastic\\20261122-211400-475.png",
 "format":"png","width":1799,"height":1160,"taken_by":"daemon","clipboard":false,"capture_id":1,
 "captured":{"kind":"window","handle":2821696,"title":"a.txt - Notepad","process":"Notepad.exe",
 "rect":{"x":75,"y":116,"width":1799,"height":1160},"display":"windows-monitor-DEL41B4-dp0"}}
```

`captured.kind` is `display`, `rect`, `window`, `last_region` or `interactive`. For
`interactive`, `captured.chosen` says what the user chose: its `kind`, `rect`, `intent`, and for a
window its `title` and `process`.

## When there is no capture

```json
{"version":1,"status":"refused","reason":"ambiguous_window","message":"2 windows match; ...",
 "matches":[{"handle":2821696,"title":"a.txt - Notepad"},{"handle":527960,"title":"b.txt - Notepad"}]}
```

`reason` is stable; `message` is for people and may change.

| reason | exit | meaning |
| --- | --- | --- |
| `no_window` | 2 | no window that can be captured matches |
| `ambiguous_window` | 2 | several windows match; they are listed in `matches` |
| `unknown_display` | 2 | no display has that id |
| `outside_desktop` | 2 | the rectangle is empty or reaches outside every display |
| `no_last_region` | 2 | no region has been chosen on a display that still has the same shape |
| `recording_running` | 2 | Captastic is recording; still captures wait until it stops |
| `cancelled` | 2 | the user closed the overlay or the editor without choosing |
| `timed_out` | 2 | nobody chose on the overlay in time |
| `recording_unsupported` | 2 | the user chose Record on the overlay |
| `unsupported` | 2 | not possible here, such as `--interactive` without Captastic running |
| `bad_request`, `unsupported_version` | 2 | the request was malformed or from another version |
| `shutting_down` | 2 | Captastic is stopping |
| `capture_failed` | 1 | the capture was tried and could not be taken |
| `write_failed` | 1 | it was taken and could not be written, for example the file exists |
| `clipboard_failed` | 1 | it was written, and `path` says where, but the clipboard could not be had |
| `engine_unavailable` | 1 | the capture engine is busy or recovering; try again |
| `busy` | 1 | other requests kept Captastic busy; try again |
| `untrusted_pipe` | 1 | something other than this user's Captastic answered; nothing was sent |
| `request_failed` | 1 | the request got no answer |

A wrong command line answers `{"version":1,"status":"usage","reason":"bad_command_line",...}`
with exit code 64.

## Examples

```powershell
# The window an agent is working on, by title.
captastic-cli capture --window-title "Visual Studio Code" --output C:\temp\editor.png

# The primary display, as JPEG, and on the clipboard too.
captastic-cli capture --display primary --format jpeg --clipboard

# Ask the user to choose.
captastic-cli capture --interactive
if ($LASTEXITCODE -eq 2) { 'The user cancelled, or did not choose in time.' }
```

## Checking it on a desktop

`scripts/check-command-line.ps1` runs every command above against test windows of its own, with
and without a daemon, and checks the answers, exit codes and pixels. See its header for what it
does to the desktop while it runs.
