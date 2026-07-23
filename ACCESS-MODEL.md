<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# CarbideOS Access and Build Model

CarbideOS is an appliance operating system for the AIde fleet, not a general
purpose Linux distribution. Public images exist so people can explore and test
the security model without receiving access to AIde infrastructure.

## Build Types

### Fleet

Fleet builds run AIde services.

- They use production signing keys and the production update feed.
- They contain no default login password or private credential.
- SSH accepts keys only.
- Drydock supplies per-node identity and secrets during provisioning.
- `ace-mgmt` is a recovery operator, not an unrestricted root account.
- Privileged recovery operations are limited by an immutable, signed command
  broker.

### Debug

Debug builds are internal development tools.

- They use development signing keys and cannot be installed on fleet systems.
- They may contain debugging tools, verbose logs, and unrestricted sudo.
- They use a developer-supplied SSH key and never enable SSH password login.
- They must be clearly marked as debug builds and must not use the production
  update feed.

### Playground

Playground builds are public security-testing appliances.

- They have separate signing keys, identity, and update feed from fleet builds.
- They contain no AIde credentials and cannot connect to fleet control systems.
- SSH is disabled by default.
- First boot asks at the local console for a username and password; no shared
  password is built into the image.
- Console users receive the same restricted permissions as `ace-mgmt` on a
  fleet build. They do not receive unrestricted sudo.
- The image is intended to run in a disposable VM that can be restored from a
  snapshot after destructive testing.

The playground should closely match fleet hardening. Differences should be
limited to identity, credentials, update trust, local enrollment, and software
included specifically for demonstration.

## Recovery Operator

The recovery operator can inspect the machine and request a small set of safe
recovery actions. The initial command set should cover:

- system health and failed units;
- bounded, non-interactive journal output;
- update and rollback status;
- signed base OS updates;
- installation and removal of named, allowlisted signed extensions;
- reboot and poweroff;
- a future authenticated scuttle or reprovision request.

The operator must not receive a general root shell, arbitrary `systemctl`,
arbitrary file-writing tools, mount control, block-device access, or the
ability to run an arbitrary command through the service manager. New recovery
operations are added through a signed CarbideOS update after review.

This policy applies equally to fleet `ace-mgmt` and playground console users.
The public playground therefore tests the real operator boundary instead of a
weaker demonstration-only policy.

## What The Playground Tests

A playground user can test whether an ordinary logged-in operator can:

- gain unauthorized root privileges;
- escape the approved recovery command set;
- modify verified operating-system content;
- install an unsigned or modified extension;
- make unauthorized persistent configuration changes;
- access data hidden by filesystem permissions or SELinux;
- abuse an approved operation to execute something broader.

This is not the same boundary as an ACE process. ACE will run as a systemd
`DynamicUser` with no login shell, no sudo access, an isolated filesystem, and
a restricted syscall and capability set. Testing the ACE sandbox requires a
separate challenge workload that starts inside the actual `ace@.service`
sandbox. A console account cannot accurately simulate it.

Both tests are useful and should eventually be available in the playground:

1. Operator test: start from the same permissions as fleet `ace-mgmt`.
2. Workload test: start code inside the same sandbox as an untrusted ACE job.

## Root Boundary

CarbideOS does not promise to safely contain unrestricted root on a running
node. Root can always attack availability by stopping services, erasing state,
or writing to block devices. Verified boot, dm-verity, signed extensions, and
stateless configuration limit what survives and what can be forged, but they
do not make unrestricted root harmless.

The practical security boundary is therefore preventing untrusted users and
workloads from obtaining root. Real fleet recovery that needs unrestricted
root belongs at the hypervisor console, in a separately signed recovery image,
or in reprovisioning through Drydock. It is not a routine SSH capability.

## Network Discovery

Proxmox fleet and debug images may include QEMU Guest Agent so Proxmox can
report their IP addresses. The Proxmox host already controls the guest, so this
does not create a new trust boundary. Other hosting environments should report
their addresses through the authenticated outbound fleet connection.

IP discovery does not require SSH password authentication. Fleet and debug
SSH remain key-only, while playground access begins at the local VM console.

## Release Safety

Fleet, debug, and playground artifacts must have explicit build and publish
commands. A generic public flag must not turn one variant into another. The
release process must prevent debug or playground artifacts from entering the
fleet feed, and fleet systems must not trust debug or playground signing keys.
