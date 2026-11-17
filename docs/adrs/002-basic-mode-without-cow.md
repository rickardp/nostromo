---
status: Active
date: 2026-11-17
---

# ADR-002: Basic disk management without copy-on-write

## Decision

Provide basic mode on Linux and on macOS stores without CoW. Basic mode routes Cargo intermediates
into the managed store, tracks usage and reclaims idle build-dirs under the existing eviction policy.
It never seeds build-dirs by copying or hardlinking a donor. Linux CoW is out of scope.

On macOS, `init` must fail without CoW unless the current invocation explicitly passes
`--allow-no-cow`. Persist the consent as `allow_no_cow = true` for diagnostics. Reinstallation
still requires the flag on a non-CoW store. Linux uses basic mode without an override.

**Applies to:** `src/util.rs`, `src/scan/`, `src/seed.rs`, `src/config.rs`, `src/cli/`, installation.

**Constraints:** Preserve build correctness, busy-build protection and the existing Cargo permission
context. Use allocated bytes rather than logical file lengths. Do not follow symlinks during scans.
Keep APFS clone-aware accounting and seeding on supported macOS stores.

**Exceptions:** None.

## Context

The `shell` installation target uses Linux/ext4 without reflinks. Disk conservation remains useful
without worktree cache reuse. A macOS external store may also lack CoW; the user must explicitly
acknowledge the reduced functionality when installing there.

## Options Considered

### Option 1: Require macOS/APFS everywhere (previous implementation)
- Pro: One filesystem backend.
- Con: Cannot manage build storage on Linux or other macOS filesystems.

### Option 2: Basic mode with explicit macOS override
- Pro: Retains disk conservation without requiring shared-storage support.
- Con: New worktrees build cold.

### Option 3: Linux reflinks or full-copy seeding
- Pro: Could reuse dependencies across Linux worktrees.
- Con: Linux CoW adds filesystem-specific accounting; full copies increase disk use. Both are out of scope.

## Consequences

**Positive:**
- Linux and non-CoW macOS stores can use managed cleanup.
- macOS installations cannot silently lose CoW functionality.

**Negative:**
- Basic mode does not accelerate fresh worktrees through seeding.
- Linux shared extents created externally are not deduplicated; figures are allocation estimates,
  and reclaimable bytes may be lower on externally reflinked filesystems.

## Related

[Disk conservation](../specs/features/disk-conservation.md)
