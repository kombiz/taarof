# Workstation performance baseline: `70b14c4`

This record was captured from clean source commit
`70b14c417928e35bb7cddd2ee9513bdf51cae6cc` on 2026-09-24 at
15:30:11 UTC. The harness was built in release mode with Rust/Cargo 1.98.1 on
Linux 7.2.6-1-cachyos, x86_64, a 13th Gen Intel Core i7-13700F, and 24 logical
CPUs.

Each workload ran one warmup and five measured repetitions of 200 iterations.
There was no parallel build during the measurement window. Timings are
observational; this single workstation series does not define a CI threshold.

| Workload | Fixture | Checksum | Source reads | Work units |
| --- | --- | ---: | ---: | ---: |
| `runtime-probe` | `runtime-probe-v1-8x4x12` | 13212336913182444560 | 313800 | 76800 |
| `session-restore` | `session-restore-v1-8x16x4` | 14115511789149790672 | 0 | 129600 |

All checksum, source-read, and work-unit values were identical across the
warmup and every measured repetition.

| Workload | Warmup ns | Measured ns | Min ns | Median ns | Max ns | Range / median |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| `runtime-probe` | 90031077 | 92442847, 88181272, 88393771, 87425598, 88682187 | 87425598 | 88393771 | 92442847 | 5.68% |
| `session-restore` | 91758758 | 90897632, 95612356, 87734613, 85517952, 87870681 | 85517952 | 87870681 | 95612356 | 11.49% |

The current harness does not provide application runtime measurements for
input-to-paint latency, long GTK callbacks, idle CPU/RSS, loopback web request
counts, or remote-disconnection behavior. Those remain unmeasured in this
record; see the capture procedure in [Performance measurement](../performance.md).
