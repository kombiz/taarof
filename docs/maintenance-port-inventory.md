# BD-0490 maintenance port inventory

Status: inventory and public-safety review complete; implementation ready for CI
and independent exact-head review. Next: green PR CI, reviewed pinned merge,
green development-branch push CI, then authorized fast-forward promotion.

Compared source main `f20817e8e9b7ddf9549edc2fc9f0b8aa02123998` against public
`kmux`/`main` `7a0e124b8d0bc66efa75203345608afd17908811`. Source identifiers below
are inventory keys, not shared-history ancestry claims. Decisions were made
before copying product changes. Open proposals are assessed at their fetched
heads; dropping them here neither merges nor closes them at their source.

| Source item | Decision | Evidence / reason |
|---|---|---|
| e7f5c2a | drop | Protocol npm lockfile only; the public protocol contains OpenAPI/fixtures, no npm package or consumer. |
| a03705c | port | Emit app artifact provenance in `mise run build`; preserve channel installer behavior. |
| 455d9f5 | drop | Source-specific book/history observations; not product behavior or public runtime evidence. |
| ca4ec4c | already in taarof | CI/Release use `mirror.gcr.io/library/debian:trixie-slim`; socket uses `is_some_and(matches)`. `bc96a62` deliberately retains the Rust 1.98 atomic API. |
| ddc3553 | already in taarof | `testing/ci/Dockerfile` already uses `mirror.gcr.io/library/archlinux:latest`. |
| db79496 | drop | Source-only prerequisite ledger entry; no `.plan/tasks.json` exists in the public branch. |
| 69c83a1 | already in taarof | `Cargo.lock` pins Tokio 1.53.3 (`583a927`, public PR #88); source ledger excluded. |
| bf21dc0 | drop | Source-only prerequisite ledger design; no public task schema to replace. |
| eb68a73 | already in taarof | `taarof-web/package-lock.json` pins Vite 8.1.6 and source-map-js 1.2.2 (`583a927`); keep its separately resolved dependency tree. |
| 3ef6606 | drop | Source-only prerequisite closure records; their fixes are already public. |
| 2a06ad9 | already in taarof | CI already has workflow/PR-or-ref concurrency with `cancel-in-progress: true`; do not import source ledger. |
| 8639d27 | port | Both disposable install routes produce retained launcher receipts; add archive regressions and isolated native fallback fixtures. Exclude source-specific closure/proof docs and export scripts. |
| ba65107 | already in taarof | `package-linux` already uses standard `actions/checkout@v4`, without a PR-head ref override. |
| 14c5540 | port | Exact-head Release rehearsal receipts, SQLite build prerequisite, and complete-prompt synchronization. Preserve standard PR merge-ref checkout. |
| 3eb11ca | port | OSV informational-only unmaintained exceptions match `deny.toml`; no vulnerability exception. Exclude export/ledger records. |
| 6c79fc1 | port | Install `inetutils` for the Arch Fish regression environment. |
| 742e5c1 | drop | Source-to-public export rehearsal only; superseded by the next source commit and unnecessary in the destination. |
| 7d355b5 | drop | Source-to-public trusted export dispatch only; this destination has its own Release rehearsal. |
| PR #529 | drop | Unmerged account/profile and AI-synopsis design with failed app/contracts CI. It depends on the old discovery layer and would add model/history integration beyond a maintenance port. Keep the current provider-adapter contract; no claims that these features are already present. |
| PR #537 | drop | Draft gateway re-pin integration: the public workspace has no control-gateway crate, service, or remote-terminal runbook to maintain. Do not add that separate service as part of this port. |
| PR #541 | drop | Source book/history refresh only; host-specific observations are not public product evidence. |
| PR #543 | already in taarof | Its 16 task proposals are implemented by the public simplification queue: discovery/child/atomic/lifecycle/tmux/ledger/procfs/diagnostics/signals/SHA/path/socket/options/sidebar/PR refresh work (examples below). Do not import conflicting task IDs. |
| PR #544 | drop | Draft detached-notification redesign still has pending isolated desktop/activation evidence and explicitly recorded a reviewed-draft stopping point. Keep the existing detached notices and separately merged native completion notices (#86); do not claim the draft redesign is present. |
| PR #548 | drop | fast-uri update targets the same absent protocol npm package as e7f5c2a; copying its lockfile would create an unused dependency surface. |

## Tree-level reconciliation

The whole-tree comparison has 160 identical shared files, 149 different shared
files, 772 source-only files, and 41 public-only files. This is a content
comparison: the histories are independent. The source-only operator tooling,
private records, export machinery, obsolete experiments and deployment evidence
are not a public product import list. Public-only features and refactors remain.

To distinguish inherited differences from new source work, also diffed the last
export baseline `179ad3fe` against the fetched source main. All 23 changed paths
are covered above: three workflows, three source ledger files, two lockfiles,
source packaging-proof documentation, release documentation, mise tasks, OSV
policy, installer validation, protocol lockfile, two export scripts, history and
socket code, the Arch image, installed smoke and its regression suite, source
workflow contracts, and shell regressions. No additional post-export source
product change was hidden by the 18-commit inventory.

Public queue matches for PR #543 include `c78fe18` (discovery), `49f4dd6`
(child environment), `a5e97d6` (atomic ledger), #63 (lifecycle), `3062697`
(tmux runners), `0cdeb43` (probe batching), `1d1cbfc` (ledger projections),
`9533435` (process facts), #66 (diagnostics), `d33d3fb` (Unix signals),
`64bf874` (SHA-256), `fc32e3e` (path boundaries), `07a3778` (socket events),
`fc8162d` (HTTP options), `b43f0a1` (sidebar actions), and `0aad298` (PR refresh).

The public branch contains no `.plan/` directory. Its task state is recorded
here and in the PR, without introducing the source repository's ledger schema.

## Public-safety review

Reviewed changed content before copying it. Excluded private infrastructure,
source repository/org identifiers, host paths, operator reports, and source
proof receipts. The proposed product/test files contain no private hostnames,
private source links, credentials, or personal runtime data. Synthetic histories,
provider commands and receipt fixtures remain synthetic. Existing runner labels
in destination workflows are retained; this port adds no infrastructure names.
Only product-safe release instructions are carried over.

The source bookkeeping and export-only changes are dropped rather than rewritten.
The unmerged feature proposals above are explicitly dropped from this maintenance
port; they are not silently described as shipped. No owner-only publication
content remains in the selected port.

## Validation scope

Local tests were deferred by the workstation admission check (89.9% CPU).
Use the repository CI on the pushed commit. The full contributor command remains
`mise run ci`; its packaging stage exercises the installed launcher smoke for
both installer routes. PR CI checks a merge ref, so its receipt source is that
merge ref; push CI proves the merged development commit. File/fixture parity
does not establish attended desktop or real-provider acceptance. No local
installation, runtime restart, version tag or release publication is part of
this task.
