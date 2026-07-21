# Fork, build, and import a custom version into OVM

OVM can run your own patched builds side-by-side with official releases. A
local build is imported as a **dev version** (`dev:<label>`), switchable and
launchable exactly like any installed version, and never touched by
auto-update.

Product support:

- **Codex** — fully open source (`openai/codex`, Rust). The canonical
  fork-build-import target.
- **Pi** — open source (`earendil-works/pi`); ships as a bundle (binary +
  package.json + themes + wasm), so import with `--bundle`.
- **Claude Code** — not open source. Dev imports only make sense for locally
  repacked official binaries; there is nothing to fork.

## Codex: the full loop

```bash
# 1. Fork on GitHub, then clone your fork
git clone git@github.com:<you>/codex.git && cd codex

# 2. Make your changes, then build the CLI (Rust workspace lives in codex-rs/)
cd codex-rs && cargo build --release

# 3. Import the build into OVM under a label
ovm install codex dev --dev mypatch --binary codex-rs/target/release/codex

# 4. Switch to it (or launch one-off without switching)
ovm use codex dev:mypatch
ovm cx --ovm-version dev:mypatch

# 5. Iterate: rebuild, then re-import the same label to refresh it
cargo build --release && ovm install codex dev --dev mypatch --binary codex-rs/target/release/codex
```

For a tight edit-build-run loop, `--link` imports a live reference instead of
a copy, so every rebuild is immediately what `dev:mypatch` runs — at the cost
of the version breaking if the checkout moves. Copies (the default) are
content-addressed and survive anything.

Notes:

- Codex 0.144.0+ spawns the `codex-code-mode-host` sidecar for shell
  commands. Official installs bundle it; for a dev build either build that
  target too and import with `--bundle <dir>` containing both binaries, or
  expect shell-command execution to fail in the dev version.
- `ovm ls codex` shows dev versions alongside official ones; remove one with
  `ovm uninstall codex dev:mypatch`.
- Dev versions are exempt from the verified-registry gate — they are your
  own builds and OVM treats them as trusted local artifacts.

## Pi: bundle import

```bash
git clone git@github.com:<you>/pi.git && cd pi
# build per upstream instructions, then import the built bundle directory:
ovm install pi dev --dev mypatch --bundle path/to/built-bundle
ovm use pi dev:mypatch
```

## OVM itself

Hacking on OVM is a different flow — `./scripts/dev-install.sh` installs a
content-addressed snapshot of your checkout as the active control plane. See
CONTRIBUTING.md.
