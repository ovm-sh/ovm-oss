# Changelog

All notable changes to OVM will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

<!-- The version baseline is managed by the release owner; see RELEASING.md.
     Do not bump versions in feature commits. -->

## [0.0.3-alpha.7] - 2026-07-31

### Changed

- Every `ovm` invocation now kicks the same due-gated background refresh as
  product launches — built-in commands, claudex aliases, and `ovm-*` PATH
  plugins alike, from a single hook ahead of command dispatch (`ovm self …`
  stays exempt; those commands manage versions deliberately). Users who only
  interact with the `ovm` CLI, or only through a plugin, previously never
  staged a self-update; now a stale check (older than `updateCheckInterval`)
  triggers staging on any command, and the next invocation activates it with
  the usual `↑ OVM <new>` notice.

### Fixed

- A self-update staging attempt that fails after the version check succeeds
  (transient network drop mid-download) no longer waits out the full
  `updateCheckInterval` before retrying: the background check re-arms
  whenever the cached latest is newer than the active version and no
  matching update is staged. The retry serves the version lookup from the
  local cache, so only the outstanding download is repeated.

## [0.0.3-alpha.6] - 2026-07-30

### Changed

- The `ovm` banner now shows the running OVM version, so a bare `ovm` is
  enough to check which version is active.

## [0.0.3-alpha.5] - 2026-07-30

Proof release. Verifies real public-channel self-update between public
alphas (`ovm self update` from v0.0.3-alpha.4, whose release feed is the
public repository, hot-swaps to this version). No functional changes
beyond the version itself.

## [0.0.3-alpha.4] - 2026-07-30

Recovery release. This is the first release published from the repaired
public history of `ovm-sh/ovm-oss` (clean-history baseline with tag
protection and immutable releases). It supersedes `v0.0.3-alpha.3`, whose
public release was removed because its artifacts could not be tied to the
public tag that should have produced them. Installs of the old alpha recover
automatically by updating to this strictly newer version.

### Changed

- OVM now always appears as a product in `ovm select` (previously gated behind
  the alpha channel or `advanced.selfInPicker`). Its entry opens a self menu:
  a "manage versions" toggle (defaults on for the alpha channel) that reveals
  the installed OVM versions for swapping — dev snapshots, alphas, and stables
  side by side — plus the self auto-update policy and a new retention setting.
- New `self.retentionDays` setting (keep forever / 30 / 90 days, cycled in the
  self menu): after a successful self-update, inactive OVM *release* versions
  older than the window are pruned. The active and previous versions are
  always kept.
- One-way OVM versions are now guarded: releases older than `0.0.3-alpha.1`
  have no `ovm self` commands, so switching to one means no way back short of
  reinstalling. The self menu greys them out ("no way back") and refuses to
  select them; `ovm self use` asks for explicit confirmation on a terminal and
  refuses non-interactively.
- Dev snapshots get readable labels: `dev-<branch>-<MMDD>-<hash8>` instead of
  a bare 16-character content hash.
- Public artifacts now point at the public `ovm-sh/ovm-oss` repository: the
  curl|sh installer, `ovm self update` release feed, npm package metadata,
  in-CLI error links, and site links.
- Mochi the Cat now greets, works, and celebrates in the curl|sh installer,
  and wears brand-purple fur in both the installer and the CLI (color is
  applied only when the destination stream is a terminal).

### Fixed

- Explicitly selecting a version (`ovm use <product> <version>`, `ovm <product>
  <version>`, or the picker) now records it as a deliberate pin. With
  auto-update `on`, a plain launch no longer silently jumps a pinned selection
  to the newest release — it asks first (or prints one deduplicated notice when
  non-interactive) and launches the pinned version on decline. Selection prints
  a heads-up that auto-updates will ask, and any follow-latest action (`ovm use
  <product> latest`, `ovm <product> latest`, or accepting the update prompt)
  clears the pin and resumes normal auto-updates.
- Requested benchmark measurements must all complete before a version is
  successful. Failed and model-incompatible runs remain queued, outlier samples
  are rerun within a bound, and the public model matrix keeps incompatibility
  evidence without turning it into latency.
