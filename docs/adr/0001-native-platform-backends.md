# ADR 0001: Native platform backends

## Status

Accepted for the prototype.

## Decision

Captastic will share commands, portable CPU-frame metadata, errors, and performance records across operating systems. Native capture sessions and GPU resources will remain inside concrete platform crates.

Generic screenshot libraries may be used as functional baselines, but they will not own the performance architecture unless they expose persistent sessions, native timing, GPU-buffer lifetime, readback control, copy accounting, and recovery behavior.

## Consequences

The Windows implementation can directly measure DXGI and WGC. macOS and Linux can follow their native permission, threading, and buffer models without being forced into a lowest-common-denominator API.

