<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# carbide-agent socket protocol

**Version:** 1
**Status:** Draft for review
**Socket:** `/run/carbide/agent.sock`, `SocketMode=0600`, root only

The privileged interface to CarbideOS. Any program on the node with root may
drive base and extension lifecycle through it. It is the only supported way to
do so from inside a sandbox.

## Constraints this protocol inherits

From SPEC.MD §6.4, and non-negotiable:

- **The agent never names a particular extension.** Every command takes a name.
- **The agent holds no credentials.** Ever, for any reason.
- **The agent owns what is a property of the OS; the caller owns what is a
  property of the payload.** Where artifacts live is sealed into the base and
  the agent resolves it. Which version to install is the caller's policy.
  Credentials are the caller's problem.
- **The agent does not accept caller-supplied URLs.** Resolution goes through
  sealed configuration or through a local path. Nothing else.
- **Recovery stays local.** Reverting to a retained known-good image must never
  require the network, whatever else the agent learns to do.
- **A command belongs here only if it needs privilege the caller cannot have.**

## Why this exists

Watchtower runs with `ProtectSystem=strict`, `PrivateDevices=yes` and an empty
capability set. It cannot write `/var/lib/extensions`, cannot refresh sysexts,
and cannot resolve `$BOOT`. Shelling to a setuid path would hand the sandbox a
way out. A socket does not, and the answers come back typed rather than scraped
from a program's output.

## Framing

Unchanged from v0: one request per connection, one line of JSON in, one line of
JSON out, connection closed. A caller cannot hold the socket open and starve
the next one. Large payloads are paginated by cursor rather than streamed.

## Versioning

The agent ships in the sealed base image. Watchtower ships as a sysext and
updates in seconds. **They will drift permanently, and the newer side is
usually the caller.** Therefore:

- Every reply carries `protocol` (integer, currently `1`).
- Callers issue `hello` first and negotiate on `capabilities`.
- Unknown commands return `ok:false` with `code:"unsupported"` and the list of
  supported commands. They must never fail to parse.
- Unknown request fields are ignored. Unknown reply fields must be ignored by
  callers. Every field not marked required has a default.
- A caller that cannot parse a reply must report *that*, and must never
  interpret it as "the agent is absent".

The last rule is the whole point. In v0 `AgentReply.ok` was required with no
default, so any drift became a parse failure, which the caller turned into
`None`, which meant "no agent", which fell back to a sandboxed `sysupdate` that
can only ever fail with `$BOOT: Required key not available`. Schema drift was
indistinguishable from the agent not existing.

## Request

```json
{"command": "<name>", ...fields}
```

## Reply

```json
{
  "ok": true,
  "protocol": 1,
  ...command-specific fields
}
```

```json
{
  "ok": false,
  "protocol": 1,
  "error": {
    "code": "unsupported",
    "message": "human summary, one line",
    "stage": "validating",
    "command": "systemd-dissect --validate ...",
    "exit_code": 1,
    "stderr": "trimmed, last 4 KiB",
    "unit": "watchtower.service",
    "log": ["last journal lines relevant to the failure"]
  }
}
```

Only `code` and `message` are required inside `error`. The rest are present
when known. `code` is a stable machine-readable string; `message` is for
humans and may change.

### Error codes

| code | meaning |
|---|---|
| `unsupported` | unknown command; `supported` lists what exists |
| `malformed` | request did not parse |
| `not_found` | named extension, image or unit does not exist |
| `digest_mismatch` | image did not match the digest the caller declared |
| `untrusted` | verity or signature validation failed |
| `base_mismatch` | image declares a base version other than the one requested |
| `busy` | another privileged operation is in flight |
| `no_space` | insufficient space on the state partition |
| `precondition` | refused; see `message` (e.g. required extension not staged) |
| `failed` | the operation ran and did not succeed |

### Stages

`checking`, `validating`, `staging`, `activating`, `soaking`, `reverting`.
Present on failure when the agent got far enough to know.

## Commands

### `hello`

```json
{"command": "hello"}
```

```json
{"ok": true, "protocol": 1, "agent_version": "0.2.0",
 "capabilities": ["update-status", "stage-extension", "activate-extension",
                  "stage-base", "logs"],
 "components": ["chrome"],
 "base_version": "0.1.58"}
```

`components` lists the extension names this agent can acquire on its own. A
caller uses it to decide whether to ask the agent to fetch or to fetch itself
and hand over a path.

### `update-status`

Unchanged from v0 so an old caller keeps working.

```json
{"command": "update-status"}
```

```json
{"ok": true, "protocol": 1, "available": "0.1.59", "pending": false}
```

`available` is absent when the node is current.

### `stage-extension`

Place a validated image for a base version. Does not activate and does not
restart anything. This is how an extension is prepared for a base the node has
not booted yet.

Two forms. **Acquired form** — the agent fetches through its own sealed
configuration:

```json
{"command": "stage-extension", "name": "chrome", "base": "0.1.59"}
```

