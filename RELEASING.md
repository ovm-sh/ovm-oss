# Releasing OVM

This checklist is for the first public release and future tagged releases.

## Version Baseline

OVM is not live yet. The current pre-live package version is `0.0.1` across the
Rust crates and local npm packages. Do not suggest or cut `0.1.x`, `0.2.x`, or a
higher public version unless the release owner explicitly asks for that bump.

## Local Preflight

Run these from a clean `main` checkout:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo publish -p ovm --dry-run
cargo build --release
```

For release automation changes, also run `shellcheck` and `actionlint` when
available.

### Dependency refresh (deliberate, per release)

There are no routine Dependabot version-bump PRs — lockfiles pin everything,
security-only PRs arrive when a CVE actually affects us, and the weekly
scheduled `cargo-audit` plus per-push `cargo-audit`/`cargo-deny` catch
advisories in between. Freshness happens here, deliberately, as release prep:

```bash
cargo update && (cd tools/benchmark && npm update)
# then rerun the full preflight above; commit as one `chore(deps)` change
```

Update SHA-pinned GitHub Actions at the same time if their majors moved.

## Branch and Release Model

OVM uses the CLI release model:

```text
feature branch -> PR -> main -> v* tag -> GitHub Release -> manual package publish
```

There are no long-lived `dev`, `staging`, or `prod` branches. `main` should stay
releasable, and the pushed tag creates the release artifacts.

When GitHub branch protection is available for this repository, protect `main`
with:

- Require a pull request before merging
- Require one approving review
- Dismiss stale approvals when new commits are pushed
- Require branches to be up to date before merging
- Block force pushes and branch deletion
- Require these CI checks:
  - `Rust (macos-latest)`
  - `Rust (ubuntu-latest)`
  - `repo-hygiene`
  - `Scripts smoke`
  - `cargo-audit`
  - `cargo-deny`

Package publishing is manual: run the `Release` workflow with an existing `v*`
tag after reviewing the GitHub Release artifacts, and type the exact
confirmation string `publish <tag>` (for example, `publish v0.0.1`). The publish
jobs also reference the GitHub `release` environment, so configure that
environment with required reviewers after the repository moves to an org/plan
that supports private-repo environment protection rules.

Until then, the practical protection is:

- `main` is protected by PR review and required CI.
- `v*` tags should be protected by a repository ruleset that blocks deletion and
  updates, and restricts tag creation to admins.
- Tag push only builds and creates the GitHub Release.
- Manual workflow dispatch publishes packages only after the exact
  `publish <tag>` confirmation passes.

## Public binary bundle

`crates/ovm/ovm-bundle-v1.tsv` is the authoritative list of OVM's public
binaries and Cargo packages. It currently declares the `ovm` control plane,
`ovm-codex-skew`, and `ovm-claudex`, but the number of side binaries is dynamic.
Every release archive includes the manifest plus exactly the binaries it names.
Release packaging, direct installation/self-update, Cargo publication, npm, and
Homebrew all consume that same manifest.

`ovm-codex-skew` guards older Codex versions from newer state DB schemas;
`ovm-claudex` supplies claudex dispatch. Direct installs keep each complete bundle
in an immutable `~/.ovm/self/versions/<version>/` directory and hot-swap one
`current` pointer. Adding or removing a side binary must therefore change the
manifest once and pass the cross-channel bundle contract tests.

For a source install, install every Cargo package listed by the manifest (side
packages first, then the main package). For the current bundle:

```bash
cargo install ovm-codex-skew ovm-claudex ovm --locked
```

Verify any packaged bundle by extracting it and comparing its top-level entries
to `ovm-bundle-v1.tsv`; no undeclared or missing binary is valid.

## First Release Checklist

1. Confirm `crates/ovm/Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, and
   `README.md` all name the intended version.
2. Confirm the GitHub release workflow can build the four supported targets:
   `aarch64-apple-darwin`, `x86_64-apple-darwin`,
   `x86_64-unknown-linux-gnu`, and `aarch64-unknown-linux-gnu`.
3. Configure only the package channels being published:
   - crates.io: `CARGO_REGISTRY_TOKEN`
   - npm: `NPM_TOKEN`
   - Homebrew: `HOMEBREW_TAP_TOKEN` plus the `ovm-sh/homebrew-ovm` tap
4. Create the local tag:

```bash
git tag v0.0.1
```

5. Push the tag to build and create the GitHub Release:

```bash
git push origin main --tags
```

The tag-push workflow builds and creates a GitHub Release, but it does not
publish to package channels. After reviewing the release artifacts, manually run
the `Release` workflow with the tag to publish packages. Stable tags such as
`v0.0.1` publish to crates.io, npm's `latest` dist-tag, and Homebrew when the
corresponding secrets are configured. Prerelease tags such as `v0.0.1-beta.1`
create a GitHub prerelease, publish every manifest-declared Cargo package,
publish npm packages under the `next` dist-tag, and update the Homebrew tap's
`ovm-beta` formula.

## Beta Channel Smoke

Use a prerelease tag to exercise the beta lane before promoting a stable tag:

```bash
./scripts/release.sh 0.0.1-beta.1
git push origin main --tags
```

After the tag-push workflow creates the GitHub prerelease and artifacts, inspect
the tarballs, then manually dispatch the `Release` workflow with:

