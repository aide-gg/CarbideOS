<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Rat Game 16 System Extension

This builder compiles upstream commit
`277d18d3dc6e999692f62352e67d5aa8bd0b92b4` as a hardened static PIE and
packages it with its assets in a signed, dm-verity-protected system extension
DDI. The extension is pinned to the `VERSION_ID` in `mkosi.version` and signed
with CarbideOS's Secure Boot key, which is available to the kernel's platform
keyring for verity signature validation.

Build it with:

```bash
RAT_GAME_SOURCE=/path/to/rat-game-16 ./extensions/rat-game-16/build
```

Omit `RAT_GAME_SOURCE` to clone the pinned source from GitHub. Install and
activate the resulting extension on CarbideOS with:

```bash
sudo install -Dm0644 rat-game-16.raw /var/lib/extensions/rat-game-16.raw
sudo systemd-sysext refresh
rat-game-16
```

The wrapper stores the writable debug log under
`${XDG_STATE_HOME:-~/.local/state}/rat-game-16`; the ELF and game assets remain
read-only in the extension.
