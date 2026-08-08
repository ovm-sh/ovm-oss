# CLAUDE.md

Instructions for agents and contributors working in the public OVM source tree.

OVM is a Rust CLI version manager for Claude Code, Codex, Pi, and QM. The public
repository is `ovm-sh/ovm-oss`; its release assets must always be built from the
exact public tag that contains their source.

## Development gates

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
bash tests/scripts/export-oss.sh
```

For a standalone local snapshot, run `./scripts/dev-install.sh`. Re-run it after
code changes: building alone does not replace the installed snapshot.

## Code and security

- Prefer readable Rust and typed errors; do not use `unwrap` on user input.
- Keep tests isolated with temporary directories; never touch real `~/.ovm/`.
- Never commit credentials, personal absolute paths, or private project state.
- Treat redirect validation, archive bounds, checksums, and atomic filesystem
  changes as security boundaries.
- Update `CHANGELOG.md` for user-visible changes.

See `docs/architecture.md` for the product and storage model and
`CONTRIBUTING.md` for the contribution workflow.
