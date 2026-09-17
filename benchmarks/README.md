# Benchmark evidence and the claim procedure

Captastic publishes no performance number that is not backed by artifacts committed here. This file
is the procedure for producing them, and the reasoning behind each step that would otherwise look
like ceremony. The tooling it relies on landed in PRs #87 (the host fingerprint and compatibility)
and #91 (the raw artifacts, `benchmark compare`, and this procedure).

The failure this guards against is not a wrong number. It is a *plausible* one. Two cursor-on runs
on this project were once reported at +171 and −157 microseconds before per-capture outcome counting
revealed that all 600 captures in each had returned `absent_not_visible`: they had measured
cursor-off twice, under another name, and nothing in the output said so. Every check below exists
because some version of that already happened.

## What the pieces are

**A budget** (`budgets.toml`) is a claim about hardware, not about correctness — which is why it
lives here rather than in the test suite. `captastic benchmark --budgets benchmarks/budgets.toml`
judges a run against it, and a budget *names the host it describes*: a run anywhere else is skipped
**loudly**, with every mismatch named and the measurements still reported, because a GPU timing
budget evaluated on a hosted CI runner fails every time and a check that always fails is one nobody
reads. A breach on a matching host fails the command. Relative budgets (`p99 within 6× p50`) are
populated, since a ratio needs no calibration; the absolute ceilings are deliberately empty until
they are measured by the procedure below.

**The fingerprint** is what every report carries under `environment`, and what `RunCompatibility`
keys on when it decides whether two runs may be compared at all:

- backend, capture mode, cursor mode, whether a CPU frame was taken, and whether the run was
  synthetic;
- the whole build — version, short commit, profile, target, and the dirty flag — because two
  `0.2.0-dev` builds can be a hundred commits and an optimization level apart;
- every display as `id:WxH@rotation:scale:refresh`, because scale and refresh change the
  measurement without changing the geometry;
- every adapter as `description (driver version)`, because a driver update is the likeliest single
  cause of a latency change between two runs a week apart on the same machine;
- the OS build, the session state, and the power source.

Anything in that list differing between two runs stops a comparison. That is the point.

**An artifact set** is what `--output-dir` writes: `run-1.json … run-N.json` (a whole report each),
`run-N.events.jsonl` per run when `--raw-events` is given, and `repeated.json` holding the set, its
compatibility, its per-stage agreement, and the budget verdict. `captastic benchmark compare` reads
either shape back.

## The operator procedure

Every step is a precondition for the claim, not a suggestion.

### 1. Prepare the host

- A **console session**, physically at the machine. Remote Desktop composes onto a virtual display
  adapter DXGI will not duplicate, and a locked session measures a desktop nobody is looking at.
- **AC power.** The same machine clocks down on battery and the difference is silent.
- **Something repainting on the primary display** for the whole run — a looping video, a clock with
  a second hand, anything. `latest` mode reuses the last retained frame when the desktop has not
  presented a new one, so a run against a static desktop measures the retained-frame path and not
  acquisition. This is the step most easily skipped and the one that most changes the figures.
- Close what you can. The measurement includes whatever else is compositing.

### 2. Check the preflight, and read it

```powershell
cargo build --release -p captastic-app
captastic doctor --json
```

In the `environment` block, all four must hold:

| Field | Required | Why |
|---|---|---|
| `session` | `"interactive"` | anything else is not this host |
| `power_source` | `"ac"` | battery is a different machine |
| `adapters[].software` | `false` for the adapter driving the displays | a software rasterizer measures a CPU emulating a GPU |
| `build.dirty` | `false` | a dirty tree is the one state nobody can reconstruct from a commit id afterwards |

A dirty tree fails the claim rather than the run: the numbers are fine, but nothing can ever be
built again that produced them.

### 3. Run each cell of the matrix

Two cursor modes × the capture modes being claimed. For each:

```powershell
captastic benchmark --backend dxgi --cpu-frame true --repeat 3 --iterations 200 --warmup 20 `
    --budgets benchmarks\budgets.toml --output-dir benchmarks\runs\<date>\<mode>-<cursor>
