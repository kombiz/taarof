# Contributing to taarof

Thanks for taking the time to improve `taarof`. This is an early public release,
so the contribution process is intentionally lightweight: small, focused issues
and pull requests are the easiest to review.

## Product and experiment

The desktop product is `taarof-app/` with its `taarof-web/` browser client.
The installed `taarof` command is the Python client in `taarof-cli/taarof`.
`examples/taarof` is a separate experimental Zellij/SSH launcher with the
same filename. It uses the [experimental WASM sidebar](wasm-sidebar/README.md),
not GTK. Neither experiment is installed by the desktop packaging scripts.
Keep their identities distinct when reporting bugs or changing installation docs.

## Development setup

`taarof` is a Linux desktop app built with Rust, GTK4/libadwaita/VTE, and a
Vite-based web client.

Install Bash, Zsh and Fish for the real-shell integration checks.
Install the system packages for your distribution, then install the Rust and
Node toolchains. If you use `mise`, run:

```bash
mise install
```

Useful local commands:

```bash
mise run build
mise run test
mise run lint
(cd taarof-web && npm ci && npm run build)
cargo fmt --manifest-path taarof-app/Cargo.toml --all --check
cargo fmt --manifest-path wasm-sidebar/Cargo.toml --all --check
cargo build --manifest-path wasm-sidebar/Cargo.toml --target wasm32-wasip1
bash packaging/linux/validate-local-install.sh
```

To run the desktop app during development:

```bash
mise run dev
```

## Reporting bugs

Use GitHub Issues:

https://github.com/kombiz/taarof/issues

Please include:

- what you tried
- what happened
- what you expected
- your Linux distribution and desktop environment
- `taarof --version` output, if available
- whether the optional HTTP API or browser client was involved

For security-sensitive reports, use `SECURITY.md` instead of a public issue.

## Pull requests

- Create task branches from `origin/kmux` in dedicated worktrees and target pull
  requests to `kmux`.
- Promotion from `kmux` to `main` requires review and explicit authorization;
  `kmux` is the GitHub default branch and `main` carries only promoted work.
- Keep PRs focused on one behavior change, bug fix, or documentation topic.
- Open an issue first for large design changes or changes to the local trust
  model.
- Run the relevant local checks before requesting review.
- Include screenshots or terminal output when changing visible behavior.
- Do not commit secrets, bearer tokens, local runtime files, or generated logs.

## Code of conduct

All project spaces use the expectations in `CODE_OF_CONDUCT.md`.
