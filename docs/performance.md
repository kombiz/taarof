# Performance measurement

Taarof has two complementary performance measurement paths. The deterministic
release harness is safe for headless contributor machines and CI. GTK/VTE
interaction measurements require a real desktop session and remain separate
from the source correctness gate.

## Deterministic release baseline

Run:

```bash
mise run performance
```

The task builds `performance_harness` in release mode, warms each workload once,
then records five measured repetitions at 200 iterations per repetition. It
writes `taarof-app/target/performance-baseline.json` by default. Set
`PERFORMANCE_BASELINE_OUTPUT` to keep a named artifact elsewhere. Pull-request
CI uploads the record as `performance-baseline-<source SHA>` for 30 days. The
first clean workstation record is checked in at
[performance-baselines/70b14c4-workstation.md](performance-baselines/70b14c4-workstation.md).

The JSON record includes the source SHA and dirty-tree status, Rust toolchain,
OS/kernel/CPU details, fixture identifiers, iteration settings, warmups,
repetitions, elapsed times, semantic checksums, source reads, and semantic work
units. Only a record with `source_dirty: false` is publishable baseline evidence.
The workloads are:

| Workload | Fixture | Work per iteration |
| --- | --- | ---: |
| `runtime-probe` | 8 tabs x 4 panes x 12 processes | 384 process records |
| `session-restore` | 8 workspaces x 16 tabs x 4 panes | 648 saved nodes |

The recorder fails immediately if a fixture identifier, checksum, source-read
count, or work-unit count changes. Timing values are observational. There is no
elapsed-time threshold because a stable distribution has not yet been measured
on the CI runner.

To update the contract, submit the fixture/code change and the expected values
in the same pull request. The review must explain the semantic reason for every
checksum or work-count change. A timing threshold may be added only from a run
series on the same runner class that demonstrates its normal variance and gives
the proposed limit explicit headroom. Never copy a threshold from a developer
workstation to CI.

For an optimization, capture the baseline and candidate with identical source
fixtures, iterations, warmups, repetitions, build profile, toolchain, and host.
Keep both JSON files and compare every sample and summary; do not compare only
the fastest run. A source change that alters the fixture is not a before/after
performance comparison.

## Application runtime baseline

The current synthetic harness does not measure input-to-paint latency, GTK
main-thread callback duration, idle app CPU/RSS, web request counts, or remote
disconnect behavior. Physical input-to-paint and GTK presentation tracing
require a real GTK/VTE session. CPU/RSS, loopback request counts, and remote
disconnect behavior can be automated headlessly once CI has an isolated app,
HTTP, and disposable remote fixture; that capture is not implemented yet.

Until then, capture all six metrics on a separate named development session;
do not reuse a production or pinned acceptance instance.

Record the source SHA, installed binary identity, host/display details, session
name and config path, monitor refresh rate, warmup actions, repetition count,
and the exact fixture before measuring:

1. Start the isolated session with an unused socket and HTTP port, and open the
   fixed local shell/tmux fixture. Do not launch an authenticated provider.
2. Capture at least 20 identical keystrokes with a frame/presentation tracing
   tool and report median and worst input-to-paint latency.
3. Capture GTK main-loop callbacks for the same actions and report every
   callback over 16.7 ms, including its maximum duration.
4. Leave the fixture untouched for five minutes and report per-process CPU and
   RSS at one-second intervals, including median and maximum.
5. Count loopback HTTP requests by route while repeating the same UI actions.
   Record counts only; never record bearer tokens or request headers.
6. Disconnect the fixed remote fixture once, record time to visible disconnected
   state and recovery behavior, then restore the fixture.

Attach the raw trace or counter output and a table with these fields:

| Metric | Before | After | Capture status |
| --- | ---: | ---: | --- |
| Input-to-paint median / worst | — | — | Requires real GTK/VTE session |
| GTK callbacks over 16.7 ms / maximum | — | — | Requires GTK tracing |
| Idle CPU median / maximum | — | — | Current harness does not start an isolated app |
| Idle RSS median / maximum | — | — | Current harness does not start an isolated app |
| Web requests by route | — | — | Current harness has no HTTP fixture |
| Remote disconnect detection / recovery | — | — | Current harness has no disposable remote fixture |

An unavailable interactive metric stays explicitly unavailable. It does not
turn a headless checksum/work-count regression into a pass, and it does not
block unrelated correctness work.
