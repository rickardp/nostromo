---
status: Proposed
date: 2026-11-15
---

# ADR-001: Trigger maintenance from Cargo's existing process context

## Decision

Use the existing Cargo wrapper to trigger maintenance instead of installing a LaunchAgent.
Do not configure Time Machine automatically. Installation and removal only edit Cargo configuration
and manage the store; they contain no background-service lifecycle or migration code.

**Applies to:** `src/cli/init.rs`, `src/cli/doctor.rs`, `src/wrapper.rs`, cleanup scheduling.

**Constraints:** Introduce no prompt or grant beyond ordinary Cargo writing its cache to the same
location from the same application/context. Use permissions already accepted for that baseline. Never request expanded
privacy access to recover from blocked operations. Validate inherited macOS privacy responsibility
on supported volumes and invoking contexts.

**Exceptions:** None to the user's existing-permissions requirement.

## Context

Cargo can be invoked by terminals, IDEs, and automated build hosts. A separately launched
maintenance process can have a different privacy responsibility identity from the process
that already builds the project. Triggering maintenance from Cargo avoids adding an independent
permission context. It also keeps installation independent of a system service manager.

## Options Considered

### Option 1: Independent LaunchAgent (previous implementation)
- Pro: Runs every minute even when Cargo is idle.
- Con: Introduces a separate privacy context that requested additional access.

### Option 2: Build-triggered maintenance
- Pro: Uses the existing Cargo process context and existing maintenance mechanism.
- Con: No periodic idle cleanup; final accounting can wait for the next build.
- Con: Permission behavior must be validated for supported volumes and invoking contexts.

## Consequences

**Positive:**
- No new LaunchAgent or automatic Time Machine setup during installation.
- No obsolete background-service lifecycle or migration code remains.

**Negative:**
- No maintenance while no builds run, unless the user explicitly invokes GC.
- Blocking filesystem operations in hooks remain a separate reliability gap.

## Related

[Existing permissions requirement](../specs/non-functional/security.md)