- Config writes are locked, atomic, and preserve settings introduced by newer
  OVM versions. Built-in command dispatch now comes from Clap, terminal modes
  restore on every exit path, and non-UTF-8 arguments return an error instead of
  panicking.
- Codex schema-skew checks use authoritative read-only SQLite migration state
  and report unreadable or malformed state as indeterminate instead of safe.
  Migrations through Codex 0.145 are backfilled, including the destructive
  migration 42 boundary. New stable Codex releases now regenerate the embedded
  manifest from the exact upstream tag and test the guard before the verified
  registry refresh is committed.
- The public site documents that newer models can require newer CLI harnesses;
  incompatible version/model pairs are listed as gaps rather than benchmark
  failures or zero-time samples.

### Security

- ovm.sh now self-hosts its chart libraries (previously loaded from jsDelivr
  without integrity hashes) and ships CSP, HSTS, and related security headers.
- Benchmark data generators redact runner home paths.
- `install.sh` enables `pipefail` where the shell supports it.
- Claudex applies redirect-origin checks, bounded streaming downloads, and
  bounded atomic extraction. Proxy authentication sends the real key only on
  the exact TCP connection whose peer process was verified.
- The OSS export copies tracked allowlisted files only, strips private-only
  development sections, and supplies runnable public CI/release instructions.
  Public release workflows verify package-version alignment, tag identity,
  checksums, and build-provenance attestations before upload.

## [0.0.3-alpha.3] - 2026-07-17

Observation release: exercises the staged self auto-update live (a machine on
`alpha` with `self.autoUpdate=on` should stage this in the background and
activate it on the following invocation).

## [0.0.3-alpha.2] - 2026-07-17

*The self-driving update.* OVM now keeps itself current the way it keeps
products current — staged in the background, activated atomically between
invocations, never in the way.

### Added

- Unified `on | off | notify` auto-update policy for products AND for OVM
  itself (`ovm autoupdate self on|off|notify`; `self.autoUpdate` defaults to
  on). Under `on`, a background check stages a newer OVM (checksum-verified,
  immutable install), and the next invocation activates it atomically with a
  single `↑ OVM <new> (was <old>)` line — the launch hot path never touches
  the network. Under `notify`, a TTY gets a one-keypress
  `[i]nstall now, [s]nooze` prompt (5s timeout defaults to snooze, 3-day
  per-version snooze); non-TTY gets a single deduplicated notice. Dev
  snapshots are never auto-updated.
- ovm.sh homepage release feed — OVM's own releases now surface on the ovm.sh
  homepage alongside the managed products.

### Fixed

- The auto-dispatched alpha canary now runs: the release workflow chains it
  explicitly and the dispatch is granted `actions: write`.
- The mini runs the alpha canary and the deep benchmark lane exclusively
  (shared concurrency group), and the canary retries verdict posting through
  transient API failures.

## [0.0.3-alpha.1] - 2026-07-17

*The go-public hardening train.* Everything since the sidecar catch, gathered
into the first release-candidate lane ahead of open-sourcing: self-managed OVM,
the claudex harness, release watching, and a sweep of pre-OSS security fixes.
This train also stood up the alpha release lane end to end — tag → build →
GitHub prerelease → mini canary (`ovm-alpha-canary` commit status) → opt-in
`ovm self channel alpha` update path — as a dry run of the release machinery
before any package-channel publishing.

### Added

- **Self-managed OVM hot-swap** — the recommended direct installer now keeps
  immutable OVM bundles under `~/.ovm/self/versions/` and a standalone control
  plane at `~/.ovm/bin/ovm`. `ovm self update/current/list/use/rollback` can
  atomically switch active OVM behavior while retaining a working escape path
  from historical binaries. Checkout development installs content-addressed
  copies instead of repository-bound symlinks, so repositories can move safely.
  A versioned bundle manifest dynamically drives release archives, direct/Cargo
  updates, npm, Homebrew, and side-binary link reconciliation; side binaries may
  be added or removed without hard-coded installer counts.
