# Closed investigations

These are the investigation narratives that used to live in `ROADMAP.md`. They were moved here so
the roadmap carries planning signal and this file carries the evidence behind it. Nothing was
dropped in the move: each section is the record as it was written, edited only enough to read on its
own, and the roadmap keeps a short conclusion and a link at every site a narrative left.

Each of these is closed. Open questions stay in the roadmap.

## Cursor composition, and the two bugs behind it

Optional cursor composition — DXGI pointer shapes, hotspots, visibility, clipping, and
WGC-equivalent semantics — is done, after finding it had never once worked.

Two independent bugs sat between the request and the implementation. `DxgiBackend::capture` still
rejected `CursorMode::Include` with a guard left over from the native-frame milestone, so every
capture that asked for a pointer failed outright while the implementation waited behind it. With the
gate open, composition still drew nothing: DXGI reports the pointer *incrementally and per
acquisition*, filling in position and shape only on a frame that carries a mouse update and leaving
the fields at their defaults otherwise — defaults that read as an invisible pointer at the origin.
So a stationary pointer over a repainting desktop reported not-visible on every frame, and a report
that arrived on a frame later discarded was gone for good. Every acquisition now feeds the pointer
cache before anything decides whether to keep the frame, and `CursorAbsence::PositionNotYetKnown`
separates "has not been told" from "has been told it is hidden".

Rotation is settled, and the answer was that there is nothing to do. Both halves of the pointer
report arrive in the upright desktop space the normalized frame already uses — position exact
against `GetCursorPos` at 0°, 90°, 180° and 270°, shape delivered as the upright cursor rather than
one turned to match the panel — so `RotatedDisplayUnverified` was refusing work that already worked,
and is gone.

Pixel-correctness was then established against a *retained* frame, so composition is the only
difference between the cursor-on and cursor-off captures rather than two moments of a desktop that
repainted in between. The latest run differed in 280 of the 2,304 pixels inside the reported 48×48
pointer rectangle and in none outside it. The separate-measurement half of that exit criterion
remains open and is still recorded in the roadmap, because the two figures that once claimed it
(+171 and −157 microseconds) turned out to be two cursor-off runs under another name.

## Lock phases, and the denials the session can now explain

Lock/unlock diagnosis is covered by a live harness
(`a_locked_session_explains_every_duplication_failure` in
`crates/captastic-windows/src/session.rs`, `#[ignore]`d because it locks the machine for ~8 minutes
and cannot unlock itself). Recovery through lock → displays asleep → unlock is measured by
`scripts/measure-lock-unlock-recovery.ps1`: across a 229 s lock the daemon lost its duplication to
`AccessLost`, had the rebuild refused as `DesktopUnavailable ... the workstation is locked`, waited
on the session probe for 402 polls over 207 s **without walking the adapter list once**, and rebuilt
0.6 s after the unlock with 59 successful captures after it and no intervention.

The same run found one loose end it did not close: a locked-session
`get_physical_cursor_position` denial arrived as a bare `PermissionDenied`, unexplained. That denial
now goes through the same session check `duplicate_output` uses and comes out as
`DesktopUnavailable` naming the lock, keeping the original error whenever the session cannot account
for it — covered by unit tests, and not yet seen by a live lock run. The display-identity query
(`QueryDisplayConfig`) took the same route afterwards; it degrades rather than failing, so what it
changes is a warn line.

It was called the last bare denial on the #51 lineage, and a survey of the crate then found three
more: `create_d3d11_device`, and the two window-capture backends, neither of which had a session
check anywhere in it. All three now go through the same check, which lives in `session.rs` with its
"only on a denial" gate written once instead of at five call sites. `create_d3d11_device` is the one
that changes behaviour rather than wording: a session that refused the capture device used to make a
`display = primary` daemon exit at start-up and, on a rebuild, build a fresh D3D11 device against a
lock screen on an exponential back-off; it now waits on the session probe like every other explained
denial. Unit-tested; no live secure desktop or sandbox has been watched producing the new messages,
and none of the three denials has been reproduced on this host.

The phase structure those checks key on is recorded below, under *Lifecycle recovery*: what ends
duplication is not the lock screen owning the desktop but the display power-down, and
`WTSSessionInfoEx` is the only signal that answers in every phase.

## The 4K DXGI resource step (#53), narrowed

Four runs on 2026-08-17 and 2026-08-18, after a first 9-minute leg showed one unexplained step of
+22 GDI, +21 USER and +65 handles.

A 40-minute DXGI soak at 3840×2160 with the clipboard and file output **off** — 4,513 captures, no
errors, no refusals, display sleep suppressed so it could not confound the counters — held GDI at
exactly 10 for all 478 samples, USER within one, handles within a ±5 band netting −3, and private
bytes within 0.3 MB. An 8-minute idle control was equally flat.

Two further legs restored the rest of the original configuration: the clipboard alone at 250 ms
(5,359 captures, no refusals), then both destinations at 250 ms (2,548 captures, 2,407 files,
4.17 GB, **141 `BufferExhausted` refusals**). GDI held at exactly 10 in every sample of all three
runs — 12,420 captures across 79 minutes — against an original that stepped 10 → 33 and held.

