# Remote terminal deployment runbook

Operator runbook for deploying a **tailnet-only private HTTPS** path to
`taarof-control-gateway` on the graphical owner host.

> **Scope and trust model.** The gateway is the *only* remote trust boundary.
> It binds loopback and does its own device pairing, hardware attestation,
> grants, rate limiting, and revocation. Caddy is transport only: it terminates
> private TLS on the Tailscale interface and reverse-proxies to the loopback
> gateway. Taarof's own HTTP port (`7800`) and its Unix socket **never** face the
> network. See `docs/homelab-control-gateway.md` for the trust boundaries and
> the optional always-on edge topology.

Every command below is run **by you, the operator, on the graphical owner
host** unless a step says otherwise. Nothing here is activated automatically;
the packaging step installs artifacts but never enables or starts services.

---

## 0. Prerequisites

- The owner host is on the tailnet and healthy: `tailscale status` shows it up.
- The desktop `taarof-app` runs on that host as your user (not root).
- You can reach the owner host from a second **tailnet** device (phone/laptop)
  and, for the negative tests, from a **non-tailnet / LAN-only** vantage point.
- `caddy` and `systemd --user` are available on the owner host.

Record these values once; you will substitute them throughout:

| Placeholder | How to obtain it | Example |
| --- | --- | --- |
| `TAILNET_HOST` | `tailscale status --json \| jq -r .Self.DNSName` (strip trailing dot) | `desktop.tailXXXX.ts.net` |
| `TAILNET_ADDR` | `tailscale ip -4` | `100.64.0.1` |
| `LAN_ADDR` | `ip -4 addr show \| grep -v 127.0.0.1` | `192.168.1.50` |
| `UID` | `id -u` | `1000` |

---

## 1. Build and install the artifacts

The gateway binary and the deployment artifacts install under your `~/.local`
prefix. Installation is **not** activation.

```bash
cd /path/to/taarof   # your source checkout

# Build the gateway (bundled SQLite keeps it host-independent).
cargo build --release --manifest-path taarof-control-gateway/Cargo.toml

# Build the desktop app + web bundle if not already built (install-local.sh
# needs them present).
cargo build --release --manifest-path taarof-app/Cargo.toml
(cd taarof-web && npm ci && npm run build)

# Install everything, including the gateway binary, the systemd user unit, and
# the Caddy site template. Idempotent; safe to re-run.
bash packaging/linux/install-local.sh
```

This places:

| Artifact | Path |
| --- | --- |
| Gateway binary | `~/.local/bin/taarof-control-gateway` |
| systemd user unit | `~/.local/share/systemd/user/taarof-control-gateway.service` |
| Caddy site template | `~/.local/share/taarof/caddy/taarof-control.caddy` |
| Gateway license | `~/.local/share/licenses/taarof-control-gateway/LICENSE` |

`~/.local/share/systemd/user/` is on the `systemd --user` unit search path, so
the unit is enable-able by name after `systemctl --user daemon-reload`.

---

## 2. Enable taarof's loopback control API

The gateway is a loopback client of taarof's HTTP/WebSocket control API. Keep
that API loopback-only; enable the control gate. Edit
`~/.config/taarof/config.toml`:

```toml
[http]
enabled = true
port = 7800
bind_address = "127.0.0.1"
unsafe_allow_non_loopback = false   # MUST stay false

[http_control]
enabled = true                      # narrow write surface, loopback-only
```

Restart `taarof-app` so the change takes effect. Confirm it is loopback-only:

```bash
pid=$(jq -r .pid "$XDG_RUNTIME_DIR/taarof-current.json")
curl -fsS http://127.0.0.1:7800/health          # -> ok
ss -ltnp | grep ':7800'                          # -> bound to 127.0.0.1 ONLY
```

If `ss` shows `0.0.0.0:7800` or `*:7800`, stop: `unsafe_allow_non_loopback` is
set. Do not proceed until port 7800 is loopback-only.

---

## 3. Configure the gateway

Create `~/.config/taarof/gateway.toml`. The gateway parses strictly (unknown
fields are hard errors) and fails closed at startup on anything missing.

```toml
[gateway]
# Loopback only. The gateway refuses to start on any non-loopback address.
bind_address = "127.0.0.1:8710"

[runtime]
# Pins the gateway to exactly one taarof runtime. It refuses to stream a byte
# from any other runtime identity (fail-closed).
session_name = "default"                         # your TAAROF_SESSION, or "default" if unset
instance_id  = "REPLACE_WITH_CURRENT_RUNTIME_ID" # see the note below
registry_path = "/run/user/UID/taarof-current.json"   # substitute your UID
http_address = "127.0.0.1:7800"                    # actual runtime HTTP endpoint

[database]
# The device store. MUST live under the service's StateDirectory so paired
# devices survive restarts. Substitute your real home path.
path = "/home/USER/.local/state/taarof-control-gateway/devices.db"
```

