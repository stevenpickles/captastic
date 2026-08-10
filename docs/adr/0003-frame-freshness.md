# ADR 0003: Frame freshness

## Status

Accepted for Phase 0.

## Decision

Captastic exposes two modes and never merges their measurements:

- `fresh` waits for a qualifying post-trigger frame within a timeout;
- `latest` snapshots the newest warm frame and reports its age.

A fast latest-frame result without frame-age data is invalid. Presentation-time provenance is recorded so OS timestamps, inferred arrival timestamps, and synthetic timestamps cannot be confused.

