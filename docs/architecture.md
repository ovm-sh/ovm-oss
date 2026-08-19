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

Products are defined in the `Product` enum (`crates/ovm/src/product.rs`). Adding a new product is considerably more than that — the full path, including the parts the compiler does not check, is `docs/internal/adding-a-product.md`.

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
│   ├── pi/
│   │   └── versions/
│   │       └── 0.67.6/release/bundle/pi/pi   # Pi ships as a bundle
├── bin/
│   ├── claude -> ...             # Active Claude binary
│   ├── codex  -> ...             # Active Codex binary
│   ├── pi     -> ...             # Active Pi binary
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

Each product JSON lists all versions with publish dates. `registry.json` is the
small aggregate: it carries the stable latest and summary counts for every
product. Refreshed by `scripts/update-registry.sh`, it has an HTTP ETag. OVM
uses a five-second timeout and falls back to direct upstream calls for explicit
resolution when the registry is unreachable.

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
  → read the previously validated local latest snapshot
  → install it first if auto-update policy is `on` and it is newer
  → arm a detached conditional request for registry.json if the probe is due
  → prune inactive old installs according to cleanup retention
  → auto-install if no active version (for `latest` / bare version args)
  → export OVM_PRODUCT + OVM_VERSION for the launched process
  → exec the product binary with remaining args
```

The detached worker coalesces invocations to at most one product probe per
minute and sends the cached ETag. An unchanged registry returns `304` with no
body. A changed aggregate refreshes only the full product indexes whose summary
changed; an interrupted index download stays pending and is retried after later
`304`s. Offline failures back off exponentially to one hour. A short-lived lock
prevents parallel terminals from stampeding the registry. The foreground launch
never waits for this request and only consumes validated local state; explicit
`latest` requests may still fall back to upstream APIs.

Install cleanup is local-only. The default retention is 30 days; it removes
inactive release installs older than the configured window and skips active
versions, archived stubs, and dev installs.

## Distribution

Supported distribution paths:

1. **GitHub Releases** — verified prebuilt tarballs per platform
2. **Direct installer** — installs the complete verified GitHub release bundle

crates.io, npm platform packages, and the Homebrew tap are prepared future
channels. They are not published or supported yet. `ovm self-update` retains
implementations for those install methods, but the working public update path is
the direct GitHub release channel.

Published artifacts target macOS and Linux. Manual maintainer validation is currently macOS-only.