### Runtime rotation: action required and explicit recovery

The runtime ID changes on each app restart or reboot. Startup authenticates to
`http_address` using the process-scoped token discovered through `registry_path`
and compares **both** the configured instance ID and session name. The request
has a five-second deadline and a 64 KiB response limit. A mismatch prints
`ACTION REQUIRED: stale runtime pin` and exits with status 78 (`CONFIG`);
`RestartPreventExitStatus=78` leaves the service failed without retrying.
Other startup failures, including an older local `ExecStartPre` pin script,
are limited to three starts per 60 seconds. systemd does not schedule an
automatic retry when that interval expires; operator recovery is required.

1. On the intended owner host, verify the intended app/session is running.
   Run `taarof runtime-identity --pretty` (or
   `taarof --session NAME runtime-identity --pretty` for a named session).
   The CLI reads authentication internally; never print or copy the bearer token.
2. Confirm the displayed runtime belongs to the intended session, then edit
   **only** `[runtime].instance_id` in `~/.config/taarof/gateway.toml` to that
   explicitly selected runtime ID. Do not automatically copy the latest registry
   or runtime identity. Keep the session, registry, HTTP address and device store
   pinned to their existing intended targets.
3. Validate the edited pin without starting listeners or touching the database:

   ```bash
   TAAROF_GATEWAY_CONFIG="$HOME/.config/taarof/gateway.toml" \
     ~/.local/bin/taarof-control-gateway --check-runtime
   ```

4. Only after validation succeeds, clear the start limit and restart:

   ```bash
   systemctl --user daemon-reload
   systemctl --user reset-failed taarof-control-gateway
   systemctl --user restart taarof-control-gateway
   systemctl --user status taarof-control-gateway --no-pager
   journalctl --user -u taarof-control-gateway -n 20 --no-pager
   ```

Startup revalidates the pin, so another rotation between steps 3 and 4 fails
closed again. Existing paired devices remain in the same database. This check
proves the pinned runtime is reachable, not that every experimental gateway
protocol route is production-ready; retain the functional readiness gate below.

Named-session registry filenames include a bounded prefix and digest. Use
`taarof --session NAME` to resolve the session; do not construct filenames from
the raw name. Set the gateway registry path to that session's actual registry.

---

## 4. Install and start the systemd user service

```bash
systemctl --user daemon-reload
systemctl --user start taarof-control-gateway
systemctl --user status taarof-control-gateway --no-pager

# Confirm it bound loopback only:
ss -ltnp | grep ':8710'          # -> 127.0.0.1:8710 ONLY
curl -fsS http://127.0.0.1:8710/health   # -> ok
```

Enable it to start with your graphical session (optional; only after the
verification below passes):

```bash
systemctl --user enable taarof-control-gateway
# User lingering alone does not provide a desktop runtime or 24/7 terminals.
```

The unit is hardened (loopback-only address families, `ProtectSystem=strict`,
no capabilities, `MemoryDenyWriteExecute`, syscall-filtered, IP-allowlisted to
localhost). Inspect the exposure score if curious:

```bash
systemd-analyze --user security taarof-control-gateway
```

### Functional readiness gate

`/health` proves only that the scaffold process is alive. Before exposing or
pairing the gateway, prove that the executable mounted the versioned protocol
routes. An unauthenticated request may be rejected, but it must not be `404`:

```bash
code=$(curl -sS -o /dev/null -w '%{http_code}' http://127.0.0.1:8710/v1/runtime)
test "$code" != 404
```

If this returns `404`, stop. The running binary is health-only and does not yet
provide pairing, observe sessions, navigation, terminal relay, or control. Do
not treat Caddy health or the four network-isolation probes as functional
remote-control acceptance.

---

## 5. Install the private HTTPS edge (Caddy on the owner host)

This owner-host Caddy site binds only to the Tailscale address. The gateway
authenticates devices itself; Caddy is a transport boundary, not a replacement
for gateway authentication.

1. Copy the template into the owner host's Caddy config directory and fill the two
   placeholders (or export them as environment variables for Caddy):

   ```bash
   sudo install -Dm644 \
     ~/.local/share/taarof/caddy/taarof-control.caddy \
     /etc/caddy/sites/taarof-control.caddy
   sudo sed -i \
     -e "s|{\$TAAROF_TAILNET_HOST:[^}]*}|$TAILNET_HOST|" \
     -e "s|{\$TAAROF_TAILNET_ADDR:[^}]*}|$TAILNET_ADDR|" \
     /etc/caddy/sites/taarof-control.caddy
   ```

   Ensure the host's main `Caddyfile` imports it (once):
   `import /etc/caddy/sites/*.caddy`

