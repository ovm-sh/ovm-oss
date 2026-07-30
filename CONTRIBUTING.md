# Contributing to OVM

Thanks for your interest in contributing to OVM.

## How this project is maintained

OVM is maintainer-run: the code is open, day-to-day development is trunk-based
by the maintainer, and external pull requests are handled at maintainer
discretion (see `docs/dev-practice.md` for the full model). Concretely:

- **Issues are the best way to contribute.** Bug reports with reproduction
  steps are always welcome and get read.
- **Pull requests may be reviewed, reworked and landed as ordinary commits
  (with credit), or closed.** There is no review SLA and no obligation to
  merge. For anything non-trivial, open an issue first so effort isn't wasted.
- **No CI runs on external pull requests** until a maintainer has read the
  diff and explicitly approved a run. Workflows, runners, and secrets are
  never exposed to unreviewed code.

## Getting Started

```bash
git clone https://github.com/ovm-sh/ovm-oss.git
cd ovm
sh .hooks/install.sh   # Install git hooks
cargo build            # Build the CLI
cargo test             # Run tests
```

To exercise the CLI from your shell, install a standalone development snapshot:

```bash
./scripts/dev-install.sh
```

This copies the manifest-declared binary bundle under `~/.ovm/self/versions/`;
it does not symlink into the checkout. Rerun it after code changes. The installed
CLI continues working if the repository is moved.

## Development Workflow

1. Create a branch from `main`
2. Make your changes
3. Run the pre-commit checks (automatic via git hooks):
   - PII/secret scanning
   - `cargo fmt`
   - `cargo clippy --all-targets --all-features -- -D warnings`
   - `cargo test`
   - `shellcheck` when installed locally
   - `actionlint` when installed locally

   The pre-push hook can additionally send your unpushed diff to a local
   `codex` or `claude` CLI for an AI review. This is opt-in (your diff
   leaves your machine): enable it with `OVM_HOOK_AI_REVIEW=1`.
4. Commit with conventional format: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `chore:`
5. For significant changes, add a devlog entry in `docs/devlog/`
6. Update `CHANGELOG.md` for user-facing changes
7. Open a pull request against `main` (see "How this project is maintained"
   above for what to expect)

## Project Structure

```
crates/ovm/     Rust CLI (core)
tools/          Node.js plugin packages (e.g. ovm-benchmark)
npm/            npm distribution packages for the core binary
docs/           Architecture notes, devlog, version registry JSON
scripts/        Build, release, registry-update automation
tests/          Compatibility data (known-features.json)
.hooks/         Git hooks
```

## Code Style

- Rust: `cargo fmt` defaults, `cargo clippy` clean
- Shell: `shellcheck` clean
- GitHub Actions: `actionlint` clean
- Keep functions small and focused
- Prefer explicit error types over `.unwrap()`
- Tests alongside source in `#[cfg(test)] mod tests`
- No unnecessary dependencies

## What We're Looking For

- Bug fixes with test coverage
- Performance improvements
- New product support (beyond Claude Code, Codex, and Pi)
- Plugin examples (any `ovm-*` binary on PATH becomes a plugin — see `docs/architecture.md`)
- Platform support (Linux improvements, Windows exploration)
- Documentation improvements

## Support Policy

OVM currently supports macOS and Linux only.

- macOS changes can assume maintainer validation
- Linux changes run in GitHub Actions, but they are not yet manually validated by the maintainer
- Windows is not supported because the current symlink and launcher model is Unix-first

## What to Avoid

- Large refactors without prior discussion
- Adding dependencies for things the standard library handles
- Changes that break backward compatibility without discussion
- Generated code, AI slop, or copy-pasted boilerplate

## Reporting Issues

Open an issue at [github.com/ovm-sh/ovm-oss/issues](https://github.com/ovm-sh/ovm-oss/issues).

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
