# Release Playbook

`taarof` publishes Linux release tarballs from GitHub Actions when a `v*` tag is
pushed. The tarball is the supported Linux x86_64 install artifact and the
source artifact for downstream binary packages. Pull requests continue to validate
formatting, tests, builds, and local Linux packaging before release tags are cut.

## CI runner isolation policy

Every pull-request-reachable job runs on a statically selected GitHub-hosted
runner (`ubuntu-latest`), not persistent self-hosted infrastructure. This
includes the normal CI jobs plus Claude's pull-request review and review-comment
entry points; collaborator status is not an exception to runner isolation. A
GitHub-hosted runner is a fresh, single-use VM per job with no workspace state
carried over between runs, so a pull request cannot execute code on shared build
infrastructure or exfiltrate anything durable.

`ci.yml` declares workflow-level `permissions: { contents: read }`. Its fork PR
jobs use only the default read-only `GITHUB_TOKEN`; an untrusted pull-request
job must not reference a custom secret. The Claude review workflows retain their
OAuth token only behind their explicit collaborator authorization gates, and do
so on GitHub-hosted runners. No pull-request-reachable job may hold a
repository-scoped write permission. `id-token: write` is the single accepted
write scope, because it grants no repository access: it only lets the job mint an
OIDC JWT for its own identity, which `anthropics/claude-code-action` exchanges
for a short-lived app token.

Persistent self-hosted jobs are limited to trusted `push`/`workflow_dispatch`
events and never check out a pull-request ref. Pages deploys on
`[self-hosted, Linux, X64, builder]`; the aggregate Local CI gate targets the
development builder with `[self-hosted, Linux, X64, docker, builder,
development]`. `pull_request_target` is not used anywhere in this repository's
workflows and must never be combined with a persistent runner.
Local CI executes the gate through `testing/ci/container-run.sh`, keeping the
GTK/VTE toolchain in a repository-owned image and build outputs inside the
disposable container rather than requiring desktop development packages on the
generic builder host.
Public CI runs the HTTP contract verifier in `testing/test_http_contract_docs.py` and the real-shell verifier in `testing/test_shell_integration.py`. Private runner-policy checks remain in the development repository.

## Pre-release checks

Run these from the repository root:

```bash
cargo fmt --manifest-path taarof-app/Cargo.toml --all --check
cargo fmt --manifest-path wasm-sidebar/Cargo.toml --all --check
cargo test --manifest-path taarof-app/Cargo.toml
cargo build --release --features bundled-sqlite --manifest-path taarof-app/Cargo.toml
bash packaging/linux/emit-artifact-provenance.sh taarof-app/target/release/taarof-app
(cd taarof-web && npm ci && npm run build)
bash packaging/linux/release-bundle.sh
cargo build --manifest-path wasm-sidebar/Cargo.toml --target wasm32-wasip1
bash packaging/linux/validate-local-install.sh
```

The `validate-local-install.sh` script builds the release binary, installs it
into a temporary prefix with the checked-in desktop assets plus the built web
bundle, and verifies the launcher, icon, AppStream metadata, and installed
frontend files under `share/taarof/web`.

## Exact-head release rehearsal

Before creating a release tag, dispatch the `Release` workflow from the
release-candidate branch and supply its immutable full commit SHA as the
required `commit` input. The input must be a lowercase 40-hex SHA and, before
any build work, the workflow requires all three identities to be equal:
`commit` input, `GITHUB_SHA`, and checked-out `HEAD`.

```bash
candidate_sha="$(git rev-parse HEAD)"
gh workflow run Release --ref "$(git branch --show-current)" \
  -f commit="$candidate_sha"
```

This is a non-publishing rehearsal: it builds the web bundle and release
binary, creates and verifies the Linux tarball and checksum, attests and stages
provenance, then uploads the
`taarof-linux-x86_64` workflow artifact. It has no `contents: write` permission
and cannot create a tag or GitHub Release. Record the exact `candidate_sha`,
workflow-run URL and conclusion, and the artifact metadata (name, size, and
expiry) on the candidate PR or release sign-off. Do not download the artifact
merely to collect that metadata.

