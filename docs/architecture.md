# Architecture

## Overview

OVM is a Rust CLI binary that manages versions of AI coding tools. It downloads, installs, switches, and launches product binaries from multiple sources.

## Platform Scope

OVM currently targets macOS and Linux.

- **macOS** — supported platform for maintainers and manually tested
- **Linux** — covered in CI, but not yet manually tested by the maintainer
- **Windows** — not supported (symlink + launcher model is Unix-first)

## Core Concepts

### Products

OVM manages multiple products, each with its own download source and storage layout:

| Product | Aliases | Source | Binary |
|---------|---------|--------|--------|
| Claude Code | `claude`, `cc` | GCS CDN (native) + npm registry | `claude` |
| Codex | `codex`, `cx` | npm registry + GitHub Releases (`openai/codex`) | `codex` |
| Pi | `pi` | GitHub Releases (`earendil-works/pi`) | `pi` |

Products are defined in the `Product` enum (`crates/ovm/src/product.rs`). Adding a new product means adding a variant, a source module in `sources/`, and updating `version_manager::install_*`.

A declarative plugin system for custom products (`~/.ovm/products.d/*.toml`) is planned for v0.2 — see `docs/features/backlog.md`.

### Storage Layout

```
~/.ovm/
├── products/
│   ├── claude/
│   │   └── versions/
│   │       └── 2.1.91/
│   │           ├── native/claude
│   │           └── npm/installed/
│   ├── codex/
│   │   └── versions/
│   │       ├── rust-v0.120.0/release/bin/codex
│   │       └── dev:my-fix/dev/bin/codex
│   └── pi/
│       └── versions/
│           └── 0.67.6/release/bundle/pi/pi   # Pi ships as a bundle
├── bin/
│   ├── claude -> ...             # Active Claude binary
│   ├── codex  -> ...             # Active Codex binary
│   └── pi     -> ...             # Active Pi binary
├── hooks/                        # Lifecycle hook scripts
└── config.json
```

All managed products use namespaced directories under `~/.ovm/products/<name>/`.

### Version Sources

- **Native** — pre-built platform binary (Claude via GCS)
- **npm** — Node.js package from npm registry (Claude)
- **Release** — GitHub Release archive (Codex, Pi), with npm platform tarball fallback for Codex
- **Dev** — local binary or symlink for development builds (Codex)

Codex uses a shared upstream GitHub Releases feed that also contains internal
build tags and dependency releases. OVM treats only `rust-v...` releases with
Codex binary assets as installable.

### Symlink Switching

Version switching is atomic: write a temp symlink, then rename over the current one. This guarantees no window where the symlink is missing.

### Version Registry

To avoid slow paginated GitHub API calls, version lists are served from a static registry:

```
https://ovm.sh/api/claude.json
https://ovm.sh/api/codex.json
https://ovm.sh/api/pi.json
https://ovm.sh/api/registry.json     # product index
```

Each product JSON lists all versions with publish dates. Refreshed by `scripts/update-registry.sh`. OVM fetches the registry with a short timeout (5s) and falls back to direct upstream calls if unreachable.

### Plugin System

Any binary named `ovm-<name>` on the user's `$PATH` is auto-discovered as a plugin:

- `ovm help` lists discovered plugins under a "Plugins" section
- `ovm <name>` executes `ovm-<name>` with remaining args

Follows the git subcommand extension pattern. Implemented in `crates/ovm/src/plugins.rs`.

## Module Structure

```
crates/ovm/src/
├── main.rs              # Entry point, CLI dispatch, plugin routing
├── cli.rs               # clap command definitions
├── product.rs           # Product enum and metadata
├── version_manager.rs   # Core install/use/uninstall logic
├── config.rs            # Storage paths and configuration
├── error.rs             # Error types
├── symlink.rs           # Atomic symlink operations
├── hooks.rs             # Lifecycle hook execution
├── node.rs              # npm/fnm binary discovery
├── mochi.rs             # Mascot ASCII art (DEFAULT/HAPPY/SAD)
├── plugins.rs           # PATH scan for ovm-* binaries
├── dev_metadata.rs      # Git metadata for dev installs
├── release_metadata.rs  # Provenance metadata for release installs
├── commands/            # Command implementations (select, use, install, ls, …)
└── sources/             # Download backends
    ├── gcs.rs              # Google Cloud Storage (Claude native)
    ├── npm.rs              # npm registry (Claude packages)
    ├── codex.rs            # GitHub Releases + npm fallback (Codex)
    ├── pi.rs               # GitHub Releases (Pi)
    ├── github_releases.rs  # Release-notes fetcher
    └── registry.rs         # ovm.sh/api/ fetcher
```

## Data Flow

### Install

```
User: ovm install codex latest
  → resolve "latest" via GitHub Releases API
  → download platform-specific tar.gz
  → hash downloaded archive and persist release/meta.json
  → extract binary to ~/.ovm/products/codex/versions/rust-v0.120.0/release/bin/codex
  → set executable permissions
  → run post-install hook if present
```

### Use (Switch)

```
User: ovm use codex rust-v0.120.0
  → verify version is installed
  → verify binary exists (not archived)
  → run pre-switch hook
  → atomic symlink: ~/.ovm/products/codex/current -> versions/rust-v0.120.0
  → atomic symlink: ~/.ovm/bin/codex -> resolved binary path
  → run post-switch hook
```

### Select (Interactive)

```
User: ovm select
  → fetch registry for each product (fast, single HTTP)
  → interactive picker (arrow keys, release dates, companion indicators)
  → on choice:
      if installed → switch
      if not installed → prompt y/n, install, switch
```

### Launch

```
User: ovm cc exec main.py
  → bypass clap (raw args passthrough)
  → resolve and install latest release first if auto-update policy is `on`
  → spawn a background all-product registry refresh if cache is due
  → prune inactive old installs according to cleanup retention
  → auto-install if no active version (for `latest` / bare version args)
  → export OVM_PRODUCT + OVM_VERSION for the launched process
  → exec the product binary with remaining args
```

The background refresh is registry-only and protected by a short-lived lock, so
parallel terminals do not stampede the registry. Explicit `latest` requests and
launches with auto-update enabled may still fall back to upstream APIs when the
registry is unavailable.

Install cleanup is local-only. The default retention is 30 days; it removes
inactive release installs older than the configured window and skips active
versions, archived stubs, and dev installs.

## Distribution

Distribution paths:

1. **crates.io** — `cargo install ovm`
2. **GitHub Releases** — prebuilt tarballs per platform
3. **Homebrew** — custom tap (`ovm` stable formula, `ovm-beta` prerelease formula)
4. **npm** — platform-specific binary packages

`ovm self-update` updates the OVM binary itself through the detected install
method or an explicit `--method cargo|brew|dev`. The optional beta lane is
selected with `--channel beta`; cargo pins the latest crates.io prerelease and
Homebrew switches to the `ovm-beta` formula.

Published artifacts target macOS and Linux. Manual maintainer validation is currently macOS-only.
