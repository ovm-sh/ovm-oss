# Changelog

All notable changes to OVM will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2] - 2026-08-11

The tag was re-cut on 2026-08-11 before its first public release to fold in
the fixes from a full external go-live audit.

### Security

- **Linux binaries run on the systems they install onto.** Release builds
  moved from `ubuntu-latest` (whose silent 24.04 bump stamped a glibc 2.39
  requirement into every published Linux binary) to a fixed **glibc 2.35
  floor** (Ubuntu 22.04+, Debian 12+, Fedora 36+). Every release leg now
  smoke-runs the packed archive it built — the aarch64 cross-build under
  qemu — and `install.sh` refuses older-glibc and musl systems with a
  build-from-source message instead of installing a binary the dynamic
  loader cannot start.
- **CI credentials are withheld from binaries under test.** The deep lane
  executes freshly downloaded upstream releases; those child processes no
  longer inherit repository tokens or deploy secrets. Checkouts on the
  benchmark runner stop persisting the Actions token, pushes authenticate
  step-scoped, and every spawn site of a product under test rebuilds its
  environment with secret-named variables stripped (the products' own auth
  excepted — exercising it is the point of the authenticated lanes).
- **Claudex auto-update only activates registry-approved versions.** The
  automatic path no longer falls back to upstream's `releases/latest` with a
  checksum from the same publisher; it acts on OVM-registry (deep-lane)
  verdicts and stands down when none is available. Explicit installs keep
  the fallback and say which authority they trusted. Legacy proxy pid
  records also gained an identity anchor (the pidfile's own mtime) so a
  recycled PID can no longer be signalled on a name match alone.
- **Release tags are bound to their proof.** A release tag must be an
  ancestor of `main` and its commit must have passed the full CI workflow —
  an off-main tag with matching version numbers can no longer bypass the
  per-push gates. macOS release binaries are Developer ID signed and
  notarized when the `APPLE_*` secrets are configured on the building repo;
  without those secrets the build warns loudly and ships ad-hoc signed
  binaries, and a build that is signed but has no notary key fails outright
  rather than publishing signed-but-unnotarized artifacts. Promotion then
  checks the Darwin binaries of both the private qualification assets and
  the rebuilt public assets: each must pass `codesign --verify --strict`,
  chain to a Developer ID Application certificate, and report a
  TeamIdentifier (pin it with `OVM_EXPECTED_DARWIN_TEAM_ID`). Promoting from
  a host without `codesign` is refused, because an unverifiable run must not
  look like a verified one. `OVM_ALLOW_UNSIGNED_DARWIN=1` downgrades every
  one of those refusals to a warning — the deliberate escape hatch for
  promoting a build cut before the signing secrets existed. Nothing
  re-checks Gatekeeper identity after publication: `install.sh` and
  `ovm self update` verify SHA-256 sidecars, not signatures. Each release
  also ships an SPDX SBOM alongside its checksums and provenance
  attestations.

### Added

- **`ovm self uninstall` — a supported way back out.** The installer edits shell
  profiles and drops launchers, snapshots, and side-binary shims across
  `~/.ovm`; until now removing all of that meant hand-editing an rc file most
  people never knew had been touched. The command strips the exact
  `# >>> ovm >>>` block the installer wrote (every other line in the profile is
  left byte-for-byte alone), removes the `~/.ovm/bin` launchers OVM still owns
  and its snapshots under `~/.ovm/self`, and **keeps your installed
  Claude/Codex/Pi versions and config** so a reinstall resumes where you left
  off. `--purge` takes the whole `~/.ovm` tree — including `~/.ovm/claudex`,
  which holds claudex's isolated Claude home, so the preview names that
  directory and what is in it rather than letting "the whole tree" stand in for
  your conversation history. It asks for a typed confirmation, `--yes` skips
  it, and a non-interactive shell without `--yes` refuses rather than assuming
  consent. A run that cannot remove something removes everything else, says
  which item failed, and exits non-zero — including when it could not so much
  as inspect a path, which is never reported as a clean uninstall. Only OVM's
  own files go: the `ovm` at the recorded launcher path is removed only once it
  proves to be one OVM wrote (anything else is left in place and named as
  such), and recursive deletion stays inside `~/.ovm` after every symlink is
  resolved. An install whose `~/.ovm` is itself a symlink onto another disk
  is refused outright — the preview names where the link really points, and
  removal is left to a human who has verified the target, rather than
  following a link into a directory the name didn't claim.
- **A Claudex sign-off that hands back the resume command.** Leaving a session
  now prints a sleeping Mochi and the exact command to pick it back up —
  `ccxy --resume <id>`, naming the shim the session was launched from, so a
  yolo or priority-tier session resumes as itself instead of dropping into a
  different one. Claude Code shows a hook's output only when the hook fails, so
  the sign-off is written to the terminal directly; sessions that end with
  `/clear` or a resume stay silent, and a session with no terminal prints
  nothing and cannot fail.
- **[ovm.sh/writing](https://ovm.sh/writing)** — a notes section for
  investigations behind the tooling, opening with how the context window above
  was established from the shipped Claude Code binary and 145 managed builds.

### Fixed

- **Claudex now tells Claude Code how big the proxied model actually is.**
  Claude Code sizes auto-compaction from the model name, and `gpt-5.6-sol` is
  not a name it recognises, so every Claudex session assumed a 200K window,
  compacted earlier than the model required, and printed an unknown-model
  notice at launch. Claudex declares **272,000** tokens — deliberately not the
  raw 1,050,000 API window: Claudex authenticates through Codex on a ChatGPT
  subscription, where prompts over 272K input tokens bill at 2x input and 1.5x
  output for the whole request. Sessions get their real headroom and stop where
  the billing rate changes. Override with `tuning.max_context_tokens`.
- Guided setup on a fresh machine exposes the freshly installed OVM on `PATH`
  before it runs, so the guided path works on a machine that has never had OVM.
- **`ovm story` is findable.** It shipped as a headline feature, the installer
  invited people to run it, and `ovm help` never mentioned it — working, but
  undiscoverable unless you already knew. It is now listed in the full help,
  with a one-line pointer on the bare `ovm` screen. A test now fails if any
  command exists without appearing in the written help.
- The story's Mochi link points at `mochiexists.com/plate`.
- A benchmark lane that fails partway now still exports the measurements it
  produced. Previously an aborted sweep skipped the ledger export entirely, so
  a run that measured real versions before hitting the usage kill switch
  committed nothing and reported "Nothing new" — published data silently lost
  work that had already been done.

## [0.1.1] - 2026-08-10

The guided Claudex journey and cat story qualified across three alpha releases
are now the stable `0.1.1` experience.

### Added

- **One-command guided Claudex onboarding.** Running
  `curl -fsSL https://ovm.sh/install | sh -s -- --claudex` can take a pristine
  machine through installing and activating the latest verified Claude Code,
  staging the local CLIProxyAPI sidecar, connecting a ChatGPT/Codex account,
  optionally installing the Codex CLI, and launching Claude Code on GPT-5.6.
  Claudex keeps its history isolated under `~/.ovm/claudex/claude`, and its
  first-launch banner pauses long enough to make that boundary visible.
- **`ovm story` — a tale of two cats and an echo.** The interactive terminal
  story begins when the user types `/buddy`; `--fast` and non-interactive runs
  play it straight through. The installer and Claudex setup invite new users
  to discover it after setup.

### Changed

- Codex releases that pass behavioral qualification are no longer withheld
  solely because the static schema classifier disagrees. The objection is
  published as a `gate-review` detection; behavioral and probe failures still
  veto the release.

### Fixed

- Fresh-machine guided setup now supplies the required `latest` version to its
  Claude Code and optional Codex CLI install commands, then explicitly activates
  Claude Code before launching Claudex.

## [0.1.1-alpha.3] - 2026-08-10

### Added

- **`ovm story` — a tale of two cats and an echo.** The interactive terminal
  story behind OVM's cats ships with the binary: where Quelpaw came from
  (Claude Code's `/buddy`, arrived 2.1.80, removed 2.1.97 without a changelog
  line), how Mochi got here, and what Echo really is. The story only hatches
  when the user types `/buddy` at the title screen; `--fast` (or a non-tty)
  plays straight through. One chapter pause can open mochiexists.com with a
  keypress. Previously a local prototype outside the install loop.
- The installer's closing lines and the guided claudex setup now invite the
  story: "there's a story behind the cats — `ovm story`".

## [0.1.1-alpha.2] - 2026-08-09

### Fixed

- The guided setup's inline Claude Code install invoked `ovm install claude`
  without the required version argument, so on a machine with no Claude Code
  it failed — and its error message recommended the same broken command.
  Caught by the first end-to-end run of the guided journey in a pristine
  container. It now runs `ovm install claude latest` and then activates it
  with `ovm use claude latest` (install deliberately never switches, and a
  fresh machine has nothing active). The optional Codex CLI offer had the
  identical latent bug and got the identical fix.

## [0.1.1-alpha.1] - 2026-08-09

### Added

- **Guided claudex onboarding.** `ovm claudex setup` is now a complete guided
  path: it installs Claude Code inline when it's missing (check-then-do, from
  the verified registry), stages the proxy, checks for an existing Codex OAuth
  grant before prompting, offers the Codex CLI as a clearly-optional extra
  (claudex needs your ChatGPT account, never the codex binary), and ends by
  offering to launch the session right there. The installer grew a matching
  entry point: `curl -fsSL https://ovm.sh/install | sh -s -- --claudex` chains
  straight into the guided setup, re-attaching the terminal for prompts; on
  machines with no usable terminal it prints the command to run later instead
  of failing an install that already succeeded, and it releases the
  self-operation lock before chaining so a launched session never blocks
  `ovm self` operations.
- **The claudex launch banner holds for a beat on first run** — long enough to
  read `history isolated → ~/.ovm/claudex/claude`, which is the one line a new
  user needs to trust the setup. Later launches stay instant; scripts and
  pipes never hold; `OVM_CLAUDEX_BANNER_HOLD_MS` overrides in both directions.

### Changed

- **The registry gate no longer holds a release that runs.** A Codex version
  whose behavioral qualification passes is admitted even when the static
  schema classifier disagrees; the objection is published as a `gate-review`
  detection instead of parking the release behind a hand-written adjudication
  (0.145.0, 0.146.0, and 0.147.0 each needed the same waiver for a
  permanently-disagreeing ladder rung). Behavioral failures, probe failures,
  and never-qualified versions are withheld exactly as before.

## [0.1.0] - 2026-08-09

The first stable release. OVM installs, pins, switches, and launches versions
of **Claude Code**, **Codex**, and **Pi** side by side — like `nvm` for the
CLIs you code with — with a verification layer none of them ship on their own:

- **Verified versions.** Every release OVM offers has been installed and run
  on real hardware (macOS and Linux) before the registry lists it. Coding
  agents are exercised through real tool-use probes; releases that fail are
  withheld per platform rather than offered.
- **Instant listings.** Version lists come from `ovm.sh/api` in one request,
  with a direct-upstream fallback when the registry is unreachable.
- **Side-by-side installs and hot switching.** `ovm use`, per-invocation
  `--ovm-version`, launch shortcuts (`cc`, `cx`, `pi`, and yolo/fast stacking
  like `cxyf`), pinning, and adoption of existing unmanaged installs — the
  original stays untouched and OVM tells you how to retire it.
- **Self-managing.** `ovm self update` follows a channel (`stable` by
  default, `alpha` opt-in) against immutable, build-provenance-attested
  GitHub releases; hot-swap and rollback of OVM itself are first-class.
- **Safe by default.** Launches never delete installs; destructive cleanup is
  explicit and confirmed. Signature verification pins the publishers' Apple
  Team IDs on macOS. A plugin system (`ovm-*` on PATH) extends the CLI.

The installer and the GitHub release bundles are the supported channels; npm,
Homebrew, and crates.io are prepared but not yet published.

## [0.0.3-alpha.14] - 2026-08-09

### Changed

- **Release qualification moved from a nightly schedule to the release cut.**
  The scheduled nightly lane is retired; release-profile tests, the e2e
  command matrix against the release binary, and the live macOS
  signing-identity check now run in the Release workflow's test job on every
  tag. The alpha train cuts releases at least as often as the nightly ran,
  so the proof now happens exactly when an artifact users get is produced.
- **`verify-public-install` now runs only where its claim is satisfiable.**
  The public post-finalize check is gated on the Latest actually moving;
  the private tag-push copy, which could never pass before promotion and
  produced a red run for every healthy prerelease, is removed.

### Fixed

- The QM removal in alpha.13 missed every hand-written prose surface; two
  independent release reviews caught them. ovm.sh's six pages, SECURITY.md's
  in-scope list, CONTRIBUTING.md, and the benchmark feed's product allowlist
  no longer reference QM, and stale release-process docs (a data-loss
  "blocker" already fixed in code, a contradictory crates.io decision, an
  outdated go-live baseline) are corrected.

## [0.0.3-alpha.13] - 2026-08-09

### Removed

- **QM is no longer a managed product.** Its CLI turned out to be a deployment
  control plane rather than a coding tool you launch: it has a handful of
  versions, no interactive surface, and its scaffolded deployment directories
  already pin their own CLI version via `package.json` and `npm exec`, so
  OVM's version management added little on top. `ovm install qm`, the `qm`
  launcher, the registry feed (`ovm.sh/api/qm.json`), and the nightly QM
  qualification lane are gone. A `~/.ovm/bin/qm` launcher left behind by
  alpha.12 now reports an unknown product; delete it with
  `rm ~/.ovm/bin/qm`. Existing `~/.ovm/products/qm` installs are untouched
  and can be removed the same way. No stable release ever included QM.

## [0.0.3-alpha.12] - 2026-08-08

<!-- The version baseline is managed by the release owner; see RELEASING.md.
     Do not bump versions in feature commits. -->

### Added

- **QM (`@yc-software/qm`) is now a managed product.** `ovm install qm`,
  `ovm use qm`, and a `~/.ovm/bin/qm` launcher work like any other product.
  QM is Y Combinator's multiplayer agent harness — a control-plane CLI rather
  than a coding agent — distributed only on npm and stored as an extracted
  bundle. Its entrypoint is a Node script, so OVM preflights the interpreter at
  install and launch and reports the required and found versions rather than
  failing on an opaque `env: node: not found`.
- QM defaults to `autoUpdate: notify` and, unlike the other products, does not
  inherit `autoUpdate.default`. A control-plane tool deploying infrastructure
  under a version the user never chose is a different risk from a coding agent
  picking up a patch release; opting in is explicit.
- The verification gate now refuses to run when the registry directory contains
  a product file it does not know how to gate. Previously such a file was
  indexed and deployed to the public API with no verdict of any kind, silently.

### Changed

- Bundle installs validate every required member, not just the entrypoint, so a
  bundle that unpacks without its manifest is no longer marked complete and then
  discovered broken at launch. Pi's historical releases (below ~0.80) predate
  `package.json` in the published bundle and are deliberately exempt — requiring
  it retroactively would have reclassified already-installed versions as
  archives.
- A Pi release's `SHA256SUMS` manifest is now fetched with up to three
  attempts. It is a sub-kilobyte idempotent request, and a single dropped
  connection used to cost the whole update: OVM correctly refuses to install a
  build it cannot verify, so one blip failed the install outright. Only an
  unreachable host is retried — a 404, a 403, or a damaged manifest is an
  answer, and is still refused immediately.
- A manifest fetch that fails because the host could not be reached now says
  so — "Could not reach github.com to verify Pi 0.84.1 … check your connection
  and run the command again" — instead of "refusing to install on length
  checks alone", which read as a problem with the release itself.

### Fixed

- Adopting a bundle product from an npm-global wrapper now prints the package
  manager cleanup command even when the old shim still wins `PATH`. OVM keeps
  the original install and tells the user to move OVM first before removing the
  fallback.
- Codex and Pi GitHub metadata requests can authenticate with an explicit
  `OVM_GITHUB_TOKEN`, avoiding the small anonymous per-IP quota on shared
  networks. Ambient `GITHUB_TOKEN` is ignored, and the credential is sent only
  to HTTPS `api.github.com`, never to custom metadata endpoints.
- An exhausted GitHub quota now says so, and names `OVM_GITHUB_TOKEN` as the
  remedy, on every path that can hit it. Previously a rate-limited metadata
  lookup was reported as `VersionNotFound` — telling users a release that
  plainly exists does not — a listing failure gave a bare `HTTP 403`, and
  release notes came back empty as though the release had none. A genuine 404
  still reports a missing version and still shows no notes; only quota
  refusals, which GitHub marks with `x-ratelimit-remaining: 0` or
  `retry-after`, are reported differently.
- `ovm update` no longer claims "Nothing else was changed." when one product
  fails and others succeeded. A partial run now closes with "Everything else
  was applied as shown above.", so the error stops contradicting the
  `1 updated, 1 failed` summary printed one line above it.

## [0.0.3-alpha.11] - 2026-08-06

_v0.0.3-alpha.10 was qualified privately but its public build was blocked by
a test-isolation bug (see Fixed below); alpha.11 is the same feature set plus
that fix and was the first of the pair to publish._

### Added

- The `ovm update` picker now offers pinned products instead of hiding them:
  a pinned update appears as an unticked `(pinned)` row, so taking it is a
  deliberate keypress rather than a side effect of "yes, all of them".
  Ticking it updates and resumes latest-tracking, exactly like
  `ovm update <product>`. `--check` lists pinned updates too; `--yes` and
  piped sweeps still skip pins entirely.
- The picker doubles as the auto-update settings screen: below the update
  rows, `ovm update` shows the launch-time auto-update policy for each
  product and for OVM itself. Space or ←/→ cycles `off / on / notify`; one
  enter applies the ticked updates and saves any policy changes, esc
  abandons both.

### Fixed

- The `update_flow` test suite mocked only the GitHub releases API while
  Codex latest-resolution consults npm and the OVM registry first, so tests
  reached the real network and began failing (and downloading real releases)
  the day upstream shipped a newer Codex. Both fast paths now dead-end at a
  connection-refused port in tests, making the suite hermetic.

## [0.0.3-alpha.9] - 2026-08-05

### Added

- `ovm version` — OVM's own version plus, for every managed product, the active
  version, how many are installed, and anything that would stop it moving (a
  pin, or a local `dev:` build). Local state only, so it answers offline and
  instantly. `ovm --version` still prints the bare version for scripts.
- `ovm update` now checks first and asks. It resolves every product before
  touching any of them, prints what will not change (already latest, pinned,
  dev build, not installed), then offers the available updates in a picker with
  everything pre-selected — space to toggle, `a`/`n` for all/none, enter to
  apply, esc to cancel. `--check` reports and installs nothing.
- `ovm update --yes` applies every available update without asking. The prompt
  only appears when both stdin and stdout are terminals, so piped and CI
  invocations keep sweeping exactly as before.

## [0.0.3-alpha.8] - 2026-08-03

### Added

- `ovm update` — a verb that actually updates. There was previously no way to
  say "update my tools now": `autoupdate` only configured whether it happened
  on launch. `ovm update` moves every installed product to its latest release,
  `ovm update <product>` moves one, and `ovm update self` updates OVM itself
  (the same thing `ovm self update` does). It reuses the launch-time update
  path, so an explicit update and an `autoUpdate: on` launch converge on the
  same on-disk state; the only difference is that "latest" is resolved upstream
  here, because you asked for it by name.
  - Nothing installed for a product is reported per product with the
    `ovm install` line to run — never a silent "up to date", and never an
    install you did not ask for.
  - Offline, the resolver says so and falls back to the newest release already
    in your store; a product it cannot resolve at all is reported as failed and
    the command exits non-zero.
  - A pinned product is reported and left alone by a bare `ovm update`;
    `ovm update <product>` overrides the pin and resumes latest-tracking.
    `dev:` builds have no upstream and are always left alone.
- `ovm help launch` — the full launch-shortcut table, moved out of `ovm help`.

### Changed

- The launch-time setting moved under the new verb as
  `ovm update auto [product|self] [on|off|notify]`. **`ovm autoupdate …` keeps
  working unchanged** as a hidden alias, so existing scripts, docs and muscle
  memory are unaffected.
- A bare `ovm` now shows six command lines instead of ~25 commands, 11 launch
  shortcuts and 18 examples. Everything omitted is one `ovm help` away, and a
  test enforces the ceiling.
- `ovm help` teaches the `y` = yolo / `f` = fast shortcut pattern with two
  examples instead of printing the eight-row `ccy/cxy/cxf/cxyf/ccx…` matrix,
  and groups `ls`/`current`/`which`/`info`/`stats` under one **Inspect**
  heading. No shortcut or command was removed.

### Security

- `ovm adopt` now verifies what it publishes, not what it looked at: the
  local binary is captured into the install transaction's staging area first,
  and the publisher signature check (the same macOS `codesign` team-ID
  verification downloads pass) plus a `--version` re-check run on that staged
  copy before it is published. A file swapped mid-adopt — an ordinary
  `brew upgrade` racing the adopt looks exactly like this — is rejected
  instead of being mislabeled. On macOS this means an unsigned or re-signed
  local build now fails adoption rather than importing.
- A Pi release that publishes a `SHA256SUMS` manifest which cannot be fetched
  or parsed (rate limit, HTTP 500, malformed content) now refuses to install
  instead of quietly falling back to length-only verification. Releases that
  genuinely publish no checksum still install with length checks, stated as
  such.
- Adoption and dev installs are now bound to the file they validated, not to
  a path name: the source is resolved once, opened without following a final
  symlink, proven by identity (not by path) to be the file its name claimed,
  and every later step — the containment check, the identity comparison, the
  copy itself — reads that open descriptor. A rename, retarget, or link swap
  after validation can no longer change what gets published, and a source
  that *is* a file the install transaction would delete is refused before
  anything is removed. Previously, `ovm adopt` pointed at a binary inside an
  incomplete managed install tree deleted the user's file, then reported the
  original was left untouched.
- Every file an install transaction writes into its freshly prepared tree —
  copies, markers, metadata, manifests, and all four products' download
  destinations — is now opened create-only (`O_CREAT|O_EXCL`), and
  permissions are set through the file handle rather than the path. A link
  standing at any of those destinations now fails the install cleanly instead
  of redirecting the write onto whatever the link points at.

### Fixed

- `checkForUpdates: false` now means no update checks anywhere: the launch
  auto-update, the `notify` prompt and the update banner all stop consulting
  the cached latest (which other commands keep writing), so a plain launch can
  no longer download a release the cache learned before checks were turned
  off. Explicitly named versions and switches to already-installed releases
  are unaffected.
- `ovm adopt` refuses a path that is already inside OVM's own store, with the
  repair commands spelled out (`ovm install` to finish an incomplete install,
  `ovm use` to select a complete one) — adopting brings an *outside* install
  under management.
- The registry refresh defends itself against damaged upstream answers: a
  version list must be a real list of plausible version strings, no single
  refresh may retire more than a small bounded fraction of the published
  registry (a truncated listing used to be able to retire 471 of 472 Claude
  versions in one write; the override that permits a genuine mass removal
  must name the product it covers), dates must parse as calendar dates or the
  field is omitted, and dist tags may only advertise versions the registry
  actually publishes.

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
[Unreleased]: https://github.com/ovm-sh/ovm-oss/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/ovm-sh/ovm-oss/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/ovm-sh/ovm-oss/compare/v0.1.1-alpha.3...v0.1.1
[0.1.1-alpha.3]: https://github.com/ovm-sh/ovm-oss/compare/v0.1.1-alpha.2...v0.1.1-alpha.3
[0.1.1-alpha.2]: https://github.com/ovm-sh/ovm-oss/compare/v0.1.1-alpha.1...v0.1.1-alpha.2
[0.1.1-alpha.1]: https://github.com/ovm-sh/ovm-oss/compare/v0.1.0...v0.1.1-alpha.1
[0.1.0]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.14...v0.1.0
[0.0.3-alpha.14]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.13...v0.0.3-alpha.14
[0.0.3-alpha.13]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.12...v0.0.3-alpha.13
[0.0.3-alpha.12]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.11...v0.0.3-alpha.12
[0.0.3-alpha.11]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.10...v0.0.3-alpha.11
[0.0.3-alpha.10]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.9...v0.0.3-alpha.10
[0.0.3-alpha.9]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.8...v0.0.3-alpha.9
[0.0.3-alpha.8]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.7...v0.0.3-alpha.8
[0.0.3-alpha.7]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.6...v0.0.3-alpha.7
[0.0.3-alpha.6]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.5...v0.0.3-alpha.6
[0.0.3-alpha.5]: https://github.com/ovm-sh/ovm-oss/compare/v0.0.3-alpha.4...v0.0.3-alpha.5
[0.0.3-alpha.4]: https://github.com/ovm-sh/ovm-oss/releases/tag/v0.0.3-alpha.4
