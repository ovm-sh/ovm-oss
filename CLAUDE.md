# CLAUDE.md

Instructions for AI agents working on the OVM codebase.

## Project Overview

OVM (Open Version Manager) is a Rust CLI that manages versions of AI coding tools — currently **Claude Code**, **Codex**, and **Pi**. It allows developers to install, switch, and launch multiple versions side-by-side.

**Repository:** `ovm-sh/ovm-oss`
**License:** MIT
**Platform:** macOS and Linux. CI runs the test suite on `macos-latest` and `ubuntu-latest`; day-to-day developer validation is on macOS.

## Monorepo Layout

```
ovm/
├── crates/ovm/           # Rust CLI binary (core)
├── npm/                  # npm platform binary packages for distribution
├── tools/benchmark/      # ovm-benchmark plugin (Node.js)
├── docs/                 # Architecture, devlog, version registry (api/)
├── scripts/              # Build, release, registry-update scripts
├── tests/compatibility/  # Feature compatibility data (known-features.json)
└── .hooks/               # Git hooks (pre-commit, pre-push)
```

## Development Commands

```bash
# Rust core
cargo fmt                    # Format
cargo clippy -- -D warnings  # Lint
cargo test                   # Run all tests
cargo build --release        # Release build

# Node plugins
cd tools/benchmark && npm run typecheck

# Full pre-commit check (runs fmt + clippy + test + shellcheck/actionlint if installed)
.hooks/pre-commit
```

## Dev Workflow (local install)

For active development, install a standalone content-addressed snapshot:

```bash
./scripts/dev-install.sh     # build workspace + install dev-<content-hash>
ovm self current             # show the selected OVM snapshot
ovm <command>                # runs through the stable control plane
```

The installer copies the manifest-declared bundle under `~/.ovm/self/versions/`
and atomically switches `~/.ovm/self/current`. It never leaves installed commands
pointing into the checkout, so the repository can be moved safely. Rerun
`./scripts/dev-install.sh` after code changes; rebuilding alone does not refresh
the installed snapshot. `scripts/dev-uninstall.sh` only cleans links from the
retired checkout-symlink workflow and intentionally preserves standalone versions.

The pre-commit hook runs `cargo fmt --check`, `cargo clippy -D warnings`, and
`cargo test` on every commit — no extra setup needed beyond installing the hook
once via `./.hooks/install.sh`.

## Code Style

### Rust
- Edition 2021, stable toolchain
- `cargo fmt` and `cargo clippy` must pass with zero warnings
- Prefer explicit error types over `.unwrap()` in library code
- Use `thiserror` for error enums
- Tests live alongside source code in `#[cfg(test)] mod tests`
- Integration tests in `crates/ovm/tests/` — use `assert_cmd` for CLI invocation
- Test isolation via `tempfile::tempdir()` — never touch real `~/.ovm/`

### Commit Conventions
```
<type>: <subject>
```
Types: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `chore:`

Keep commits atomic. One concern per commit.

## Architecture

See `docs/architecture.md` for the full system design.

Key types:
- `Product` — enum of managed products (Claude, Codex, Pi)
- `VersionManager` — core logic for install/use/uninstall/list
- `OvmDirs` / `ProductDirs` — storage layout and path resolution
- `sources/` — download backends (GCS, npm, GitHub Releases, registry)

Plugin system:
- Any `ovm-*` binary on `$PATH` auto-discovers as a plugin
- See `crates/ovm/src/plugins.rs` — PATH scan + dispatch

## Security — Mandatory

- **NEVER** commit secrets, credentials, API keys, or tokens
- **NEVER** include real filesystem paths (e.g., absolute home directory paths) in code, docs, or commit messages
- **NEVER** include PII (real names, emails, usernames) in code or docs unless it's the project's own public metadata
- Use `~/.ovm/` not absolute paths in documentation
- Use placeholder paths like `/path/to/binary` in examples
- Review diffs for path leaks before committing

## Error Handling

- Throw meaningful errors with actionable messages
- Example: `"Claude Code 2.1.91 is not installed. Run: ovm install claude 2.1.91"`
- Clean up partial state on failure (remove incomplete downloads)
- On fatal errors, `abort()` prints the Mochi sad face before the error message

## Console Output

- Mochi the Cat mascot (see `crates/ovm/src/mochi.rs`) appears on help, success, and error
- `console` crate for styled terminal output
- Progress bars via `indicatif` for downloads
- Errors to stderr, data to stdout

## Testing

- Run `cargo test` after every change
- All tests must pass before committing
- Use `tempfile::tempdir()` for test isolation
- Never depend on real `~/.ovm/` state in tests
- Prefer dependency injection (pass paths explicitly) over mutating global env

## Custom / forked product builds

To run a patched Codex or Pi build through OVM, use the fork-build-import
flow: `docs/fork-build-import.md` (also available as the `fork-build-import`
Claude skill in `.claude/skills/`). Dev versions install as `dev:<label>` via
`ovm install <product> dev --dev <label> --binary|--bundle <path>`.

## Feature Workflow

1. Check `BACKLOG.md` for release roadmap items
2. Check `docs/features/backlog.md` for the feature queue
3. When shipping a feature, update `CHANGELOG.md`
4. Add a devlog entry in `docs/devlog/` for significant milestones

## What NOT to Do

- Do not create files unless absolutely necessary
- Do not add dependencies without justification
- Do not over-engineer — solve the current problem
- Do not add migration code for CVM (clean break)
- Do not reference the old CVM project in active code or user-facing docs
