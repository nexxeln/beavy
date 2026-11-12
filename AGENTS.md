# AGENTS.md

Guidance for agents working in this repository.

## Build, test, lint

The toolchain is pinned in `rust-toolchain.toml`; rustup installs it on first use.

```sh
cargo build                                               # builds target/debug/beavy
cargo test --workspace --locked                           # unit and integration tests
cargo fmt --all --check                                   # formatting
cargo clippy --workspace --all-targets --locked -- -D warnings
```

CI runs the last three on Linux and macOS (`.github/workflows/ci.yml`), and all three
must pass before a pull request is ready. macOS code compiles on Linux with
`rustup target add aarch64-apple-darwin` and
`cargo clippy --workspace --all-targets --target aarch64-apple-darwin -- -D warnings`,
but its tests only run in CI.

## Layout

```
crates/beavy/        library: every capability, plus the output contract
  src/outcome.rs     Outcome and its exit codes
  src/error.rs       Error, the reason registry, Next commands
  src/envelope.rs    versioned JSON documents and warnings
  src/output.rs      mode resolution (TTY, JSON, color) and the human renderer
  src/host/          host check: which worktree backends this machine supports
crates/beavy-cli/    the `beavy` binary: argument parsing and rendering only
  tests/cli.rs       runs the binary and asserts stdout, stderr, and exit code
docs/cli-contract.md the CLI contract: streams, exit codes, JSON shapes, reasons
```

Every capability goes in the library so a sandbox provider or agent platform can
link it instead of shelling out. The CLI stays thin: parse, call the library, render.

## CLI rules

beavy is agent first and follows the [Vercel CLI Design Guidelines](https://vercel.com/design/cli).
Read that page before adding or changing a command, option, output, or error, and
keep [docs/cli-contract.md](docs/cli-contract.md) in step with the code.

- stdout holds only the requested result; everything else goes to stderr.
- `--json` is an exact, documented contract on every command, with
  `schemaVersion`, and it turns prompts off. A JSON failure writes one single-line
  error document to stderr and nothing to stdout.
- Exit codes report the real outcome: 0 done, 1 failed, 2 usage or missing input.
- Complete input behaves the same with or without a TTY; missing input fails fast
  when non-interactive.
- Help works with no config, no daemon, and no network.
- An error says what failed and gives one exact next command when there is one.
- Commands read the resolved `output::Mode` and write through `output::Output`.
  Never check terminals or mode variables, and never print, anywhere else; clippy
  denies `print!` and `eprint!`.
- Report `unknown` rather than guess. Probes clean up after themselves.
- New reasons, warnings, and JSON fields go into docs/cli-contract.md in the same change.

## Commits

Commits are atomic and use conventional commit style, short and lowercase:
`feat(cli): add host check`, `docs: add cli contract`. No capitals, no trailing
period, about 50 characters at most. Pull request titles use the same style.
