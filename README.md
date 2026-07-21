# OVM

**Open Version Manager for AI coding tools.**

Install, switch, and launch multiple versions of **Claude Code**, **Codex**, and **Pi** side-by-side — the way `nvm` or `rbenv` does for languages, but for the fast-moving CLIs you code with every day.

```bash
ovm select claude        # browse and switch Claude Code versions
ovm cc                    # launch the active version
```

## Why OVM?

AI coding tools ship fast — sometimes several releases a week — and the good releases arrive mixed in with the occasional regression. When you depend on these tools for real work, "just take the latest" isn't always safe.

OVM started as a way to **survive regressions in closed-source releases**. The trigger was a Claude Code build that started improperly killing Bash commands mid-run: with the published version broken and no easy way back, the only fix was being able to pin to a known-good version and roll forward on your own schedule. Once versions were pinnable, two more uses fell out naturally:

- **Switching Codex versions** the same way — and even juggling **your own local dev builds**, so you can flip between a released version and something you're hacking on without reinstalling.
- **Tracking what changed between versions.** If you build apps on top of these tools' SDKs or CLI interfaces, you need to know when an upgrade is *breaking*, not just newer.

That last point isn't hypothetical. During a Codex migration we hit the exact failure mode OVM is meant to tame: multiple versions live at once, old and new builds interleaved, producing errors that should have stayed contained but didn't. It's a hazard for anyone who keeps long-lived sessions open across an upgrade — and a version manager that makes the active version explicit is the cleanest way to reason about it.

So OVM does three things:

1. **Pin & roll back** — never get stranded on a bad release.
2. **Switch instantly** — released versions, alphas, and your own local builds, side-by-side.
3. **Stay aware of change** — surface release dates and feature/companion support so upgrades aren't a leap of faith.

## Platforms

- **macOS** — supported by maintainers and manually tested
- **Linux** — supported in CI, not yet manually tested by the maintainer

Windows is not supported.

## Install

OVM is not live yet. The pre-live package baseline remains `v0.0.1`; do not
select a higher public version until the release owner explicitly approves it.
After the first release, the recommended cross-platform installation is the
verified direct installer:

```bash
curl -fsSL https://raw.githubusercontent.com/ovm-sh/ovm-oss/main/install.sh | sh
echo 'export PATH="$HOME/.ovm/bin:$PATH"' >> ~/.zshrc
```

The direct installer keeps immutable OVM versions under `~/.ovm/self/` and a
standalone control plane at `~/.ovm/bin/ovm`. Updating or rolling back changes
one active-version pointer, so the CLI and every manifest-declared side binary
switch together. The bundle manifest is dynamic; adding a future `ovm-*` side
binary does not require hard-coding another installer path. Marker-less bundles from
the retired direct installer have no trustworthy ownership record, so explicitly authorize
their one-time migration:

```bash
curl -fsSL https://raw.githubusercontent.com/ovm-sh/ovm-oss/main/install.sh \
  | env OVM_MIGRATE_LEGACY_DIRECT=1 sh
```

Alternative channels:

```bash
# macOS package-manager install
brew tap ovm-sh/ovm && brew install ovm

# npm prebuilt package
npm install -g @mochiexists/ovm

# Rust/source install — install every crate declared by the current bundle
cargo install ovm ovm-codex-skew ovm-claudex --locked
```

For checkout development, build a standalone content-addressed snapshot rather
than linking commands into the repository:

```bash
git clone https://github.com/ovm-sh/ovm-oss.git
cd ovm
./scripts/dev-install.sh
```

Rerun `./scripts/dev-install.sh` after code changes. The installed commands keep
working if the checkout is moved or removed. Run it before moving when possible;
if the old checkout was already moved, explicitly authorize its former root:

```bash
OVM_LEGACY_ROOT=/path/to/old/ovm ./scripts/dev-install.sh
```

### Updating and rolling back OVM

