<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# carbide-agent

`carbide-agent` is the recovery component of CarbideOS. It ships in the base
image, so it is protected by the A/B update mechanism, and it is the only thing
on the node capable of undoing a bad system extension.

Base OS updates get A/B slots and boot counting: a bad image fails, the counter
reaches zero, and firmware falls back. System extensions get none of that. A
broken extension merges perfectly, the OS comes up healthy, and whatever the
extension provided is simply gone or crash-looping. `carbide-agent` is the
mitigation.

## Scope

The agent is a **generic extension supervisor**. It does not know what any
extension does, and it must never be taught. CarbideOS is a general-purpose
immutable server OS; the payload is cargo.

It does exactly four things:

1. Merge extensions at boot and reload the service manager, so units arriving
   from an extension become visible.
2. Health-gate the extensions this node declares required, following a ruleset
   each extension ships inside itself.
3. Restore the retained known-good image when an extension fails.
4. Report the result, and withhold boot assessment when the node cannot work.

### Non-goals

These are boundaries, not omissions. Violating any of them defeats the purpose
of having a small recovery component.

- **No fetching.** The agent never downloads anything. Whoever owns the
  credentials owns the fetching.
- **No credentials.** No API keys, no tokens, no message bus. A component that
  holds a secret is a component whose compromise costs something.
- **No network.** Recovery must work on a node with no route to anywhere.
- **No extension semantics.** No hardcoded unit names, no product names.
- **No fleet logic.** Scheduling, rollout policy, canary selection and job
  handling belong to the payload.
- **No execution from mutable paths.** See "Trust" below.

If it needs to change often, something has been put in it that does not belong.

## Trust

The agent runs as root and executes commands named by ruleset files. That is
only safe because those files are verity-backed:

- Rulesets are read **only** from `/usr/lib/carbide/health.d/`, which resolves
  into either the sealed base image or a merged extension. Both are dm-verity
  protected and PKCS#7 signed.
- Rulesets are **never** read from `/etc` or `/var`. A ruleset found there is
  ignored, not honoured.
- A ruleset naming a command outside a verity-backed path is rejected and the
  extension is treated as failed.

## Required extensions

An extension's own ruleset cannot express whether the node is supposed to have
it, because a node that has lost the extension entirely would then have nothing
telling it anything is wrong. That is the exact failure this component exists
to catch, so the requirement is recorded **outside** any extension.

- Default is **empty**. A base image with no required extensions gates nothing
  and behaves like a plain appliance. Development and playground builds are
  therefore correct by default rather than by special case.
- Provisioning declares the requirement, which is what marks a node as
  belonging to a particular deployment.
- The record lives on the encrypted state partition in the agent's state file.

Its integrity is bounded by the same root threat as everything else under
`/var` (see SPEC.MD §12.3). The realistic risk is drift, not forgery.

## Ruleset format

Shipped inside the extension at `/usr/lib/carbide/health.d/<name>.conf`.
Because it travels with the image, it cannot drift from the version it
describes, and a rollback automatically restores the matching rules.

```ini
[Extension]
# The unit that must come up. Required.
Unit=example.service

# How long to wait for the unit to become active. A Type=notify unit should
# only signal readiness once it is actually able to do its job.
ReadyTimeoutSec=90

# Start attempts before the extension is considered failed.
StartAttempts=3

# Optional deeper probe. Must exit 0. Must live on a verity-backed path.
# Readiness alone cannot distinguish a working service from one that is
# running but useless.
HealthCommand=/usr/libexec/example health
HealthIntervalSec=10
HealthTimeoutSec=15

# How long the extension must stay healthy before a candidate is promoted to
# known-good. Without a soak, a service that dies after 30 seconds is recorded
# as the version to fall back to.
SoakSec=120
```

## Image slots

All images live on the encrypted state partition.

```
/var/lib/extensions/<name>.raw                       active, merged
/var/lib/extensions/<name>.raw.<version>.rollback    retained known-good
/var/lib/extensions/<name>.raw.<version>.candidate   staged, not yet active
```

**The suffixes are load-bearing.** `systemd-sysext` merges every `*.raw` in the
directory. A retained image whose name ends in `.raw` would be merged
simultaneously with the active one. Do not "tidy" these names.

Storage budget is three images per supervised extension: active, known-good,
and a candidate mid-update. This is why the state partition carries a hard
floor rather than competing for leftover space.

## Activation

The payload stages a verified candidate and asks the agent to activate it. The
agent never fetches; it only arbitrates between images already present.

