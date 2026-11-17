---
status: Active
maturity: Exploring
module: src/evict.rs
related_adrs: []
---

# Disk Conservation

## Overview

Nostromo's primary objective is to conserve disk space used by Cargo build intermediates.
Speedups from reusing sibling worktree caches are a secondary benefit.

## User Stories

- As a developer running Rust builds across worktrees, I want idle build intermediates managed
  and reclaimed so they do not fill the disk.

## Requirements

- Prioritize disk conservation when evaluating product effectiveness and implementation choices.
- Assess store footprint, reclaimed disk space, and free-space headroom against configured limits.
- Treat faster worktree builds as a bonus; speedup is not the main success criterion.
- Preserve build correctness and protect build-dirs in use while reclaiming idle storage.
- Keep Nostromo transparent to developers and agents running ordinary Cargo commands: add no
  output to stdout, stderr, or the controlling terminal, and preserve wrapped-command output
  and exit status. Recoverable Nostromo failures must also stay silent.
- Emit build-time Nostromo diagnostics only with explicit `NOSTROMO_DEBUG=1` opt-in. Keep stored
  disk-pressure warnings available through `nostromo status`; builds must not consume them.
- Do not infer disk savings or speedups from successful builds and tests alone.

## Implementation State

**Current focus:** None; this record captures the product priority.

| Milestone | Status |
|-----------|--------|
| Managed intermediate store and eviction policy | Done |
| Build, seeding, and eviction correctness checks | Done |
| Quantify disk conservation in production workloads | Not Started |

**Open questions:**
- Disk-space savings must be quantified on representative workloads before making measured benefit claims.

**Constraints:**
- Existing soft limits and protected-build policies apply; this priority does not make them hard caps.
- Permission requirements must match ordinary Cargo using the same cache location and context.
