# ADR 0008: What a capture reveals by default

## Status

Accepted for the Windows prototype.

## Context

Two Milestone 4 defaults decide how far a capture travels beyond the moment it was taken. Both were
deliberately left unmade in code (issue #44) because either behaviour is a few lines, and the
easiest one to implement should not be the one that ships.

**Where the clipboard goes.** Captastic publishes every capture to the clipboard. Windows Clipboard
History retains what is placed there, and Cloud Clipboard syncs it to the signed-in Microsoft
account and on to that account's other machines. Saying nothing means both apply — so pressing a
hotkey can put a password manager, a private message, or an unreleased design onto someone else's
server, and nothing in the act of taking a screenshot suggests that.

**What the filename says.** `{application}` and `{title}` are filename tokens. A window title is
content: document names, ticket titles, customer names, full URLs. A filename is also visible in
places the image is not — a file picker during a screen share, a backup index, a sync client's
activity feed, a directory listing pasted into a chat.

## Decision

**The clipboard declines both retention paths by default. The filename says what was captured by
default.** These point in opposite directions on purpose, and the reason is the same one both
times: what can the user undo?

Clipboard retention cannot be undone. A capture that has reached an account is not recallable, and
the user was never told it was going. A user who wanted Win+V history and does not get it has lost a
convenience they can switch back on in one line of configuration. The asymmetry between those two
mistakes is the whole argument, so the default is the recoverable one:

    [clipboard]
    allow_history = false
    allow_cloud_sync = false

Implemented as the two documented registered formats, `CanIncludeInClipboardHistory` and
`CanUploadToCloudClipboard`, each carrying a `DWORD` of zero. `ClipboardRetention::default()`
declines both, so the value a caller gets by making no decision is the conservative one. The markers
are transferred before the pixels, and a marker that cannot be set fails the publish rather than
publishing a capture that would then be retained against the user's configuration.

A filename, by contrast, is local, visible, and changeable. The user sees it in their own capture
directory the first time, and the template that produced it is one line of configuration. So the
default names what was captured:

    filename_template = "{timestamp}-{application}-{title}"

Timestamp first, so a directory listing still sorts chronologically. The tokens degrade to nothing
when a capture has no window — a display or region capture yields the timestamp alone — and the
separators collapse rather than leaving a name pocked with empty fields.

## Consequences

Filenames now carry content the image also carries, into places the image does not go. That is a
real exposure and it is the cost of this default: a screen share showing a save dialog, or a backup
index, will show document and ticket titles. The mitigation is that the user can see it happening
and can change it, which is exactly what is not true of the clipboard.

The sanitizer already treats titles as hostile input — path separators, parent hops, reserved device
names, control characters, and length are all handled, and a name cannot escape the output directory
— so this decision changes what a filename *reveals*, not what it can *do*.

Declining clipboard retention is a request to Windows, not an enforcement. Windows honours these
formats; a third-party clipboard manager reading the clipboard directly is under no obligation to,
and this ADR claims nothing about those. What can be verified from inside Captastic is that both
markers reach the real clipboard as a zero `DWORD`, and two ignored tests do exactly that.

Neither Clipboard History nor Cloud Clipboard was enabled on the development host, so the
end-to-end behaviour — a capture published and then absent from Win+V — has not been observed here.
The formats themselves are demonstrably live: an ordinary Chromium copy on that host carries both.