```
idle
  -> staged        candidate present, signature and compatibility checked
  -> activating    candidate promoted to .raw, sysext refresh, manager reload
  -> starting      unit started, waiting for readiness
  -> soaking       readiness reached, health probes running
  -> active        soak passed; previous demoted to .rollback, candidate cleared
```

Failure at any point after `activating`:

```
  -> reverting     known-good restored to .raw, refresh, reload, start
  -> recovered     previous version healthy again; rollout should halt
  -> unrecoverable no known-good, or the known-good also failed
```

Before staging is destructive, the agent must confirm free space for the
candidate. Filling the state partition mid-update is a way to wedge a node that
no amount of rollback logic recovers from.

`unrecoverable` is terminal. The agent reports it and stops. It must not loop
between two broken versions, and it must remain reachable over SSH so the node
can be repaired or reprovisioned.

## Boot assessment

The agent gates boot blessing, but only after extension-level recovery has been
exhausted. The ordering matters because the two failure modes have different
remedies:

- **A broken extension.** The agent rolls it back, the node becomes healthy,
  and the boot blesses normally. The base image was never at fault.
- **A base image that breaks a working extension** — an incompatible
  `SYSEXT_LEVEL` or `VERSION_ID`, so a previously fine extension refuses to
  merge. Extension rollback cannot help. The gate fails, the boot goes
  unblessed, and A/B returns the node to the previous base image, which is the
  correct remedy.

Extensions live on the state partition, which is **shared by both base slots**.
Rolling back the base OS does not restore extensions. Gating on extension
health before attempting extension recovery would therefore trigger a base
rollback that cannot fix the problem.

Implemented as a unit ordered `Before=boot-complete.target`, since
`systemd-bless-boot.service` runs after that target. With an empty required
set, it succeeds immediately.

A node whose required extension is absent with no known-good to fall back to
fails the gate. On a freshly provisioned node this is safe, because
provisioning is the moment when something is present to retry it.

## State

Persisted on the state partition, written with the same durability discipline
used elsewhere: write to a temporary file, `fsync`, atomic rename, `fsync` the
directory.

Per supervised extension:

- required
- active version and digest
- known-good version and digest
- candidate version and digest
- current phase and request id
- last health result and timestamp
- last failure reason

Completed request ids are retained, bounded, so a replayed activation cannot
silently invert a rollback.

## Interface

The binary ships at `/usr/bin/carbide-agent` inside the sealed base image.

```
carbide-agent health-gate              verify every required extension
carbide-agent activate NAME VERSION    promote a staged candidate
carbide-agent rollback NAME            return to the known-good image
carbide-agent require NAME             record that this node must have NAME
carbide-agent unrequire NAME           stop requiring NAME
carbide-agent status                   report state as JSON
```

`activate` accepts an optional third argument, a request id. Repeating an id
that has already completed is a no-op, so a replayed command cannot invert a
rollback that happened in between.

`require` is what provisioning calls to mark a node as belonging to a
deployment. Until something is required, the agent gates nothing.

`health-gate` is invoked by `carbide-agent-health.service`, which is ordered
`Before=boot-complete.target` and installed `RequiredBy=boot-complete.target`.
`systemd-bless-boot.service` carries `Requires=boot-complete.target`, so a
failure here prevents the target being reached and the boot is never marked
good. `RequiredBy` rather than `WantedBy` is what makes the failure propagate
instead of merely being logged.

systemd is driven through its command line tools rather than D-Bus. A bus
client is a large dependency to carry inside the one component that has to keep
working when everything else is broken.

## Acceptance

The agent is not finished until each of these is demonstrated on a real node.
SPEC.MD §13 makes the deliberately broken extension a release gate: if it does
not recover itself, nothing ships.

1. Valid activation succeeds without a reboot.
2. Extension with an invalid signature is refused before the active pointer
   moves.
3. Correctly signed extension built against the wrong base version is refused.
4. Unit crashes immediately — rolled back.
5. Unit hangs and never signals readiness — rolled back on timeout.
6. Unit signals readiness but fails the health probe — rolled back.
7. Unit passes readiness and health but dies inside the soak — not promoted.
8. Power loss during staging leaves the active version untouched.
9. Power loss between promotion and refresh resolves deterministically.
10. Both candidate and known-good fail — terminal, reported, no loop, SSH alive.
11. Required extension absent with no known-good — terminal, reported.
12. Empty required set never gates anything.
13. A base image that orphans a healthy extension fails the gate and the node
    returns to the previous base slot.
14. Insufficient free space refuses the update instead of starting it.