Every Captastic-side suspect is therefore exonerated: the capture path, both destinations, and the
refusal path. What differed in all three runs is display power, uncontrolled in the original and
suppressed here, and it fits the shape exactly — a one-time event allocating a batch of GDI and
USER objects and then holding flat, unrelated to capture volume.

A fourth run powered the display down for 30 seconds with captures running — the monitor entering
sleep confirmed by observation rather than inferred — and GDI and USER did not move at all. That
eliminates the last suspect: **12,472 captures across four runs and 81 minutes, with GDI at exactly
10 in every sample of every run.** The step in #53 is not reproducible under any Captastic-side
condition, and the remaining candidate is the lock transition that occurred during the original
soak, which has never been measured with these counters.

That candidate was then measured as well: a further leg ran captures across a verified lock
transition, bringing the series to the **12,670 captures across 81 minutes, with GDI at exactly 10
in every sample of every run** quoted in the roadmap's soak exit criterion. The four-run total
above and that figure differ by exactly that leg.

Two incidental findings. The refusals were never "250 ms is too fast for 4K": both destinations
lease from the same three-slot CPU pool and the file worker holds its lease across a `Compact`
encode plus the write, so the refusals are the two destinations contending — the clipboard alone at
that rate refused none of 5,359. And Desktop Duplication keeps producing full-resolution frames
while the monitor is asleep, so a sleeping display is still capturable and does not detach from
enumeration.

#53 is closed as not reproducible. The harnesses are `scripts/soak-resource-step.ps1` and the two
probes beside it, `scripts/probe-display-power.ps1` and `scripts/probe-session-lock.ps1`.

## Lifecycle recovery: a daemon with nothing to capture

The daemon no longer exits when enumeration finds no displays. Locked, disconnected, asleep or
unplugged all reach DXGI as an empty output list, so they share one kind — `DesktopUnavailable`,
"not now" — and the daemon waits, registers its hotkeys, and builds the capture engine when a
display appears. Verified end to end on 2026-08-17 with an injected blackout
(`CAPTASTIC_TEST_NO_DISPLAYS_MS`, debug builds only): start with nothing attached, seven triggers
refused with an accurate reason, engine built unattended, 4K captures following.

The measurement that shaped it is worth keeping, with a later correction. A lock does **not** stop
enumeration: displays enumerate with their persistent identities throughout, so a lock is not what
produced the empty display list in #51, and a fix keyed on the lock would have missed the failure it
was filed about. That original condition — an empty list *and* a denied `QueryDisplayConfig` — has
still not been reproduced on demand. The denied `QueryDisplayConfig` half no longer reports itself as
a permissions problem when it does happen: it goes through the same session check as the other two
denials and comes out as `DesktopUnavailable` naming the session state, keeping the original error
whenever the session cannot account for it. That is unit-tested and unmeasured, for the same reason
the condition is un-reproduced. The same is now true of `create_d3d11_device` and of both
window-capture backends, which had no session check at all; the check itself lives in `session.rs`,
and its "ask the session only on `E_ACCESSDENIED`" gate is written once rather than at each call
site.

The first run of that test also had a fresh daemon build a duplication 0.3 s after the lock engaged,
which led to the overly strong claim that a lock does not break DXGI at all. A later run with a
daemon already holding a duplication showed otherwise: at the lock it takes `AccessLost` (`the keyed
mutex was abandoned`), and the rebuild is refused with `DesktopUnavailable in dxgi/duplicate_output:
the session is locked or a secure prompt owns the desktop`.

That correction was itself too strong, and continuous sampling across three lock cycles replaced it.
Duplication is not refused *because* the lock screen owns the desktop — for as long as the lock
screen is lit it keeps working, 12 seconds of it in one run, and `OpenInputDesktop` answers
`Default` throughout because Windows 11's lock screen is an ordinary application rather than a
secure desktop. What ends duplication is the **display power-down**: about three seconds after the
monitors sleep it begins refusing, and one 190-second run held that state for 125 seconds. A
sleeping display is capturable while unlocked, so it is the combination that refuses.

The refusal that the earlier run saw was real but was the credential-prompt phase, which is the one
phase the desktop probe can see. In the phase that matters — locked, displays asleep, `Default`
desktop — the probe reported an ordinary interactive session on all 499 failing samples, so the
failure arrived as a bare `Access is denied`. `WTSSessionInfoEx` answers in every phase and is now
what the probe keys on.

## The 2026-08 code, architecture, and roadmap review

Fully remediated. The three confirmed High defects and the silent-drop, sticky-failure, config-write
and dormant-surface findings were fixed on the review branch, and the Medium findings that were
deferred into batched issues — window-capture alpha and geometry parity, timeout budgets, worker
exhaustion, log-rotation coexistence, the clipboard's stored-DEFLATE encoder, unlocked state writes,
and the unreachable schema error — closed alongside the overlay extraction and Milestone 4.
