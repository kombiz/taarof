# Taarof control gateway

`taarof-control-gateway` is the optional, loopback-only remote trust boundary
for a Taarof desktop runtime. It pairs and authenticates devices, applies
scoped grants, relays terminal traffic, and calls only the desktop runtime's
authenticated local API.

This component was imported with repository-owner authorization from
`OldNorthRepository/kmux` at source snapshot
`b256de0b389544dc29e08569e63fb250ef7ebdc7`. That snapshot includes the
stale-runtime-pin recovery work and the earlier gateway implementation history.

The gateway is licensed under GPL-3.0-or-later, separately from the repository's
default MIT/Apache-2.0 terms. See [LICENSE](LICENSE).

Build and test it from the repository root:

```bash
cargo build --release --manifest-path taarof-control-gateway/Cargo.toml
cargo test --manifest-path taarof-control-gateway/Cargo.toml
```

See [`../docs/remote-terminal-runbook.md`](../docs/remote-terminal-runbook.md)
for configuration and deployment, including runtime-ID rotation recovery.
