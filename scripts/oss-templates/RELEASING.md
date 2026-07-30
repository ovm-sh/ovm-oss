# Releasing OVM

Public releases are built only by `.github/workflows/release.yml`, dispatched
at an existing public `v*` tag with the same tag supplied as its `tag` input.
Never copy or mirror an archive from another repository or workflow. Before
publishing, repository rules must block tag updates/deletions and GitHub
immutable releases must be enabled. The workflow rejects a branch dispatch,
re-checks the remote tag immediately before upload, and refuses a moved or
deleted tag.

The public repository must provide `OVM_PRIVATE_RELEASE_TOKEN` with narrowly
scoped read access to the private release and tag, commit statuses, and Actions
runs. Every dispatch fails closed unless the private tag points at the supplied
commit and the current green `ovm-alpha-canary` status identifies a completed,
successful canary run on that exact commit.

## Local preflight

From a clean public `main` checkout:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
bash tests/scripts/export-oss.sh
```

Confirm every Cargo package named by `crates/ovm/ovm-bundle-v1.tsv` has the same
version and that `CHANGELOG.md` describes the release. Use
`./scripts/release.sh <version>` to update package versions, commit, and create a
local lightweight tag. Review that commit and tag before any push.

## Public tag workflow

The private promotion coordinator pushes the public tag, then dispatches the
workflow with `--ref <tag>`, `tag=<tag>`, and an explicit `stage`. The workflow
validates all three identities before it runs tests on macOS and Linux, builds
the four supported target bundles from that public commit, checks the bundle
manifest, emits portable SHA-256 sidecars, attaches build-provenance
attestations, and stages the GitHub release. It does not publish crates.io,
npm, or Homebrew packages.

Publication is draft-until-final. `stage=draft` stages a fully verified
UNPUBLISHED draft without moving the installed fleet; validate that draft from
a clean, isolated install. `stage=finalize` does not build: it requires the
staged draft bound to the same private commit, re-verifies every expected
asset and sidecar, then performs the one and only publish with the final
prerelease/Latest state and rechecks the immutable tag. Immutable releases
lock a release at publication, so nothing is edited afterwards; marking a
prerelease-suffixed tag as Latest requires the typed `confirm_latest`
confirmation. GitHub release bundles and the direct installer are the
supported channels. npm, Homebrew, and crates.io are prepared future channels
and are not part of this public workflow.

For every release, verify that these identities agree:

- the public tag object and workflow commit;
- every manifest-declared Cargo package version;
- the provenance attestation subject and commit;
- each archive and its SHA-256 sidecar;
- the version reported by the extracted `ovm` binary.

If any identity differs, quarantine the release. Do not reuse a published
version: ship a strictly higher version so existing clients can recover.
