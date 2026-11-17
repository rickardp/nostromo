---
category: Security
---

# Security Requirements

| Metric | Target | Measured At |
|--------|--------|-------------|
| Additional permission prompts or grants versus ordinary Cargo | Zero, for the same cache location and invoking application/context | Install, wrapper, cleanup and diagnostics |

## Permission parity with ordinary Cargo

**Requirement:** Nostromo must introduce no permission prompt or grant that ordinary Cargo would
not require when writing its cache to the same location from the same application/context.
Permissions already accepted for that baseline must suffice for Nostromo. Do not ask for broader
permissions, Full Disk Access, elevated privileges, or a privacy database reset to repair a hang.
A prompt ordinary Cargo itself would produce for that same location is the baseline, not an
additional Nostromo permission.

**Rationale:** A cache manager must not expand Cargo's privacy requirements. A separate helper
identity or access to an unrelated volume can violate parity even when the intended build output
is already authorized. Compare like-for-like paths and invoking contexts before assigning cause.

Implementation: installation no longer creates a LaunchAgent or configures Time Machine.
Build-triggered cleanup remains a child of the existing Cargo process context. Permission parity
on external volumes and bounded filesystem access still need verification; local tests alone do
not establish compliance. An external build-dir override must not require access to the configured
store merely to discover that the build-dir is outside it.