**Supplied form** — the caller already has the image, because the agent cannot
reach it:

```json
{"command": "stage-extension", "name": "watchtower",
 "path": "/var/lib/watchtower/staging/watchtower.raw",
 "digest": "sha256:...", "base": "0.1.59"}
```

`version` may be given in the acquired form to pin one; absent means whatever
the source offers. `digest` is required with `path` and optional otherwise.
`base` is optional; absent means the running base.

Either way the agent verifies the digest when one is known, validates verity
and signature under the same image policy `systemd-sysext` will use, checks the
image's declared base matches `base`, and only then places it.

```json
{"ok": true, "protocol": 1, "name": "chrome", "version": "0.1.59",
 "staged": "/var/lib/extensions/chrome_0.1.59.raw", "acquired": true}
```

A caller should prefer the acquired form and fall back to the supplied form
only for a `name` absent from `components` in the `hello` reply. Requesting the
acquired form for an unconfigured name returns `not_found`.

### `activate-extension`

Activate a staged image for the **running** base: adopt the current one as
known-good, promote the candidate, refresh, reload, start the unit, wait for
readiness, soak. On failure it reverts to the retained known-good image and
reports why. This is the existing `activate` path, exposed.

```json
{"command": "activate-extension", "name": "watchtower", "version": "0.2.93"}
```

```json
{"ok": true, "protocol": 1, "name": "watchtower", "version": "0.2.93",
 "phase": "active", "known_good": "0.2.92"}
```

On failure the error carries `stage` (`activating`/`soaking`/`reverting`),
`unit`, and `log`, and the agent has already reverted.

### `stage-base`

Download and stage a base image, then set the one-shot boot entry. Refuses if
any extension currently installed for the running base has no image staged for
the incoming one, and names the offender.

```json
{"command": "stage-base", "version": "0.1.59"}
```

```json
{"ok": true, "protocol": 1, "version": "0.1.59", "pending": true,
 "boot_entry": "carbideos-fleet_0.1.59+3-0.efi"}
```

Refusal:

```json
{"ok": false, "protocol": 1,
 "error": {"code": "precondition",
           "message": "chrome is installed but has no image staged for 0.1.59",
           "stage": "checking"}}
```

`version` is optional; absent means "whatever the feed offers".

### `logs`

Read the journal. The agent is unsandboxed and can; the caller usually cannot.
The agent paginates. Chunking for any onward transport is the caller's problem.

```json
{"command": "logs", "unit": "watchtower.service", "lines": 200,
 "cursor": "s=...", "priority": "warning"}
```

```json
{"ok": true, "protocol": 1, "lines": ["..."], "cursor": "s=...", "eof": false}
```

`unit` is optional; absent reads the whole journal for the current boot.
`lines` defaults to 200 and is capped by the agent. `cursor` resumes from a
previous reply. `priority` is an optional maximum syslog level.

Reading the journal is a privileged read of everything on the node, including
whatever other units log. It is deliberately not filtered beyond `unit`,
because the socket is already root-only and a root caller can read the journal
directly if it is not sandboxed.

## Where sources come from

An acquirable extension is a `systemd-sysupdate` component:

```
/usr/lib/sysupdate.chrome.d/50-chrome.transfer

[Source]
Type=url-file
Path=https://updates.aide.gg/carbideos/fleet
MatchPattern=chrome_@v.sysext.raw

[Target]
Type=regular-file
Path=/var/lib/extensions
MatchPattern=chrome_@v.raw
InstancesMax=2
```

The agent runs `systemd-sysupdate --component=chrome` and inherits TLS, resume,
and `SHA256SUMS` signature verification from a binary already sealed in the
base. It adds no HTTP client of its own, which keeps its dependency set — and
therefore the supply chain of the recovery path — at `serde` and `serde_json`.

Someone building CarbideOS for their own fleet points these transfer files at
their own feed once, in the image, and every program on the node inherits it.
That is the whole reason acquisition belongs to the OS rather than to each
caller.

## What deliberately is not here

- **No caller-supplied URLs.** See the constraints above.
- **No credential access.** An artifact behind a credential is fetched by the
  caller that holds it and handed over as a path.
- **No job or fleet concepts.** The agent does not know AIde exists.
- **No `reboot`.** `carbideos-ops reboot` already exists for operators and does
  not need privilege the caller lacks.
- **No streaming.** Cursors are sufficient and keep the framing trivial.

## Caller obligations

1. Issue `hello` and cache `capabilities` for the connection's lifetime.
2. Treat a connect failure with `ENOENT` as "agent absent" — nothing else is.
3. Treat every other failure as an agent fault and report it verbatim. Never
   silently fall back to running a privileged tool from inside a sandbox.
4. Set a timeout on connect, write and read. The agent may be soaking an
   extension for minutes; `activate-extension` needs a caller timeout above the
   ruleset's `ReadyTimeoutSec` plus `SoakSec`.