`ovm autoupdate` controls **Claude, Codex, and Pi** updates on launch — and, via
`ovm autoupdate self`, OVM itself. OVM's own launch updates default to **on**: a
launch stages the newer release in the background and activates it atomically at
the start of the next invocation (see [Version Registry](#version-registry) for
the three policies). Explicit self-management is always available:

```bash
ovm autoupdate self notify      # ask before updating OVM on launch (or `off`)
ovm self update                 # follows the configured channel (default stable)
ovm self channel alpha          # opt in: persist the alpha channel
ovm self channel                # show the current channel setting
ovm self update --channel alpha # one-shot alpha update (flag overrides config)
ovm self update --channel beta  # one-shot package-manager prerelease lane
ovm self current
ovm self list
ovm self use <version>
ovm self rollback
```

**Channels.** `stable` (the default) tracks GitHub's latest non-prerelease.
`alpha` tracks the highest-semver release *including* prereleases (tags like
`v0.2.0-alpha.3`); when the newest overall release is the latest stable, alpha
simply installs that. Set the channel persistently with `ovm self channel
<stable|alpha>` (stored as `self.channel` in `~/.ovm/config.json`), or override a
single run with `--channel`. Every alpha update keeps the same verification as
stable — repository pin, checksum, bundle extraction, activation probe, and
rollback — and the downgrade guard is prerelease-aware: switching from an alpha
back to stable while stable still trails is refused rather than silently
downgrading.

`ovm self-update` remains an alias for `ovm self update`. Direct installs support
version switching and rollback; Homebrew and Cargo updates remain owned by their
package managers. A direct update probes the new control plane and restores both
control and active version if activation fails. The emergency recovery command is:

```bash
~/.ovm/self/control-previous self repair-control
```

See [RELEASING.md](RELEASING.md) for the release checklist.

## Quick Start

```bash
# Interactive — pick any product, any version
ovm select

# Jump straight into Claude versions
ovm select claude

# Direct switch (prompts to install if missing)
ovm select claude 2.1.91

# Launch the active version
ovm cc                       # Claude
ovm cx                       # Codex
ovm pi                       # Pi
ovm ccy                      # Claude in yolo mode
ovm cxy                      # Codex in yolo mode

# Adopt an existing install without deleting it
ovm adopt codex              # install/switch the same Codex version under OVM
```

## Supported Products

| Product | Alias | Source | Binary |
|---------|-------|--------|--------|
| Claude Code | `cc` | npm `@anthropic-ai/claude-code` (native bin via GCS) | `claude` |
| Codex | `cx` | npm `@openai/codex` + GitHub Releases `openai/codex` | `codex` |
| Pi | — | GitHub Releases `earendil-works/pi` | `pi` |

## claudex — Claude Code on GPT-5.6

claudex runs the Claude Code UI with OpenAI's GPT-5.6 models as the brain, over
your own ChatGPT/Codex subscription. It productizes the recipe
[shared publicly by OpenAI's Codex lead](https://x.com/thsottiaux/status/2076119366647894371)
— an unofficial integration; use at your own risk.

```bash
ovm claudex setup   # one-time: proxy install, Codex OAuth, isolated Claude home
claudex             # Claude Code, thinking in GPT-5.6 Sol
ccxy                # same, in yolo mode
claudex --fast      # OpenAI priority service tier ("fast mode")
```

**Where your prompts go:** Claude Code (the UI you type into) → a local
[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) sidecar on
`127.0.0.1` (translates Anthropic's API shape to OpenAI's) → OpenAI's ChatGPT
backend, authenticated by a Codex OAuth grant that lives only in
`~/.ovm/claudex/`. Nothing goes to Anthropic in a claudex session, and your
Anthropic credentials are scrubbed from the child process.

**What stays separate:** claudex gets its own isolated Claude home
(`CLAUDE_CONFIG_DIR`), so its sessions never appear in your normal `claude`
history (and vice versa), its OAuth grant is independent of Codex CLI's, and
`/model` maps opus/sonnet/haiku to `gpt-5.6-sol/terra/luna` (plus `-fast`
priority-tier aliases). Project-level `CLAUDE.md` files are shared via the
working directory; your global `~/.claude/CLAUDE.md` is imported into
claudex's own instruction file.

**Operations:** the sidecar is OVM-managed (checksummed GitHub-release
download; safe launch-time auto-updates; `ovm claudex update` / rollback; a `pin` in
`~/.ovm/claudex/config.json` freezes a known-good Claude+proxy pair),
launches verify the proxy's identity with an authenticated probe before any
traffic flows. Updates download while sessions keep using the old proxy and
activate only after its shared session lease is idle. Logs live in
`~/.ovm/claudex/logs/`, `ovm claudex doctor`
diagnoses the whole chain, `ovm claudex stop` halts the sidecar, and
`ovm claudex uninstall [--purge]` removes it (fully, with `--purge`).
The first update from a pre-session-lock OVM build is staged conservatively;
after those older sessions exit, run `ovm claudex update` once to migrate the
daemon into guarded automatic activation.

## Using It Cleanly

OVM owns the active binary through its `~/.ovm/bin` symlinks — that's what makes switching instant. The one thing to avoid is letting a tool's **own** auto-updater run alongside OVM, because then two managers are fighting over the same binary and your "active version" stops meaning anything.

For Claude Code specifically, the trap is the **native install method**. If `~/.claude.json` has `"installMethod": "native"`, Claude treats `~/.local` as a native install it owns: it re-downloads versions into `~/.local/share/claude/` (hundreds of MB) and repoints `~/.local/bin/claude` out from under OVM. Setting `"autoUpdates": false` alone does **not** stop this — the `native` method is the trigger.

So to keep OVM authoritative:

- Keep `installMethod` as anything but `native` (e.g. `global`). Don't run `claude install` to "repair" install-method warnings — that re-establishes the native install that competes with OVM.
- If Claude's `/doctor` notes a "running native installation but config install method is 'global'" mismatch, that's **cosmetic** — OVM is the real source of the running binary, and the note triggers nothing.

OVM can check and fix all of this for you:

```bash
ovm doctor claude          # report install hygiene
ovm doctor claude --fix     # flip installMethod off native + remove the ~/.local strays
```

### Adopt an existing install

If you already installed one of the managed CLIs with Homebrew, npm, or a native
installer, use `adopt` to move control to OVM without deleting the original
install first:

```bash
ovm adopt codex
ovm adopt claude /opt/homebrew/bin/claude
```

`adopt` reads the existing binary's version, installs that same version through
OVM's normal trusted install path, switches to it, and verifies that
`~/.ovm/bin/<tool>` now wins on `PATH`. If Homebrew/npm/native still shadows
OVM, it tells you the exact `PATH` fix and does not suggest deleting the old
install yet.

Once takeover is confirmed, `adopt` prints the cleanup command when it can infer
the old manager, such as `brew uninstall ...`, `npm uninstall -g ...`, or
`ovm doctor claude --fix` for Claude's native install. Cleanup stays explicit so
you can keep the old install as a fallback until you're comfortable.

### Keep your session history

A thing many people don't realize: **Claude Code auto-deletes old session transcripts.** The `cleanupPeriodDays` setting in `~/.claude/settings.json` defaults to **30 days**, and sessions older than that are pruned at startup. If you like keeping your history around — which pairs naturally with OVM's "stay aware across versions" ethos — bump it up:

```json
{ "cleanupPeriodDays": 3650 }
```

There's no literal "off" switch (the value must be ≥ 1), but there's no documented maximum either, so a large number effectively disables pruning. Leave it at the default if you'd rather Claude keep tidying up for you — the point is just to know it's happening so it's your choice. (Codex keeps its own session history too; its retention/pruning behavior is something we haven't audited yet.)

## Commands

**Interactive:**
- `ovm select [product] [version]` — pick a version interactively (browse, install, switch)

**Version management:**
- `ovm use <product> <version>` — switch to an installed version
- `ovm adopt <product> [path]` — adopt an existing app install without deleting it
- `ovm install <product> <version>` — install without switching
- `ovm uninstall <product> <version>` — remove

**Query:**
- `ovm ls <product>` — installed versions (`--remote` for available, `--all` for both)
- `ovm current [product]` — active version (all products if none given)
- `ovm which [product]` — path to active binary (all products if none given)
- `ovm info <product> [version]` — release notes
- `ovm stats` — installed/archived counts and disk usage per product

**Maintenance:**
- `ovm clean <product> [version]` — free disk space
- `ovm cleanup [30|60|never]` — set automatic inactive install retention
- `ovm archive <product> [--below <ver>]` — archive old versions
- `ovm doctor <product> [--fix]` — check (and optionally repair) install hygiene
- `ovm autoupdate [on|off|notify]` — set the default launch update policy
- `ovm autoupdate <product> [on|off|notify]` — override one product
- `ovm autoupdate self [on|off|notify]` — control OVM's own launch updates (default `on`)

**Manage OVM itself (`ovm self`):**
- `ovm self current` — show the active OVM snapshot
- `ovm self list` — list installed OVM snapshots
- `ovm self use <version>` — switch OVM to an installed snapshot
- `ovm self update [--channel <stable|alpha|beta>]` — update OVM (alias: `ovm self-update`)
- `ovm self rollback` — return to the previous OVM
- `ovm self channel [stable|alpha]` — show or set the persistent update channel
- `ovm self repair-control` — emergency control-plane recovery

**claudex (Claude Code on GPT-5.6):**
- `ovm claudex setup` — one-time proxy install, Codex OAuth, isolated Claude home
- `ovm claudex doctor` — diagnose the whole proxy chain
- `ovm claudex update` — update the OVM-managed CLIProxyAPI sidecar (with rollback)
- `ovm claudex stop` — halt the running sidecar
- `ovm claudex uninstall [--purge]` — remove claudex (fully, with `--purge`)

**Other:**
- `ovm shortcuts` — install bare `ccy`/`cxy`/`ccx*`/`claudex` commands on PATH
- `ovm completions <shell>` — generate shell completions
- `ovm help [command]` — overview, or details on one command

**Launch shortcuts:**
- `ovm cc [args]`, `ovm cx [args]`, `ovm pi [args]` — run the active managed version
- `ovm ccy [args]`, `ovm cxy [args]` — run Claude/Codex in yolo mode
- `ovm cxf [args]`, `ovm cxyf [args]` — run Codex on the priority service tier (`f` = fast; `yf` = fast + yolo)
- `ovm ccx [args]`, `ovm ccxy`, `ovm ccxf`, `ovm ccxyf` — launch claudex (`y` = yolo, `f` = fast; suffixes stack)

Run `ovm help` for an overview of installed products and examples.

## Interactive Picker

`ovm select` opens a TUI with:
- Arrow keys / `j`/`k` to navigate
- `enter` to select (switches, or prompts to install)
- `i` to view release notes inline
- `d` to download an uninstalled version, or delete an installed inactive one
- `b` to filter to companion (buddy / pet) versions; `r` toggles real-vs-all Codex releases
- `esc` to go back (version picker → product picker → quit)

On the alpha channel (`ovm self channel alpha`, or the explicit
`advanced.selfInPicker` config flag) OVM itself appears as a selectable entry in
the `ovm select` product picker, opening a second-level list for switching
between installed OVM versions. Default stable users never see it.

Each version shows the release date and product-specific companion support: Claude versions show whether `/buddy` is available, and Codex versions show whether `/pet` is available. Codex `/pet` first appeared in source before it was available in an OVM-installable build; the first installable OVM release with assets is `rust-v0.131.0-alpha.16`, released 2026-05-14.

By default, OVM lists only official installable Codex releases (`rust-v...` tags with Codex binary assets). It filters out internal build tags and dependency release tags from the upstream GitHub repository. For `latest`, OVM prefers npm's stable `@openai/codex` dist-tag and uses npm platform tarballs as a fallback when GitHub's release API is unavailable.

## Version Registry

Version lists are served from `ovm.sh/api/`:

- `/claude.json` — all Claude Code versions with publish dates
- `/codex.json` — all Codex versions
- `/pi.json` — all Pi versions
- `/cliproxyapi.json` — CLIProxyAPI versions (the claudex proxy sidecar, not a directly installable product)
- `/registry.json` — product index

The registry is refreshed automatically. OVM fetches it in a single request instead of hitting upstream npm/GitHub APIs directly — so listing hundreds of Codex versions takes milliseconds instead of seconds.

If the registry is unreachable, OVM falls back to direct upstream calls.

Launches also opportunistically refresh the cached registry in the background for all products. `autoupdate` defaults to `on`: OVM resolves the latest release in the foreground, installs it if needed, switches the active version, and then launches. The three policies are:

- **on** — update to the latest release on launch (products install before exec).
- **off** — never auto-update; stay on the active version.
- **notify** — announce a newer version instead of updating. An interactive terminal gets a one-keypress prompt (`[i]nstall now, [s]nooze`) with a ~5s timeout that defaults to snooze; a non-interactive one prints a single notice. A snooze silences that exact version for three days, but a newer version re-announces immediately. Install-now applies the update just like `on`.

The same three policies apply to **OVM itself** via `ovm autoupdate self`, which defaults to **on**. Under `on`, a launch stages the newer OVM release in the background (download + checksum verify + immutable install) without touching the running version, then activates it atomically at the *start of the next invocation* — printing a single `↑ OVM <new> (was <old>)` line — reusing the direct self-updater's activation probe and rollback. This keeps the launch hot path network-free, and a failed check, download, or activation never breaks or delays a launch. Dev snapshots (`dev-<hash>`) are always exempt; the configured `self.channel` (stable/alpha) and `OVM_GITHUB_TOKEN` are honored. `notify` prompts instead of staging, and `off` disables launch-time self-updates entirely (explicit `ovm self update` still works).

`cleanup` defaults to `30`: inactive release installs older than 30 days are removed on launch; use `ovm cleanup 60` or `ovm cleanup never` to change it.

## Storage Layout

```
~/.ovm/
├── bin/                       # active-version symlinks (add to PATH)
├── config.json                # settings: channels, auto-update, cleanup
├── hooks/                     # pre/post install, switch, uninstall hooks
├── self/                      # OVM's own managed control plane
│   ├── versions/              # immutable OVM snapshots
│   └── current                # active OVM (the stable control-plane pointer)
├── claudex/                   # claudex home: isolated Claude config, OAuth, proxy, logs
└── products/
    ├── claude/versions/
    ├── codex/versions/
    └── pi/versions/
```

Each product keeps a `current` symlink pointing at the active version; `~/.ovm/bin/<binary>` resolves through it, so switching a version is a single symlink swap. OVM manages *itself* the same way: `~/.ovm/self/current` selects the active immutable OVM snapshot (with `control-previous` kept for emergency rollback), which is why updating or rolling back OVM is also a single-pointer swap.

## Development

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo publish -p ovm --dry-run
```

## Roadmap

- **Custom products (plugins)** — today, supporting a new product requires a PR. A declarative plugin system (`~/.ovm/products.d/*.toml`) will let users register their own tools without forking. Planned for a future release.
- **Shell auto-switching** — optional `.ovmrc`-style version pinning per directory.

## License

MIT
