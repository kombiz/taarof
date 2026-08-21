# Security Policy

## Supported versions

Security fixes are handled for the latest public release of `taarof`. During the
v0.1.x series, the public API and packaging flow may still change, so please
test against the newest released version before reporting a vulnerability.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability.

Use GitHub's private vulnerability reporting flow for this repository:

https://github.com/kombiz/taarof/security/advisories/new

Include:

- affected version or commit
- operating system and desktop environment
- whether the optional HTTP API was enabled
- a minimal reproduction, if you have one
- impact and any known workaround

Please redact bearer tokens, runtime paths that identify private machines, and
terminal output that includes secrets. Maintainers aim to acknowledge reports
within 72 hours.

## Local trust model

`taarof` is a local Linux desktop application. Its v0.1.x security boundaries
are:

- The Unix socket at `$XDG_RUNTIME_DIR/taarof-<pid>.sock` is a privileged
  same-user control surface. `taarof` verifies that the runtime directory is
  owned by the current user and has private permissions before binding.
- The optional HTTP API defaults to loopback-only binding (`127.0.0.1` / `::1`)
  and requires a per-session bearer token stored in
  `$XDG_RUNTIME_DIR/taarof-http-<pid>.token`.
- The browser client stores the bearer token in local browser storage and the
  WebSocket transport accepts the token in the URL query string. This is
  intentional for the local-loopback v0.1.x workflow.

## Remote access

Remote/share mode is not supported in v0.1.x.

Do not bind the HTTP API to a non-loopback interface unless you are fronting it
with a trusted private transport such as an SSH tunnel or Tailscale and you
understand that token-bearing URLs and browser-local token storage are part of
the current local-only design.

A dedicated remote/share design is planned for a later release.
