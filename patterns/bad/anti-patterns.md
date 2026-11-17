# Anti-Patterns

Common mistakes to avoid in this codebase. Add a section per anti-pattern in this shape:
category, severity, bad example, problems, correct example, and rationale.

## Permissions: Requesting more access to repair unattended cleanup

**Severity:** Critical

### Don't Do This
```text
Install a LaunchAgent for gc --auto.
When it blocks on removable-volume access, ask the user to grant more permissions.
```

**Problems:**
- A separate launchd process can have a different macOS privacy responsibility identity.
- A background process can await an authorization response without a foreground user flow.
- Asking for more access violates the existing-permissions requirement.

### Do This Instead
```text
Run maintenance from the already authorized Cargo context.
Keep installation and removal free of background-service lifecycle code.
Keep recovery within permissions already accepted by the user.
```

**Why:** The user requires permission parity with ordinary Cargo writing its cache to the same location
from the same application/context. Permission failures
must change the design, not the user's privacy settings. Test process responsibility on macOS;
inheritance is not proven by a successful local build.

---
