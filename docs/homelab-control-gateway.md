# Homelab control gateway topology

`taarof-control-gateway` is the shipped remote trust boundary for one running
Taarof desktop instance. It permits authenticated remote access without making
the desktop app's bearer token, HTTP port, or same-user Unix socket into network
interfaces.

## Supported topology

```text
remote client
  -> private HTTPS / authenticated edge
    -> private tunnel, when the edge is on another host
      -> owner-host loopback taarof-control-gateway
        -> owner-host loopback Taarof HTTP/control API
          -> shared SocketMessage control handler
            -> GTK state on the desktop main thread
```

The GTK runtime and gateway must run as the owner user on the **same graphical
host**. The runtime registry, process-scoped bearer token, owner socket, and
authenticated runtime HTTP endpoint are local resources. The gateway is pinned
to one explicit runtime ID and session name and refuses any different runtime.

An optional always-on server may provide TLS/authentication and keep a private
reverse tunnel to the owner host. That server forwards to the **gateway**, never
directly to the runtime API. It does not host terminal sessions: when the owner
desktop is off, suspended, or Taarof is stopped, remote terminal access is
unavailable even if the edge remains healthy.

There is no supported always-on headless/server runtime. Designing a headless
runtime, durable terminal ownership, identity lifecycle, and recovery is a
separate architecture decision and is outside this topology.

## Trust boundaries

### Desktop runtime

- The Unix socket is same-user privileged control.
- The optional HTTP API remains on loopback and requires its process-scoped
  bearer token.
- HTTP write routes additionally require `[http_control].enabled = true` and a
  loopback bind.
- HTTP control requests use the same typed socket-message handler as local
  control; the gateway is not a second desktop-control implementation.
- GTK state stays on the main thread.

### Control gateway

- The gateway listener accepts loopback addresses only.
- It holds the runtime token server-side and never returns it to a client.
- It performs device pairing and attestation, issues scoped observe/control
  grants, rate-limits requests, and records metadata-only audit events.
- Its owner-administration Unix socket is private to the owner user.
- Its SQLite device store survives gateway restarts; live grants do not.
- It verifies the configured runtime identity before opening the database or
  either listener.

### HTTPS/auth edge

- The edge terminates private HTTPS and may add an independent identity layer.
- It forwards only to the gateway's loopback listener through a local proxy or
  authenticated private tunnel.
- A remote edge must not receive the runtime bearer token or access the runtime
  Unix socket.
- Network reachability is defense in depth; gateway device authentication and
  grants remain mandatory.

## Deployment shapes

### Private HTTPS on the owner host

The packaged Caddy template binds HTTPS to an explicitly selected Tailscale
address and proxies only to `127.0.0.1:8710`. Both the gateway and the runtime
stay on loopback. This is the simplest topology when the owner host itself is
reachable on a private network.

### Always-on remote edge

Keep the gateway on the owner host and create a private authenticated reverse
tunnel from the owner host to a loopback port on the always-on edge. Configure
the edge proxy to use that loopback tunnel endpoint. For example, if the
gateway listens on `127.0.0.1:8710`:

```bash
ssh -N -R 127.0.0.1:18710:127.0.0.1:8710 edge.example
```

The edge then proxies its authenticated HTTPS route to `127.0.0.1:18710`.
Keep the reverse-tunnel port loopback-only and restrict the SSH identity to the
single forwarding purpose. The committed package does not create this tunnel
or configure a remote edge automatically.

## Runtime rotation and availability

Every Taarof app restart rotates the runtime ID. Gateway startup authenticates
to the configured loopback runtime and compares both the observed runtime ID
and session name to the operator-selected pin.

A mismatch is an operator action, not a transient crash:

- the process prints `ACTION REQUIRED: stale runtime pin` and exits 78;
- the packaged user unit uses `RestartPreventExitStatus=78`;
- all other startup failures are bounded by `StartLimitBurst=3` within
  `StartLimitIntervalSec=60`;
- recovery requires selecting the intended runtime, editing only the explicit
  pin, running `--check-runtime`, resetting the failed unit, and restarting it.

Never auto-trust the newest registry entry. A newest process is not proof that
it is the intended session. The complete value-blind procedure is in
[remote-terminal-runbook.md](remote-terminal-runbook.md).

## Prohibited shortcuts

- Do not set `[http].unsafe_allow_non_loopback = true` as a remote-access path.
- Do not proxy the raw runtime HTTP port or Unix socket.
- Do not put the runtime bearer token in configuration, URLs, browser storage,
  issue comments, logs, or tunnel arguments.
- Do not move the gateway alone to an always-on server and claim the desktop
  runtime is available there.
- Do not add an unbounded service restart loop around stale-pin failures.

## Operator entry point

Use [remote-terminal-runbook.md](remote-terminal-runbook.md) to build, install,
configure, preflight, activate, verify, and recover the gateway. Installation
never enables the gateway or changes firewall, Caddy, Tailscale, or SSH state.