The tarball, checksum, and
`taarof-linux-x86_64.tar.gz.intoto.jsonl` provenance bundle are a single
required asset set. An unavailable attestation fails before artifact upload, and
a tag release cannot publish without all three files. GitHub-native attestation
for a private repository requires GitHub Enterprise and is currently unavailable
for this organization, so the rehearsal is expected to stop at attestation
until a human makes the privacy/platform decision. Do not substitute SLSA,
Sigstore, or a public Rekor log: that could publish private repository identity.
Changing repository visibility, upgrading to the eligible GitHub plan, or
selecting another provenance service is outside this rehearsal and requires
explicit human approval.

Only a `push` of a `v*` tag in `kombiz/taarof`
unlocks the separate publication job. That job downloads the same named
artifact, re-verifies its checksum, and publishes its tarball, checksum, and the
required provenance bundle. Creating or pushing that tag remains a human
approval step.

### Latest exact-head rehearsal attempt

On August 22, 2026, the non-publishing workflow was dispatched for exact commit
`2d3ff6679073148411bed6448bebc42d50f1879a` in
[run 32603958112](https://github.com/Example-Org/Example-Repo/actions/runs/32603958112).
GitHub rejected the `linux-tarball` job before any workflow step started because
the account's Actions billing or spending limit needs attention. The
`publish-github-release` job was skipped, no workflow artifact was created, and
the repository still had no tags or GitHub Releases after the attempt.

This is exact-head failure evidence, not a successful rehearsal. EXAMPLE-163
remains open until Actions can schedule the job and the required tarball,
checksum, and provenance artifact can be inspected. Re-dispatch the same
candidate SHA only while it remains the intended candidate; otherwise dispatch
the new immutable candidate head and replace this evidence.

## Release-readiness sign-off

Use this section to record what was validated for the release candidate. The
checks above prove the repo is buildable; this sign-off is the manual/runtime
layer that confirms the desktop app behaves correctly when launched.

Use [`docs/release-test-checklist.md`](release-test-checklist.md) as the
working checklist for manual release validation.

## Supported systems for this release

The shipped Linux tarball builds `taarof-app` with `bundled-sqlite`, so
OpenCode history does not require an external SQLite runtime package. Source
and distro builds use the system SQLite library by default.

Expected supported targets today:

- Linux only
- GTK4 desktop environments on Wayland or X11
- Distros where the required GTK4/libadwaita/VTE packages are readily
  available, with the README documenting:
  - Arch / Manjaro
  - Fedora 39+
  - Ubuntu 26.04+

Not currently expected to ship as supported targets:

- macOS
- Windows

### Historical manual validation (expired for release sign-off)

The April 9, 2026 validation below is useful coverage history, but it is not
exact-head evidence for a current release candidate. Do not use it as release
sign-off; complete and record the exact-head rehearsal above, then perform the
current manual validation needed for the candidate.

Most recent manual end-to-end pass: April 9, 2026 (local Linux desktop session,
isolated runtime/config/data dirs, GTK4 Broadway frontend, local socket API,
local HTTP API, and a temporary local tmux server).

Validated in that pass:

- App launch under GTK4/libadwaita with a live window surface exposed through
  Broadway
- Sidebar `+ New tab` UI flow creating and selecting a new tab
- Socket control flow for `rename-tab`, `open-pane`, `run-in-pane`,
  `send-keys`, `get-text`, `switch-tab`, `close-pane`, and `close-tab`
- Local tmux-backed tab creation through `create-tmux-tab`
- Dashboard creation through `open-dashboard`
- HTTP query API auth and query surfaces:
  - unauthenticated `/api/v1/state` rejected with `401`
  - `/health` returned `ok`
  - authenticated `/api/v1/state`, `/api/v1/sessions`,
    `/api/v1/workspaces`, `/api/v1/tabs`, `/api/v1/panes`, and
    `/api/v1/events` returned valid payloads
  - `/api/v1/events/ws` streamed live broadcast events with token auth
  - `/api/v1/sessions` covered both named and unnamed/default
    `session_name` payloads
- Snapshot/event consistency across UI, socket, and HTTP surfaces

Not explicitly validated in that pass:

- Remote host and SSH-backed flows
- Detached session attach/detach flows
- Agent workspace automation
- Saved view and workspace template UI flows through the palette/dialogs
- Leader-mode shortcuts and the full keyboard shortcut matrix
- Multi-workspace project/worktree workflows
- Packaging install on a fresh machine beyond `validate-local-install.sh`

Artifacts from that validation run were captured under `output/playwright/`.

Additional automated coverage landed on April 12, 2026:

- Persisted saved views now round-trip through config storage and appear in the
  read-only `query-state` snapshot with their effective limits.
- Persisted tab/workspace templates now round-trip through config storage and
  appear in the read-only `query-state` snapshot with lightweight summaries.
- Runtime smoke coverage exercises the socket query path with seeded saved
  views/templates so headless release validation can verify persistence without
  relying on Broadway UI automation.
- Runtime smoke coverage now exercises the headless socket detach/attach
  round-trip, proving that `detach-pane`, `list-detached`, `query-state`, and
  `attach-session` agree on detached-session state without relying on a live
  tmux server or a realized GTK window.
- HTTP route coverage now exercises the tmux pane-attach websocket against a
  live Axum server, proving that idle panes keep their initial ANSI snapshot on
  the first poll and that same-`pane_id` reattach flows retarget the existing
  socket when the backing tmux session/host changes.
- Runtime smoke coverage now seeds remote SSH/tmux pane metadata and verifies
  that `query-state` serializes `cwd_host`, `remote_shell`, and `tmux_host`
  using the documented field names.
- Host parser coverage now locks the remote probe payload contract, including
  explicit tmux session counts, `nproc`-based CPU percentages, and malformed
  payload rejection.
- Runtime smoke coverage now round-trips a multi-workspace session through disk
  state, proving that active workspace selection, per-workspace active tabs,
  worktree metadata, detached sessions, and the background section state
  survive restore.
- Socket unit coverage now includes a headless `agent-workspace` harness that
  exercises repo resolution, worktree creation, workspace metadata, reuse of
  the existing worktree workspace, `query-state` visibility, and non-destructive
  cleanup in `cargo test`.
- Keybinding unit coverage now freezes the default release shortcut matrix,
  duplicate-trigger invariants, and terminal-scoped pane action bindings so
  accidental contract drift fails fast in `cargo test`.
- `testing/test-agent-workspace.sh` now exercises the live `agent-workspace`
  socket flow against a running taarof instance, including worktree creation,
  workspace visibility in `query-state`, startup command execution, reuse of
  the existing workspace, and non-destructive cleanup when the tab is closed.

### Resolved cleanup

- Clickable URL regexes are now compiled with the VTE-required multiline flag,
  which removes the `vte_terminal_match_add_regex(...)` runtime warning and
  keeps plain `http(s)` URL matching registered for desktop terminals.

### Release recommendation

The current build is a reasonable release candidate if the intended scope is:

- Local Linux desktop usage
- Core terminal workspace flows
- Local tmux integration
- Local socket and HTTP query/control surfaces

Do not describe the release as fully validated across the entire feature set
until the unvalidated areas above have been exercised.

## Release steps

1. Merge the release-ready pull request after the `CI` workflow is green.
2. Run the pre-release checks locally on a Linux machine.
3. Complete a manual release-readiness sign-off and update the section above
   with the current validation date, scope, and known issues.
4. Update the version in `taarof-app/Cargo.toml` and `wasm-sidebar/Cargo.toml` if
   the release changes published versions.
5. Create an annotated tag, for example `git tag -a v0.1.0 -m "taarof v0.1.0"`.
6. Push the tag with `git push origin v0.1.0`.
7. Confirm the `Release` workflow creates the GitHub Release and uploads:
  - `taarof-linux-x86_64.tar.gz`
  - `taarof-linux-x86_64.tar.gz.sha256`
  - `taarof-linux-x86_64.tar.gz.intoto.jsonl`
8. Draft or refine the generated GitHub release notes from the merged changes.

## GitHub release tarball policy

- `.github/workflows/release.yml` permits non-publishing exact-commit rehearsals
  through `workflow_dispatch` in `kombiz/taarof`.
  Only a `v*` tag push in one of those approved repositories unlocks the
  publication job.
- The workflow builds the `taarof-app` release binary, re-emits its exact
  artifact-provenance sidecar, builds the `taarof-web` bundle, and then runs
  `packaging/linux/release-bundle.sh`.
- The release asset is `taarof-linux-x86_64.tar.gz`; its checksum is published
  as `taarof-linux-x86_64.tar.gz.sha256`.
- The tarball extracts to `taarof-linux-x86_64/` and includes:
  - `bin/taarof-app`
  - `bin/taarof`
  - `share/taarof/web/`
  - desktop, metainfo, and icon assets
  - `install.sh`
  - repository license files
- Running `./install.sh` from the extracted tarball installs the binary, CLI,
  desktop launcher, icon, metainfo, and packaged web bundle under `~/.local` by
  default. Pass a prefix path as the first argument to install elsewhere.
- `packaging/linux/install-local.sh` remains the source-checkout installation
  path for local development.
- Runtime web-asset lookup prefers `TAAROF_WEB_DIST_DIR` when it points to a
  valid build. Installed binaries then prefer the packaged bundle under
  `share/taarof/web`, followed by the user data bundle and the repo-local
  `taarof-web/dist` path as a last-resort developer fallback. Source-checkout runs
  still prefer the repo-local `taarof-web/dist` bundle first.
- `wasm-sidebar` is validated as a buildable artifact in CI, but release
  packaging for it remains manual.

## Standalone launcher proof receipt

Run the automated installed-command smoke against an explicit installation and
its extracted release bundle. The harness never installs software, starts a real
provider, contacts an operator socket, or stops/restarts Taarof. Its launcher
commands use a fresh HOME, configuration root and runtime directory with only
synthetic histories and adapters.

```sh
bash testing/e2e-gui/run-agent-launcher-e2e.sh \
  --prefix /absolute/installed/prefix \
  --package-dir /absolute/extracted/taarof-linux-x86_64 \
  --package-archive /absolute/taarof-linux-x86_64.tar.gz \
  --source-sha FULL_REVIEWED_COMMIT_SHA \
  --receipt /absolute/new-agent-release-receipt.json
```

`--package-archive` additionally checks the archive's command bytes and records
its SHA-256. Without it the receipt identifies the extracted bundle by its
complete manifest hash. A complete `taarof.bundle.v1` manifest must bind agent,
Taarof CLI and app hashes to the requested clean source, app build ID and agent
build ID. The harness checks package/install equality and the installed agent's
own `--build-info`; a missing or mismatched binding fails rather than inheriting
the app's source claim. Existing receipt files are never overwritten.

Optionally add `--runtime-pid PID` to observe an explicitly selected own-user
`taarof-app` process. The harness checks executable hash and PID lifetime; it
reads no process arguments, environment, HTTP tokens or runtime socket state.
A different running hash is recorded as different, and no runtime PID means
`not_observed`. File parity never establishes attended application behavior.

The automated checks cover hostile metadata, quoted cwd/session IDs, private
fixture exclusion, external timeout isolation, manifest disabling and local
new/resume with an isolated empty Taarof runtime root. This last check proves
independence from Taarof; it does **not** claim the operator's desktop was stopped.
Real provider new/resume, live tmux attach, remote degradation, operator
stop/restart, independent exact-head review and external gate receipts remain
separate requirements. Even a passing smoke emits
`release_status: pending_attended_proof`.

For a graphical/container release rehearsal, prepare the candidate using the
supported `testing/kasm/container-run.sh` or
`testing/e2e-gui/container-run.sh` and packaging install scripts. Run this harness
inside that same prepared environment against the installed prefix. Never use
`docker compose run` to build the candidate.
