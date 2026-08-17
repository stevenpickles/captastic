# ADR 0007: Daemon control channel trust

## Status

Accepted for the Windows prototype.

## Context

A single daemon instance per session is enforced with a named event, `Local\CaptasticDaemonControl-v1`,
created with default security. `captastic stop` opens the same name and signals it. The name is
well known and the DACL is the default one, so any process running as the same user in the same
session can:

- create the name first, so the daemon fails to start; or
- open it and signal it, stopping a running daemon.

Both are denial of service against the user by a process already running as that user.

## Decision

**The trust boundary is the session, deliberately, and is not defended.**

A process running as the same user in the same session can already read the user's files, inject into
their processes, read their clipboard and synthesise their input. A tool that protects its
start-up event against that attacker has protected nothing while implying it protects something. A
tighter DACL would exclude other users, who cannot reach a `Local\` name in this session anyway.

**What is fixed is the diagnosis, which was wrong.** Captastic reported "another Captastic daemon is
already running in this session" whenever the name was taken, whatever had taken it — sending a user
to look for a daemon that did not exist, with no way to discover what did.

The daemon publishes its process ID in a small named record beside the event, and a start-up that
finds the name taken reports what it found:

| what is there | what the user is told |
| --- | --- |
| a running process named `captastic.exe` | another daemon is running, with its PID |
| a running process named something else | the name is held by that process, named |
| a PID whose process has exited | the record is stale and the name should free shortly |
| no record at all | the holder did not identify itself — an older daemon, or a squatter |

The record is owned by the same guard as the event, so it cannot outlive the daemon it describes.
Publishing is best-effort: a daemon that cannot publish still runs, it is merely harder to tell from a
squatter next time.

## Consequences

The failure a user is most likely to meet — two daemons, or a stale name — now names the process
involved, which is enough to act on. The record is advisory and unauthenticated: a process that wants
to impersonate a daemon can publish a PID too. That is consistent with the boundary above, and the
record is a diagnostic aid rather than a credential; nothing trusts it for anything but a message.

The last row of that table is not hypothetical. A daemon built before this record existed holds the
event and publishes nothing, so upgrading produces exactly that message until the older daemon is
stopped — which is the honest description of the situation.

Because signalling remains open to the session, `captastic stop` from any process still stops the
daemon. That is a feature for the CLI and a denial of service for a hostile process, and there is no
way to have the first without the second while the boundary is the session.