2. Validate and reload:

   ```bash
   caddy validate --config /etc/caddy/Caddyfile
   sudo systemctl reload caddy
   ```

3. Confirm Caddy is listening **only** on the Tailscale address, never `0.0.0.0`:

   ```bash
   ss -ltnp | grep ':443'      # -> TAILNET_ADDR:443 ONLY (not 0.0.0.0, not LAN_ADDR)
   # Belt-and-suspenders: the site disables auto HTTP->HTTPS redirects, so there
   # should be NO :80 listener from this Caddy. If your Caddy version differs and
   # one appears, it must not be on 0.0.0.0/LAN_ADDR.
   ss -ltnp | grep ':80 '      # -> expect NO output (no :80 listener at all)
   ```

The site uses `tls internal`; the pairing QR pins the certificate's SHA-256 SPKI
so the mobile client trusts it by pin, not by a public CA. If you run a Caddy
build with the Tailscale module, you may switch to `get_certificate tailscale`;
the pin flow is unchanged.

---

## 6. Firewall checklist

Binding Caddy to `TAILNET_ADDR` already keeps the listener off other interfaces.
Add host-firewall rules as defense in depth so a future misconfiguration cannot
expose the port. Example with `nftables` (adapt to your ruleset):

```bash
# Allow 443 only in from the tailscale interface; drop it everywhere else.
sudo nft add rule inet filter input iifname "tailscale0" tcp dport 443 accept
sudo nft add rule inet filter input tcp dport 443 drop

# Belt-and-suspenders: the gateway (8710) and taarof (7800) are loopback-bound,
# but explicitly drop them on every non-loopback interface.
sudo nft add rule inet filter input iifname != "lo" tcp dport { 7800, 8710 } drop
```

Do not add raw nftables rules when the host ruleset is managed by UFW. Preserve
the existing tailnet policy and insert explicit service guards before any broad
`tailscale0` allow rule:

```bash
# First inspect and record the current ordering.
sudo ufw status numbered

# Keep the raw app and gateway ports unreachable on every external interface.
sudo ufw insert 1 deny in to any port 7800 proto tcp comment 'taarof loopback only'
sudo ufw insert 2 deny in to any port 8710 proto tcp comment 'taarof gateway loopback only'

# Permit the private edge on tailscale0, then deny 443 everywhere else. These
# rules must stay before a broader tailscale0 allow rule.
sudo ufw insert 3 allow in on tailscale0 to TAILNET_ADDR port 443 proto tcp comment 'taarof tailnet https'
sudo ufw insert 4 deny in to any port 443 proto tcp comment 'taarof tailnet only'
```

Re-run `sudo ufw status numbered` and all Section 7 probes after changes. If
the existing rules differ, derive and review the exact insertion order rather
than copying these positions blindly.

Checklist — confirm each:

