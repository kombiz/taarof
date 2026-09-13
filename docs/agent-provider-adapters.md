# External provider adapters

An adapter is explicitly trusted **same-user executable code**, not a sandbox.
Only an enabled TOML manifest in `$XDG_CONFIG_HOME/agent/providers.d/` (default
`~/.config/agent/providers.d/`) can add one. Finding a program on PATH never
registers it. Installation and enablement are manual; there is no marketplace,
credential injection, download, built-in override or automatic trust.

Copy `examples/agent-provider-adapter/synthetic.toml` into that directory,
replace its absolute script path, then deliberately set `enabled = true`.
The example only prints synthetic new/resume messages; it starts no real agent.
Try `agent providers`, `agent --json`, `agent --new synthetic`, and
`agent --resume "fixture with 'quotes'"`. No picker source change is needed.

Schema 1 requires `schema`, stable lowercase ASCII `id` (letters, digits, hyphen;
1–64 bytes), `display_name`, argv-array `command`, boolean `enabled`, and
`capabilities`. Unknown fields/versions, duplicate IDs, and built-in IDs fail
closed. Capabilities currently supported are `new` and `resume`; optional
capabilities may be omitted. Fork/archive/process identification are not
implemented, so declaring them is rejected rather than displaying dead actions.
Maximum 64 directory entries are inspected and 16 KiB per manifest is accepted.
Use a dedicated providers directory. Adapter program names use fixed
`/usr/bin:/bin`; use an absolute executable path for user-installed programs.

## Protocol v1

Each operation launches a fresh process directly with the manifest argv. Send
one JSON request plus newline on its private stdin pipe, then close stdin.
Return exactly one JSON line on stdout and exit successfully. No shell is
involved, and standard streams are pipes rather than inherited terminal input.
Responses reject unknown fields. All response objects include `protocol: 1`.

Copyable requests and responses (synthetic data only):

```json
{"protocol":1,"operation":"metadata","limit":50}
{"protocol":1,"id":"synthetic","display_name":"Synthetic","capabilities":["new","resume"]}
```

```json
{"protocol":1,"operation":"probe","limit":50}
{"protocol":1,"available":true}
```

```json
{"protocol":1,"operation":"discover","limit":50}
{"protocol":1,"sessions":[{"session_id":"fixture with 'quotes'","title":"Synthetic","cwd":"/tmp","updated_at_unix_ms":1}]}
```

```json
{"protocol":1,"operation":"plan-new","limit":50,"cwd":"/tmp"}
{"protocol":1,"program":"/usr/bin/printf","argv":["%s\n","Synthetic new"],"cwd":"/tmp"}
```

```json
{"protocol":1,"operation":"plan-resume","limit":50,"cwd":"/tmp","session_id":"fixture with 'quotes'"}
{"protocol":1,"program":"/usr/bin/printf","argv":["%s\n","Synthetic resume"],"cwd":"/tmp"}
```

Metadata ID, display name, and capabilities must agree with the manifest.
Session IDs are opaque machine identity; do not derive identity from titles.
Duplicate IDs and relative cwd values invalidate the provider's discovery.
Catalog display fields strip terminal controls and bidi formatting, and bound
Unicode length. As with built-ins, catalog titles use provider plus session ID
rather than potentially private native transcript titles. Returned plans must
preserve the requested cwd, use structured program/argv, and have no NULs.
A successful availability probe is required before planning. Returned launch
programs must resolve on the launcher's execution PATH (which is distinct from
the adapter's minimal PATH); plans pin their canonical executable path.
Plans are always local, require confirmation, and cannot grant live attach or
SSH authority. Planning never executes the returned plan. Adapter authors must
preserve opaque IDs as argument data and honor their own CLI's option boundary.

Operations have a two-second deadline. Each request is at most 16 KiB, stdout
at most one 256 KiB line, stderr at most 8 KiB, discovery at most 50 records,
and plans at most 128 arguments of 8 KiB each. The process group is killed and
the direct child reaped after success or failure; descendant-held output pipes
cannot extend the deadline. There is no inherited stdin or terminal output.
Only HOME, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME, fixed PATH and LANG
reach the adapter. No tokens or ambient provider settings are passed through.
This is resource-bounded cooperation with same-user code, not filesystem or
network isolation.

**No secrets or transcripts:** requests, responses, diagnostics, manifests,
and stderr must contain no credentials, prompt bodies, assistant responses,
tool arguments, or transcript content. Return bounded session metadata only.
Never emit environment values. The launcher discards captured stderr and
returns fixed error categories, so crashes cannot echo secrets into doctor.
The allowlisted environment does not authorize an adapter to retrieve secrets.

`agent doctor` identifies enabled external code, manifest failures, malformed
JSON, protocol mismatches, deadlines, crashes, oversized streams, duplicate
sessions, invalid plans, and unavailable executables without hiding other
providers. An invalid provider contributes no actionable sessions.

## Author conformance

Use a synthetic session store and run:

```sh
bash examples/agent-provider-adapter/conformance.sh synthetic
```

This reusable harness first runs the consumer's hostile display, capability,
malformed/oversized protocol, duplicate identity, environment, timeout,
descendant-pipe, crash, and failure-isolation fixtures. It then invokes
`agent conformance synthetic` against your enabled manifest. That command
checks metadata, probe, bounded discovery, and each advertised new/resume plan;
resume requires at least one synthetic discovery record. It does not launch a
plan or require any real account. Successful JSON uses schema
`agent.adapter-conformance.v1`. Run the same harness with your adapter ID.
