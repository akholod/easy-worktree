# ewtm

Easy Worktrees Manager is a Rust 1.88 command-line/TUI foundation.

## M0 status

M0 provides the `ewtm` binary, a deliberately small CLI/TUI skeleton, domain and
infrastructure boundaries, Git fixture coverage, configuration contract, and CI.
M1a adds Git discovery plus deterministic list/status output and Unicode branch
slug support. M1b adds layered typed configuration, safe `.worktreerc` import,
configuration editing, and doctor checks.

`.worktreerc` import is a non-executing assignment subset. It accepts quoted
literals, safe legacy path-array words, strict decimal flags, and Bash-style
`(...)` arrays; it never invokes a shell. Imported rules and tasks are disabled
for explicit review, and diagnostics retain source locations.

Configuration defaults `create.slug_max_bytes` to 60 (minimum 8). CLI overrides
are available on config show/validate with `--slug-max-bytes`,
`--worktree-root`, and `--directory-prefix`.

## Usage

```text
cargo run -- --help
cargo run -- --version
cargo run                  # starts the TUI
cargo run -- tui            # starts the TUI
cargo run -- list            # list discovered worktrees
cargo run -- list --json     # stable machine-readable envelope
cargo run -- list --path .   # discover from a supplied path
cargo run -- config show --json
cargo run -- config validate
cargo run -- config import --file .worktreerc --json
cargo run -- doctor --json
```

Press `q` or `Esc` to leave the TUI.

JSON list output has `schema_version = 1`, `command = "list"`, `ok`, `data`,
`warnings`, and `error` fields. Paths are strings when valid UTF-8; Unix paths
that are not UTF-8 use a tagged `{ "kind": "bytes", "bytes": [...] }` value.

## Development

```text
cargo fmt --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
```