- [ ] Caddy `:443` accepts only on `tailscale0` / `TAILNET_ADDR`.
- [ ] `7800` (taarof) and `8710` (gateway) are reachable on `lo` only.
- [ ] Tailscale ACLs restrict which tailnet devices may reach the owner host on `443`
      (tighten in the tailnet admin console; do not rely on network reachability
      alone — the gateway's device auth is the real gate).
- [ ] No public DNS record points at this private service.

---

## 7. Network-isolation verification (required before trusting the deploy)

Run all four probes. The deploy is only acceptable if the tailnet path succeeds
and **every** other path fails. Substitute your placeholders.

### 7a. Tailnet path — MUST succeed

From a **second tailnet device** (Tailscale up):

```bash
curl -sS --max-time 8 -k "https://TAILNET_HOST/health"     # -> ok
```

(`-k` because the cert is pinned, not publicly-CA-trusted; the real client
validates the SPKI pin.)

### 7b. LAN path — MUST fail

From a LAN-only host, or targeting the owner host's LAN address:

```bash
curl -sS --max-time 5 -k "https://LAN_ADDR/health"; echo "exit=$?"
# Expect: connection refused or timeout (non-zero exit). Caddy is not bound here.
```

### 7c. Non-tailnet path — MUST fail

From a host with **Tailscale stopped** (`tailscale down`), the MagicDNS name must
not resolve to a reachable route:

```bash
curl -sS --max-time 5 -k "https://TAILNET_HOST/health"; echo "exit=$?"
# Expect: DNS failure or timeout (non-zero exit). There is no public route.
```

### 7d. Taarof port 7800 and the Unix socket — MUST be unreachable remotely

From a **tailnet** device (the most privileged remote vantage), confirm the
underlying loopback services are not exposed:

```bash
# Direct taarof HTTP must not answer over the network:
curl -sS --max-time 5 "http://TAILNET_ADDR:7800/health"; echo "exit=$?"   # refused/timeout
curl -sS --max-time 5 "http://LAN_ADDR:7800/health";     echo "exit=$?"   # refused/timeout

# Gateway loopback port must not answer over the network either (Caddy is the
# only front door):
curl -sS --max-time 5 "http://TAILNET_ADDR:8710/health"; echo "exit=$?"   # refused/timeout

# Port scan from the tailnet device: 7800 and 8710 must show closed/filtered.
nc -z -w3 TAILNET_ADDR 7800 && echo "OPEN (FAIL)" || echo "closed (ok)"
nc -z -w3 TAILNET_ADDR 8710 && echo "OPEN (FAIL)" || echo "closed (ok)"
```

The Unix socket (`$XDG_RUNTIME_DIR/taarof-*.sock`) is not network-addressable by
construction; confirm no TCP forwarder or SSH `-R` tunnel exposes it:

```bash
ss -ltnp | grep -E ':(7800|8710)' # both must show 127.0.0.1 only
```

| Path | Required result |
| --- | --- |
| Tailnet -> Caddy `:443` | success (`ok`) |
| LAN -> `:443` | fail (refused/timeout) |
| Non-tailnet (Tailscale down) -> host | fail (no route) |
| Any remote -> `:7800` (taarof) | fail (refused/timeout) |
| Any remote -> `:8710` (gateway) | fail (refused/timeout) |
| Any remote -> Unix socket | not addressable |

If any "fail" row succeeds, **stop and roll back** (Section 10) before pairing a
device.

---

## 8. Pairing a device

1. On the desktop, trigger a pairing offer through taarof (five-minute, one-use).
   The gateway creates the offer via the authenticated same-user Unix-socket
   extension; taarof renders a QR containing the `TAILNET_HOST`, pairing id,
   expiry, protocol version, and the gateway certificate's SHA-256 SPKI pin. The
   QR never contains taarof's bearer token.
2. On the phone (Tailscale up), scan the QR. The app generates a non-exportable
   key, validates the hostname and pin, submits its public key, and proves
   possession.
3. The desktop shows the pending device name and key fingerprint. **Confirm only
   if they match the device in your hand**; reject otherwise.
4. The control key enrolls with hardware attestation. The gateway validates the
   attestation chain, app identity, challenge, and TEE/StrongBox level before
   allowing any control grant.

Verify the paired device count after restart:

```bash
journalctl --user -u taarof-control-gateway | grep "paired device"
```

---

## 9. Revocation

Revoke at the highest layer first; each step is independently sufficient for the
scope it covers.

1. **Revoke the device** from the owner host (desktop taarof's device-management
   surface, or the gateway's revocation API). This immediately rejects new
   control/observe sessions from that device and closes its live WebSockets.
2. **Confirm** the revocation took: the device can no longer obtain a session;
   active control streams for it are closed.
3. If you only want to drop *control* (keep observe), revoke the control grant
   only — observe survives and reconnects.

Restarting the gateway invalidates all live grants while preserving paired-device
records:

```bash
systemctl --user restart taarof-control-gateway
```

---

## 10. Emergency stop and rollback

Fastest to most complete. Any one of these severs remote access.

```bash
# 1. Owner-host emergency stop: disable taarof's control gate. The gateway then
#    fails closed (no control write path). Set in ~/.config/taarof/config.toml:
#       [http_control]
#       enabled = false
#    then restart taarof-app.

# 2. Stop the gateway entirely:
systemctl --user stop taarof-control-gateway
systemctl --user disable taarof-control-gateway

# 3. Remove the tailnet HTTPS edge:
sudo rm -f /etc/caddy/sites/taarof-control.caddy
sudo systemctl reload caddy

# 4. (Nuclear) drop the port at the firewall:
sudo nft add rule inet filter input tcp dport 443 drop
```

After an emergency stop, re-run the Section 7 probes and confirm the tailnet path
now **fails** too.

---

## Verification checklist (deploy sign-off)

- [ ] taarof `:7800` and gateway `:8710` are loopback-only (`ss -ltnp`).
- [ ] `unsafe_allow_non_loopback = false` in taarof config.
- [ ] Gateway service is hardened and started; `/health` returns `ok` on loopback.
- [ ] Caddy `:443` binds `TAILNET_ADDR` only, never `0.0.0.0` or `LAN_ADDR`.
- [ ] Section 7 probes: tailnet path succeeds; LAN, non-tailnet, `:7800`, `:8710`
      all fail remotely.
- [ ] Firewall drops `:443` off the tailscale interface and `:7800`/`:8710` off
      non-loopback.
- [ ] Device pairing requires explicit desktop confirmation + attestation.
- [ ] Revocation closes live sessions; emergency stop severs remote access.
- [ ] No public DNS/route points at this service.
