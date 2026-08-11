# ADR 0003: Frame freshness

## Status

Accepted for Phase 0.

## Decision

Captastic exposes two modes and never merges their measurements:

- `fresh` waits for a qualifying post-trigger frame within a timeout;
- `latest` drains an immediately available frame at trigger time, otherwise reuses the retained frame, and reports its age.

The daemon performs no Desktop Duplication acquisition while idle. A fast latest-frame result without frame-age data is invalid. Presentation-time provenance is recorded so OS timestamps, inferred arrival timestamps, and synthetic timestamps cannot be confused.