```text
tag: v0.0.1-beta.1
confirm_publish: publish v0.0.1-beta.1
```

Then verify all opt-in update paths:

```bash
ovm self update --method direct --channel beta --dry-run
ovm self update --method cargo --channel beta --dry-run
ovm self update --method brew --channel beta --dry-run
```

## Self-update channels

`ovm self update` follows a persistent channel, default `stable`:

- `stable` — GitHub's `releases/latest` (prereleases excluded).
- `alpha` — the highest-semver release on the repo *including* prereleases
  (`v*-alpha.N`); when the newest release overall is the latest stable, alpha
  installs that. Alpha rides the same prerelease Homebrew formula (`ovm-beta`)
  and npm `next` dist-tag as beta.

Set it persistently or override a single run:

```bash
ovm self channel alpha            # persist self.channel=alpha in ~/.ovm/config.json
ovm self channel                  # show the current setting
ovm self update                   # uses the configured channel
ovm self update --channel alpha   # one-shot; the flag always wins over config
ovm self update --channel alpha --dry-run
```

The alpha selection reuses the crate's prerelease-aware `semver` ordering, and
the downgrade guard is prerelease-aware too: an alpha (e.g. `0.2.0-alpha.3`) is
newer than `0.1.0` but older than `0.2.0`, so channel-hopping back to a trailing
stable is refused with a clear message rather than silently downgrading.

## Alpha canary gate

Every `v*-alpha.N` prerelease fires `ovm-alpha-canary.yml` on the mini
(`release: prereleased`, or `workflow_dispatch` with a `tag` input). The canary
downloads the just-published `aarch64-apple-darwin` bundle into a throwaway
`HOME`, verifies its checksum, proves the prebuilt binaries (`ovm --version`
equals the tag and the installed-command matrix dispatches), and — when a stable
release already exists — proves the channel swap end to end (`ovm self update
--channel alpha` lands exactly this alpha, then `ovm self rollback` restores the
prior version). It reports the verdict as an `ovm-alpha-canary` commit status
(success/failure) on the tag's commit.

The manual `publish <tag>` path is gated on that status. The `Release` workflow's
`canary-gate` job requires a successful `ovm-alpha-canary` status on the tag's
commit before any package publish runs. A release with no proven canary must be
dispatched with the `skip_canary: true` input, which publishes with a loud
warning — releases never silently bypass the canary.

On a disposable machine or after confirming the dry-run output, run the same
commands without `--dry-run`. Direct install atomically activates the verified
bundle while retaining the previous version for rollback. Cargo installs every
manifest-declared package; Homebrew switches to the separate `ovm-beta` formula.

## Historical/fixed Demo Snapshots

The stable control plane can demonstrate a bug and its fix even when the older
OVM binary predates self-management. Build the same manifest-declared bundle from
both revisions, then install each artifact directory with the current installer:

```bash
OVM_LOCAL_ARTIFACT_DIR=/path/to/before/target/release \
OVM_LOCAL_MANIFEST=crates/ovm/ovm-bundle-v1.tsv \
OVM_LOCAL_VERSION=demo-before-sidecar sh install.sh

OVM_LOCAL_ARTIFACT_DIR=/path/to/fixed/target/release \
OVM_LOCAL_MANIFEST=crates/ovm/ovm-bundle-v1.tsv \
OVM_LOCAL_VERSION=demo-fixed-sidecar sh install.sh

ovm self use demo-before-sidecar  # reproduce the Codex sidecar issue
ovm self use demo-fixed-sidecar   # hot-swap once; rerun the same probe
ovm self rollback                 # return to the prior snapshot if desired
```

Normal commands execute the selected historical binary, but local snapshot
installation preserves the existing standalone control plane. `ovm self ...` is
therefore handled by current management code even when the active version is old.
A direct update also probes a replacement control plane and rolls back on failure.
If manual recovery is ever required, restore the saved manager with:

```bash
~/.ovm/self/control-previous self repair-control
```

## Registry Updates

Registry refreshes are manual: run `scripts/update-registry.sh` and commit the
result. It refreshes the static registry files under `docs/api/`, which are
served from `ovm.sh/api` and used as OVM's fast path for remote version
discovery. (The former 6-hour `update-registry.yml` workflow was retired — its
pushes were rejected by branch protection anyway. The Release Canary workflow
(`release-canary.yml`) now handles automated version discovery: it verifies new
releases on macOS + Linux via Release Radar and opens a registry-refresh PR
when the checked-in registry changed.)

The generator preserves files when the fetched version content is unchanged, so
scheduled runs do not create timestamp-only commits. If an upstream version that
was previously present disappears, the registry keeps it out of the active
`versions` list and records it under `retired_versions` with `last_seen_at` and
`retired_at` timestamps. Treat new retired entries as release-health signals:
check whether the upstream package/release was intentionally pulled, whether
OVM can still install it from cache, and whether any benchmark or docs references
need to exclude or annotate that version.

Before tagging a release, run:

```bash
bash scripts/update-registry.sh
git diff -- docs/api/ scripts/update-registry.sh
```

After deploy, verify `https://ovm.sh/api/registry.json` matches the checked-in
`docs/api/registry.json`.
