<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# CarbideOS

CarbideOS is a minimal, terminal-only Fedora appliance image. The current
implementation provides the signed base image, first-boot disk provisioning,
and manually triggered A/B base OS updates. It contains no AIde, Watchtower,
ACE, Chrome, desktop, or fleet provisioning code.

## Security Properties

- UEFI-only boot with a Unified Kernel Image (UKI).
- Secure Boot signatures on systemd-boot and every generated UKI.
- EROFS root filesystem protected by dm-verity and a signed root hash.
- First-boot A/B slot creation and TPM2-encrypted system, workload, and swap
  partitions through `systemd-repart`.
- SELinux enforcing, kernel module signature enforcement, and kernel lockdown.
- No package manager or desktop environment in the finished image.
- Root password authentication is locked. Direct root SSH is disabled.
- `ace-mgmt` is the only administrative login, is managed by `systemd-homed`,
  and is in the `wheel` group. SSH accepts only its baked public key.
- The admin home is an idmapped Btrfs subvolume inside the TPM2-encrypted
  system-state filesystem and remains active for the boot so key-only SSH can
  open it without a separate plaintext unlock credential.
- Release artifacts are checksummed and signed by a separate OpenPGP key.

The checked-in files never contain private signing keys. Development builds
automatically provision disposable, passphrase-free keys under the gitignored
`keys/` directory when they are absent. Existing key material is never
overwritten. Production and Playground trust require separate, explicit key
ceremonies and must be independently backed up.

## Build

The host needs Linux, mkosi 25.3 or newer, and the dependencies reported by
`mkosi dependencies`. On Debian 13:

```bash
sudo apt install mkosi dnf systemd-boot systemd-ukify systemd-repart \
  erofs-utils mtools sbsigntool python3-cryptography gpg
make pipeline
```

`make build`, `make debug`, and `make pipeline` create missing development
signing keys, a passphrase-free local SSH keypair, and the development admin
password without prompting. The generated password is random and remains in
`keys/admin/ace-mgmt.password`. Run `make keys` directly to provision these
inputs before a build. These credentials are development-only; Fleet and
Playground keys are never auto-provisioned.

`scripts/build` invokes `sudo mkosi build` because creating an enforcing
SELinux filesystem from a non-SELinux host requires permission to write
`security.selinux` extended attributes. The remaining packaging, signing, and
verification stages run as the invoking user.

For boot debugging, `scripts/build --debug` (or `make debug`) creates separate
`carbideos-debug.*` artifacts. The debug image gives `root` the operator-supplied
`ace-mgmt` password, allowing emergency-mode console login. Normal builds keep
root locked. The compressed debug disk and its checksum are also written to
`dist/debug/`; they are not included in the signed production release created
by `make package` and must never be deployed as production images.

The Debian tools tree under `mkosi.output/tools/` is intentionally retained
between builds. `make build` replaces only `carbideos*` output artifacts, so a
normal rebuild does not reinstall the build environment. Use
`make clean-tools` only when that tools tree itself must be recreated.

Release artifacts use Zstandard level 15, a 2 GiB long-distance matching window
(`--long=31`), and all available compression threads (`-T0`).
Decoders must opt into the matching window size. For example, write an image to
a whole disk with `zstd --long=31 -dc carbideos.raw.zst | sudo dd of=/dev/sdX
bs=16M status=progress conv=fsync`.
The boot-critical UKI initrd remains at level 15 with all-core compression but
uses Zstandard's normal window, avoiding a 2 GiB allocation before userspace.

`make pipeline` performs these distinct stages:

1. `make build` creates the signed, verity-protected disk image.
2. `make package` collects only publishable artifacts into `dist/`.
3. `make sign` creates `SHA256SUMS` and its detached OpenPGP signature.
4. `make verify` validates hashes, the manifest signature, key permissions,
   and the image metadata available through `systemd-dissect`.

The package and signing stages also maintain `dist/update-feed/`, containing
the versioned root, verity, verity-signature, and UKI artifacts consumed by
`systemd-sysupdate`.

Optional system extensions have their own pinned builders under `extensions/`.
They are not part of the base image; signed extension DDIs are distributed
alongside the A/B artifacts in the signed update feed.

CarbideOS accepts only signed, dm-verity-protected extension DDIs. Install the
allowlisted Rat Game extension with `sudo carbideos-extension install
rat-game-16` and remove it with `sudo carbideos-extension remove rat-game-16`.

Access and release variants are defined in `ACCESS-MODEL.md`. Development
images use the development trust set. A fleet build is explicit and uses only
the encrypted production keys:

```bash
make fleet-pipeline
```

The fleet pipeline requests the production signing passphrase once through
`systemd-ask-password` and clears it from its environment when complete.

Fleet artifacts are staged under `dist/fleet/` and
`dist/update-feed/fleet/`. Publishing requires the separate `publish-fleet`
target and writes only to the `carbideos/fleet` R2 prefix.

Recursive `rm` operations are preflighted by an immutable guard. Deletion is
refused before any operand is processed if a target resolves inside `/`,
`/boot`, `/efi`, `/etc`, or `/usr`; this also catches `rm -rf /*`. The guard
has no supported override.

## Disk Requirements

First boot expands the image onto the backing disk with `systemd-repart`. The
shipped ESP and A-slot set are fixed, and the inactive B slot is fixed, so a
node reserves roughly 1.9 GiB before any writable partition exists.

Signed extension images live on the encrypted state partition at
`/var/lib/extensions`. Retaining an active image alongside its known-good
predecessor is what makes extension rollback possible, so state carries a hard
2 GiB floor and the largest growth weight.

