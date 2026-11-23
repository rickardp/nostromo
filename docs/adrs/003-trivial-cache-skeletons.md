---
status: Active
date: 2026-11-23
---

# ADR-003: Hide idle cache skeletons and maintain them automatically

## Decision

Hide idle trivial cache skeletons from status and clean them during build-triggered maintenance,
even when the store is below its disk limits. Retain the existing seven-day grace period.

**Applies to:** `src/store.rs`, `src/cli/status.rs`, `src/cli/gc.rs`, `src/evict.rs`.

**Constraints:** Recognize only directories and known regular Cargo lock/probe files. Fail closed
on symlinks, unknown entries and inspection errors. Preserve directories in use or being seeded;
serialize removal with eviction, and recheck contents and all use timestamps under the locks.
Remove tracking metadata only after the directory moves successfully to trash. Status and dry
runs must not delete anything. Populated caches without workspace metadata remain visible.

**Exceptions:** None.

**Enforced by:** `cargo test --test evict_cli` and `cargo test skeleton_detection`.

## Context

Eviction preserves lock-file skeletons to avoid disrupting waiting Cargo builds, then removes
workspace records. Status presented these tiny leftovers as unknown orphans. The existing sweep
ran only during eviction, allowing skeletons to accumulate while disk limits were satisfied.

## Options Considered

### Option 1: Show skeletons as unknown orphans and sweep only during eviction (previous behavior)
- Pro: No separate housekeeping call.
- Con: Clutters status and leaves old skeletons indefinitely when eviction is unnecessary.

### Option 2: Hide idle skeletons and run the existing grace-period sweep during maintenance
- Pro: Keeps useful cache information visible and reclaims old leftovers without disk pressure.
- Con: Requires inspecting skeleton contents during status and maintenance.

## Consequences

**Positive:**
- Trivial leftovers no longer obscure populated caches and active builds.
- Old skeletons and their tracking metadata are removed without waiting for disk pressure.

**Negative:**
- Cleanup waits for the next build-triggered tick or manual GC; there is no idle background timer.
- Unknown files or inspection errors keep directories visible and prevent automatic skeleton removal.

## Related

[Build-triggered maintenance](001-build-triggered-maintenance.md)

[Eviction specification](../SPEC.md#eviction)
