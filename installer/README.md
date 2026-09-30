<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# CarbideOS Installer

A bare-metal installer that renders directly to DRM/KMS and writes a
CarbideOS image to a local disk. The entire installer — interface, kernel,
and the image it installs — is one signed Unified Kernel Image.

## Scope

The installer selects a target disk and writes the image. Partitioning,
encryption, and Secure Boot enrolment are left to CarbideOS's own first boot.

1. Enumerate eligible disks.
2. Write the image after a hold-to-confirm.
3. Reboot.

## Build

```bash
make build        # the CarbideOS image the installer will carry
make installer    # the installer UKI around it
```

Which image gets carried is explicit:

```bash
make installer                       # use mkosi.output, building one only if absent
make installer VERSION=0.1.53        # a packaged release under dist/
make installer IMAGE=path/to.raw.zst # an arbitrary artifact
```

`--fleet` and `--playground` select the matching trust set. The version label
is taken from the carried image rather than from `mkosi.version`.

## Artifacts

Publishable artifacts are collected into `dist/` alongside a `SHA256SUMS`:

| Variant | Location |
| --- | --- |
| development | `dist/carbideos-installer-<version>/` |
| fleet | `dist/fleet/carbideos-installer-<version>/` |
| playground | `dist/playground/carbideos-installer-<version>/` |

| Artifact | Use |
| --- | --- |
| `carbideos-installer_<version>.iso` | Boot media: optical, BMC virtual media, or USB |
| `carbideos-installer_<version>.efi` | The bare UKI, for firmware pointed at an EFI binary |

The ISO boots over UEFI as optical media, over a BMC's virtual media, or
written directly to a USB stick:

```bash
sudo dd if=carbideos-installer_<version>.iso of=/dev/sdX \
    bs=16M status=progress conv=fsync
```

The UKI is signed with the same Secure Boot key as the image it carries, so
it boots wherever CarbideOS boots.

## Requirements

The decoded image is held in RAM, so the installer wants roughly 2 GB free
and is happiest with 4 GB.

A disk is offered when it is not read-only, not the booted installation
media, and at least 5 GiB.

## Preview

Render every screen to `installer/preview/*.png` without booting hardware:

```bash
make installer-preview
```

## Unattended write

```bash
CARBIDE_PAYLOAD=carbideos.raw.zst CARBIDE_META=image.meta \
    carbide-installer --write /dev/sdX
```

Runs the same engine with no DRM device and no operator.

## Keyboard

| Key | Action |
| --- | --- |
| `↑` `↓` | Select target |
| `Enter` | Continue, then confirm the erase |
| `Esc` | Back, or abort an armed erase |
| `S` | Rescan devices |
| `R` | Reboot (any screen except during the write) |
| `P` | Power off (any screen except during the write) |