- **claudex** — Claude Code as the harness, GPT-5.6 (Sol) as the model, via a
  local CLIProxyAPI sidecar and the user's own ChatGPT/Codex subscription
  OAuth. New `ovm-claudex` plugin crate: `ovm claudex setup` (guided intro,
  localhost-only proxy config with a random key, Codex OAuth, isolated
  `CLAUDE_CONFIG_DIR` home so claudex history never mixes with normal
  `claude`, infinite session retention, generated model-registry CLAUDE.md
  importing the user's global one), `launch` (proxy supervision + env
  injection + full-size Mochi banner showing the Claude/proxy version pair),
  `doctor`, and `stop`. The native `/model` picker maps opus/sonnet/haiku to
  gpt-5.6-sol/terra/luna; a `pin` config freezes a known-good
  (Claude, proxy) pair. Launch via `ovm ccx` / `ovm ccxy` (yolo), the
  claudex entry in `ovm switch`, or the bare `claudex`/`ccx`/`ccxy` shims.
  Also: `--fast` (OpenAI priority service tier via forked proxy model
  aliases — the same wire field Codex CLI's fast toggle sets), managed
  checksummed CLIProxyAPI installs (`ovm claudex update [version]` with
  restart-verify and rollback; setup needs no brew), `uninstall [--purge]`,
  durable per-history feedback correlation (`feedback-id` plus private JSON
  relationships under `~/.ovm/claudex/history/relationships/`, stable across
  resume and allocated locally before any upload), and preview-first native
  Codex feedback (`feedback`; explicit `--send` and separate `--include-logs`
  consent) tagged with that relationship and archived with the returned Codex
  feedback thread ID,
  and a hardened runtime: two-step canary identity probe (no key or traffic
  to unverified listeners), ambient `ANTHROPIC_*`/`OPENAI_API_KEY` scrubbed
  from child processes, PID identity (name + start time) before any signal,
  atomic 0600-from-creation credential writes, and a fake-proxy e2e suite.
- `ovm shortcuts` — installs bare `ccy`/`cxy`/`ccx`/`ccxy`/`claudex`
  commands as one-line shims in `~/.local/bin` (no shell rc edits), skips
  foreign files, warns when the shim dir is off PATH, and detects the
  claude-yolo rc block to explain how the two coexist.
- Release Canary workflow (`release-canary.yml`): scheduled macOS + Linux
  verification of newly published Claude/Codex/Pi releases — including the
  Codex `alpha` and Claude `next` channels — via Release Radar, with a
  registry-refresh PR when the checked-in version registry changed. Replaces
  the retired `update-registry.yml` and the manual Claude-only
  `version-canary.yml`.
- Release Radar: `probe` command (execution probe with runtime-sidecar and
  migration-skew checks; classifies healthy / auth-required / broken),
  `assets-diff` command (release asset manifest diffing for packaging early
  warnings), alpha/next channel watches, and `config --reset-defaults`.
  Stable watches now probe a version before `ovm use` promotes it.

### Fixed

- Pre-OSS security review hardening: `ovm info` rejects versions containing
  path separators/traversal before they become GitHub API URL segments, the
  release-notes HTTP client refuses non-HTTPS and cross-host redirects, and
  claudex proxy downloads pin redirects to HTTPS GitHub release hosts —
  matching the redirect policy the core download paths already enforced.
- A CLIProxyAPI binary that exits during startup now fails the launch
  immediately with its exit status instead of polling the port for the full
  10-second startup budget.
- Benchmark HTML reports link to the public `ovm-sh/ovm-oss` repository
  instead of the private site repository.
- Claudex now honors `auto_update_proxy`: launches check CLIProxyAPI releases
  on a 15-minute cache, checksum-verify and stage newer managed binaries, then
  activate them only while no Claudex session holds the shared proxy lease.
  Pins and disabled auto-update remain fixed, concurrent launchers serialize
  publication, failed checks launch the installed proxy, and the banner reports
  the version actually running rather than a staged `current` target. Existing
  pre-lock daemons are never restarted implicitly; after their sessions exit,
  one explicit `ovm claudex update` migrates them to guarded auto-activation.
- Concurrent installs of the same product/version are now single-flight across
  processes. Contenders wait visibly and reuse the completed install; source-local
  completion markers recover safely from crashes without deleting another valid
  source. Native Claude downloads now show bytes, percent, transfer rate, and ETA.
- Claude launch hygiene no longer claims the native updater can reclaim control
  when only an inert native download tree remains under an OVM-owned launcher.
  `ovm doctor claude` continues to report that tree for explicit disk cleanup.

## [0.0.2] - 2026-07-10

*The Codex 0.144 sidecar catch — the release that proved the watcher.* Cut
immediately after the code-mode-host sidecar break was caught and fixed, along
with the npm-extraction hardening and the release-workflow plumbing that made
the canary demo cut possible.

### Fixed

- Codex installs now include the `codex-code-mode-host` sidecar binary that
  Codex 0.144.0+ spawns for every shell command. Previously only the main
  binary was installed, leaving 0.144.0+ unable to execute any command
  ("failed to spawn code-mode host"). Applies to both the GitHub-release path
  (separate sidecar asset) and the npm fallback (extracted from the platform
  package); releases that don't publish the sidecar install as before.
- npm-fallback extraction now matches the main binary entry by exact name
  instead of taking the first `codex*` entry, so archive entry order can no
  longer install the wrong binary as `codex`.
- Failed Codex downloads/extractions no longer leave a partial archive
  (`codex.npm.tgz` / `codex.tar.gz`) behind in the version's bin directory.

## [0.0.1] - 2026-07-10

*The internal baseline.* The first internally-tagged OVM snapshot — the tool as
it stood just before the Codex 0.144 code-mode-host sidecar break was caught.
See [0.0.0] for the foundation feature set this internal train built on.

## [0.0.0] - 2026-06-23

*The internal foundation snapshot.* The feature set the 0.0.x train started
from — numbered 0.1.0 inside the pre-reset internal train, but never published;
the real first public release carries the version the public repo ships.

### Added

**Core version management**
- Multi-product install / switch / uninstall for Claude Code, Codex, and Pi
- Install Claude from native GCS binaries or npm packages
- Install Codex from GitHub Releases (including local dev builds via `--dev`)
- Install Pi from GitHub Releases — extracts the full bundle (package.json + themes + wasm), not just the binary
- Atomic symlink switching for zero-downtime version changes
- Auto-install on launch: `ovm cc latest` installs and launches the latest if not present
- Lifecycle hooks (pre/post-install, switch, uninstall)
- Product aliases: `claude`/`cc`, `codex`/`cx`
- Launchers with `--ovm-version` override for one-off testing

**Interactive picker**
- `ovm select` — TUI picker for choosing products and versions
  - No-arg mode: product picker → version picker
  - `ovm select <product>` jumps straight to version picker
  - `ovm select <product> <version>` switches directly (prompts to install if missing)
  - Release dates shown per version (via registry)
  - Product-specific companion indicators for Claude `/buddy` and Codex `/pet`
  - Press `i` to view release notes inline
  - `esc` navigates back (version → product picker → quit)
- `d` to delete an installed version (with y/N confirm)
- Prompt to launch right after switching versions (accepts `y` / `n` / `ccy` / `cxy`)

**Discovery & info**
- `ovm info <product> [version]` — show release notes fetched from GitHub Releases
- `ovm current <product>` — active version
- `ovm which <product>` — path to active binary
- `ovm stats` — installed/archived counts, active version, disk usage per product
- Version registry at `ovm.sh/api/` — single-request version lists with dates (~8× faster than paginated GitHub API for Codex)
- Buddy compatibility tracking in `tests/compatibility/known-features.json`

**Maintenance**
- `clean` and `archive` commands for disk space management
- Friendly error for missing product argument

**Companion guards & install hygiene**
- `ovm-codex-skew` — Codex's schema-skew guard, extracted from the `ovm` binary
  into a native companion plugin (new crate `crates/ovm-codex-skew`). Core now
  runs it automatically as a **mandatory companion** at pre-launch and
  post-switch, and `ovm doctor codex` delegates its report to it (env contract
  `OVM_EVENT`/`OVM_PRODUCT`/`OVM_VERSION`/`OVM_BINARY`, fail-open — a missing or
  erroring companion never blocks a launch). Companions are resolved
  deterministically (`~/.ovm/companions/` then alongside the `ovm` binary),
  never via PATH. The 35-entry Codex migration manifest moves out of the core
  binary with it. Every official distribution (GitHub release tarball,
  `install.sh`, npm, Homebrew) bundles `ovm-codex-skew` alongside `ovm`, and it
  is prepared for a future crates.io publication of `ovm-codex-skew` — see
  `RELEASING.md`.
- `ovm doctor claude --fix` — Claude install hygiene. Reports (and with `--fix`
  repairs) setups where Claude could wrest version control back from OVM: it
  flips `installMethod` off `native` (the trigger for Claude's self-updater,
  which otherwise re-downloads hundreds of MB into `~/.local/share/claude/` and
  repoints `~/.local/bin/claude`), forces `autoUpdates: false`, removes the
  stray `~/.local/share/claude` native install tree, and makes
  `~/.local/bin/claude` an **OVM-owned launcher** (a symlink to the managed
  `~/.ovm/bin/claude`). Config edits preserve key order and write atomically.
  `"autoUpdates": false` alone does not stop the native updater — the install
  method is what matters.
- Claude launcher is now kept healthy automatically: every `ovm use claude`
  (and each managed Claude launch) re-points `~/.local/bin/claude` at the
  managed binary if it's missing or a stale symlink — silent and idempotent, and
  it never deletes a real file (that stays a `--fix` decision). This silences
  Claude Code's interactive startup probe, which otherwise prints
  `⚠ claude command at ~/.local/bin/claude missing or broken` once OVM takes
  over. When the native updater is still armed (install method `native`, or a
  native install tree present), a managed launch prints a one-line nudge to run
  `ovm doctor claude --fix` rather than mutating anything.

**Distribution**
- Release automation for prebuilt binaries on `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`
- Package-channel automation prepared for crates.io (`ovm` + `ovm-codex-skew`), npm platform packages, and a Homebrew tap — every prepared channel bundles the `ovm-codex-skew` companion alongside `ovm`. None of these were published; GitHub release bundles and the direct installer are the supported channels.
- `curl -fsSL https://raw.githubusercontent.com/ovm-sh/ovm-oss/main/install.sh | sh` installer

**Developer experience**
- Shell completions for bash, zsh, fish
- `scripts/dev-install.sh` — symlink `~/.cargo/bin/ovm` to the local release build for fast iteration
- `scripts/release.sh` — local release-cutting helper (Cargo bump + CHANGELOG prompt + tag)
- Pre-commit hooks with PII scanning, secret detection, fmt/clippy/test, auto-rebuild on dev-install
- Pre-push hooks with automated agent review
- Version canary GitHub Actions workflow (compatibility tests on new releases)
- Registry update GitHub Actions workflow (refreshes product version lists)
- Launch hang/perf guards (`tests/launch_perf.rs`): assert `ovm <product>`
  reaches exec promptly against both a wedged update service (connection hangs)
  and an unreachable one (connection refused), with per-load timing output.

### Changed

- `ovm select` no longer blocks on the network before drawing. It renders
  installed versions instantly from the local cache in a dedicated **installed**
  section, with the full chronological history below (installed versions appear
  in both). A background registry refresh runs silently and folds fresher
  versions in live, with a status hint (`checking for updates…`, `updated 5m
  ago`, or `offline · showing cached versions`). On bad or no internet the picker
  stays responsive instead of hanging on the registry fetch.

### Fixed

- `ovm cc latest` / `ovm cx latest` now make the resolved version the default even when extra args are present (including the injected flag from `ccy`/`cxy`), so subsequent plain `claude`/`codex` spawns pick it up. Previously only the bare no-arg form switched the active symlinks; the yolo aliases launched the latest once and left the default pinned. `--ovm-version latest` remains an ephemeral override.

<!-- v0.0.3-alpha.4 is the first tag on the repaired public history; older
     versions predate it and intentionally have no public link targets. -->
[Unreleased]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.7...HEAD
[0.0.3-alpha.7]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.6...v0.0.3-alpha.7
[0.0.3-alpha.6]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.5...v0.0.3-alpha.6
[0.0.3-alpha.5]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.4...v0.0.3-alpha.5
[0.0.3-alpha.4]: https://github.com/ovm-sh/ovm-oss/releases/tag/v0.0.3-alpha.4