```

Add `--raw-events events.jsonl` to keep the per-capture event streams as well; under `--repeat`
they are written into the output directory as `run-N.events.jsonl`, one per run — the path given to
`--raw-events` is not used, and the run says so — and the command refuses the flag without
`--output-dir` rather than quietly writing nothing.

Each run's files are written the moment that run finishes, so a set that fails on its third run
still leaves runs 1 and 2 on disk; only `repeated.json` waits for the whole set. **A directory that
already holds artifacts is refused**, naming them, before a single capture is taken: two sets mixed
in one directory read as one set that never ran — new `run-1.json` beside a stale
`run-1.events.jsonl` and an orphaned `run-3.*` from a longer previous set, under a `repeated.json`
describing neither. Give each cell a directory of its own, or pass `--overwrite` to replace the
previous set (which removes only `run-*.json`, `run-*.events.jsonl` and `repeated.json`, so notes
you left beside the evidence survive).

For a cursor-on cell the pointer must be **visible over the primary display for the whole run**,
which in practice means leaving the mouse alone somewhere on that display and not touching the
keyboard.

### 4. Accept or discard the set

A set is accepted only if **every** stage agrees:

- every stage's p50, p95 and p99 spread is **≤ 7 %** — `repeated.json` reports all of them under
  `agreement.stages`, and the console prints one line per stage. Not just the native frame: a set
  whose medians agree and whose tails do not is exactly the set that must not be accepted on the
  strength of its medians;
- `incompatibilities` is empty;
- for a cursor-on cell, **`cursor_outcomes.composited == 200` in every run**. This is the check the
  two mis-measured runs above failed. A run with `absent_not_visible` counts measured cursor-off;
  it is not a cursor-on run with a caveat, it is the wrong run.

A set that fails any of these is discarded and re-run. It is not averaged in, annotated, or quoted
with a footnote.

### 5. Commit the accepted sets

```
benchmarks/baselines/<host-slug>/v0.2.0/<mode>-<cursor>/
```

The whole directory: every `run-N.json`, every `run-N.events.jsonl`, and `repeated.json`. A figure
whose supporting runs went to a console and were lost cannot be checked again.

### 6. Only now, fill in the absolute budgets

With accepted sets in hand, set the `[absolute]` ceilings in `budgets.toml` from the measured p99
plus **stated** headroom — write the headroom into the comment beside each ceiling, so a later
reader knows whether a breach means a regression or a budget set too tight. An invented ceiling
either passes trivially and protects nothing, or fails honestly and gets deleted.

### 7. Quote the numbers

In release notes, quote **p50 and p95 per stage**, and with them:

- the fingerprint block from `repeated.json` (`compatibility`), so the claim carries its host;
- the measured spread, so the reader knows what the number's own precision is.

A latency figure without the host and the spread is a number with no way to be wrong.

### 8. Before publishing a later claim, compare

```powershell
captastic benchmark compare benchmarks\baselines\<host-slug>\v0.2.0\<cell>\repeated.json `
    benchmarks\runs\<date>\<cell>\repeated.json
```

Either side may be a single `run-N.json` or a whole `repeated.json`. The comparison refuses, with
exit status 2, if the two did not measure the same thing — naming every differing field with both
sides. Otherwise it prints a per-stage table: baseline and candidate p50 and p95, the deltas, both
sets' own spreads, and a verdict of `within_noise`, `slower`, `faster`, or `unmeasurable`.

A stage is only `slower` or `faster` if it moved further than the widest of the two sets' own
spreads and `--noise-percent` (default 7.0). `unmeasurable` means one side's p50 was 0 ns, so there
was nothing for a percentage to be of — the raw figures are still printed, and the stage needs more
iterations before it can be compared at all. A `slower` verdict is **not** a command failure: a
comparison is a measurement still being interpreted, and a command that exits non-zero on one is a
command that gets run with `|| true` until the day it would have mattered.

## What is not automated, and why

Judgement. The tooling collects the artifacts, computes every spread, and refuses comparisons it
cannot justify; deciding that a set is good enough to publish is the operator's, and
`scripts/benchmark-claims.ps1` orchestrates the mechanical parts without making that call.