| Disk | Result |
| --- | --- |
| Under 5 GiB | Provisioning fails; state cannot meet its floor |
| 5 GiB | Minimum. Watchtower only, no room for Chrome |
| 16 GiB | Recommended fleet node with Chrome and job scratch |

`systemd-repart` grows partitions into adjacent free space and never relocates
an existing one. A node provisioned against an older, smaller layout therefore
cannot be grown in place; enlarging its disk only offers space to the final
partition. Reprovision instead.

## Base OS Updates

First boot creates fixed-capacity `_empty` root, verity, and signature slots.
Updates are downloaded from `https://updates.aide.gg/carbideos/`, written to
the inactive slots, and activated by installing the UKI last.

Updates are manual while the mechanism is being proven:

```bash
sudo /usr/lib/systemd/systemd-sysupdate list
sudo /usr/lib/systemd/systemd-sysupdate update
sudo /usr/lib/systemd/systemd-sysupdate pending
sudo /usr/lib/systemd/systemd-sysupdate reboot
```

Fleet operators use `sudo carbideos-ops update` followed by
`sudo carbideos-ops reboot`. The broker explicitly selects the counted UKI and
activates pending updates through `systemd-sysupdate`; do not hard-stop a VM
between writing an update and its first graceful reboot.

No update or reboot timer is enabled. Counted boots that reach
`emergency.target` reboot after 15 seconds, kernel panics reboot after 30
seconds, and the system manager drives the hardware watchdog. After installing
the update-capable baseline once, bump
`mkosi.version`, run `make pipeline`, and publish the accumulated feed with:

```bash
CARBIDEOS_R2_PROFILE=carbide-r2 \
    make publish-r2 SOURCE=dist/update-feed
```

## R2 Update Hosting

`scripts/publish-r2` publishes a prepared update directory to a Cloudflare R2
bucket through its S3 endpoint. It refuses to start if the bucket is already
larger than 5,000,000,000 bytes or if the completed upload would cross that
limit. Existing objects are never deleted automatically.

Install the AWS CLI and configure an R2 token restricted to the update bucket:

```bash
aws configure --profile carbide-r2
export CARBIDEOS_R2_PROFILE=carbide-r2
make publish-r2 SOURCE=dist/update-feed
```

The publisher defaults to the `aidegg-updates` bucket at the CarbideOS R2
account endpoint. `CARBIDEOS_R2_ENDPOINT` and `CARBIDEOS_R2_BUCKET` may still
override those non-secret settings when needed.

The optional `CARBIDEOS_R2_PREFIX` defaults to `carbideos`. Payloads are
uploaded before `SHA256SUMS.gpg`, and `SHA256SUMS` is uploaded last as the
release activation point. Versioned payloads receive immutable cache headers;
the manifest and signature do not.

Each uploaded object records its SHA-256 as object metadata, so a later
publish can prove a payload is unchanged independently of whichever manifest
is currently live. A payload that already exists remotely and cannot be shown
to match is refused rather than replaced. `--adopt-existing` re-uploads such
an object and is only correct when the local signed manifest describes those
exact bytes.

## Update Feed Retention

`scripts/sign` hashes whatever the feed directory contains, so the manifest
decides which versions a node can see. The feed keeps the newest four version
sets; `CARBIDEOS_FEED_RETENTION` overrides the count. Retaining several lets
an operator target a specific version and roll the base OS back through the
feed rather than depending solely on the slot a node still holds:

```bash
sudo /usr/lib/systemd/systemd-sysupdate list
```

Retained versions must exist locally under `dist/update-feed/` for `sign` to
hash them. `dist/` is not tracked, so a wiped build tree narrows the window to
whatever is rebuilt.

Publishing never deletes anything. Objects outside the window accumulate as
unreferenced garbage that no node can verify, so prune against the signed
manifest. The default is a dry run:

```bash
CARBIDEOS_R2_PROFILE=carbide-r2 make prune-r2 SOURCE=dist/update-feed
CARBIDEOS_R2_PROFILE=carbide-r2 make prune-r2 SOURCE=dist/update-feed APPLY=1
```

Pruning is scoped to one channel. Sibling prefixes such as `carbideos/fleet`
and `carbideos/playground` are never considered.

Administrative credentials are mandatory local build inputs under the
gitignored `keys/admin/` directory:

| Account | Authentication |
| --- | --- |
| `root` | Locked; no password login |
| `ace-mgmt` | Operator-supplied SSH public key and homed/`sudo` password |

SSH password and keyboard-interactive authentication are disabled. The admin
password remains necessary for homed activation and `sudo`, but neither it nor
the authorized key is present in the source tree. Use deployment-specific
credentials and protect `keys/` as signing and administrative key material.

## Secure Boot Enrollment

mkosi copies the development certificate into the image for automatic
enrollment when booting a compatible VM in UEFI Setup Mode. This does not alter
the host firmware. For Proxmox, use OVMF, add an EFI disk without pre-enrolled
Microsoft keys, and boot the raw disk in Setup Mode. Never enroll development
keys on production hardware.

Production Proxmox VMs must also use an `i6300esb` watchdog with action
`reset`. This covers hangs that cannot reach the counted-boot emergency retry
service. CarbideOS configures systemd to service it every 60 seconds.

## Scope

The current build implements the A/B base update transport, counted emergency
retries, and TPM/LUKS state, but intentionally does not yet implement automatic
update scheduling, application-aware boot health checks, volatile `/etc`,
carbideos-agent, workload sandboxing, or any closed-source AIde component.

## License

Copyright (C) 2026 CarbideOS contributors. Licensed under
`AGPL-3.0-or-later`. See `LICENSE`.
