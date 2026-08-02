# OVM

**Open Version Manager for AI coding tools** — install, switch, and launch multiple versions of **Claude Code**, **Codex**, and **Pi** side by side. Like `nvm` or `rbenv`, but for the CLIs you code with every day.

```console
$ ovm select codex          # browse every version, switch instantly
$ ovm cx                    # launch the active one
$ ovm use codex rust-v0.130.0   # a bad release shipped? drop back in one second
```

- 🔒 **Pin and roll back** — a bad upstream release can't strand you.
- ⚡ **Switch instantly** — releases, alphas, and your own dev builds, side by side.
- 🛡️ **Only versions that provably run** — every release is install-tested and driven through a real tool-use probe before OVM offers it to you.

**macOS** is manually tested and supported. **Linux** is covered by CI. Windows is not supported.

---

## Install

```bash
curl -fsSL https://ovm.sh/install | sh
```

The installer adds `~/.ovm/bin` to your shell config and tells you which files
it changed. Open a new terminal, or run the `export` line it prints, and
`ovm --version` should work.

The installer and the [GitHub release bundles](https://github.com/ovm-sh/ovm-oss/releases) are the supported channels. npm, Homebrew, and crates.io are prepared but **not published** — don't use package-manager instructions yet.

## Quick start

```bash
ovm select                   # interactive: pick any product, any version
ovm select claude            # jump straight to Claude versions
ovm install codex latest     # install without switching
ovm use claude 2.1.91        # switch to an installed version
ovm ls codex --all           # installed + available
ovm current                  # what's active, everything at a glance

ovm cc / cx / pi             # launch the active version
ovm ccy / cxy                # …in yolo mode
ovm adopt codex              # take over an existing install without deleting it
```

## Why OVM exists

AI coding tools ship *fast* — sometimes several releases a week — and good releases arrive mixed in with the occasional regression. When you depend on these tools for real work, "just take the latest" isn't always safe.

The trigger was a Claude Code build that started killing Bash commands mid-run. With the published version broken and no easy way back, the only fix was pinning to a known-good version and rolling forward on your own schedule. Once versions were pinnable, the rest followed: switching Codex versions the same way, juggling your own local dev builds, and knowing when an upgrade is *breaking* rather than merely newer.

That last part isn't hypothetical either. Codex 0.144.0 installed fine, `--version` worked, `login` worked — and every real tool call died. A version manager that reports "installed, success" would have shipped it to everyone. So OVM verifies that a release **runs** before offering it.

## How it works

One design choice drives everything: **versions are immutable directories, and the active one is a pointer.**

```
~/.ovm/bin/codex → ovm            # launcher on your PATH
                     ↓
   products/codex/current  ──→  versions/rust-v0.145.0/…   ← active
                               versions/rust-v0.130.0/…   ← still here, one flip away
                               versions/dev:my-fix/…      ← your build, same treatment
```

Switching rewrites one symlink — nothing is downloaded, deleted, or rebuilt, which is why rollback always works. Installs are staged, integrity-checked — every download is verified against the digest its publisher ships wherever one exists (Claude Code's release manifest, npm SRI, Pi's `SHA256SUMS`; OpenAI publishes none for the Codex assets we fetch), and always against the byte length the server and release metadata declare, so a truncated transfer can never be mistaken for a good one; macOS binaries additionally get a code-signing team check — and published atomically with a completion marker written last, so an interrupted install is invisible rather than half-usable. Launches read local state only: the "is there a newer version?" check is answered from a local cache that a detached background process refreshes, so a wedged network can never delay the tool you're launching. The only launch that downloads anything is one that has an upgrade to apply — or that you asked to change version.

📖 **[Full technical overview with diagrams →](https://ovm.sh/how-it-works.html)** — storage, switching, the atomic self-update, and the release pipeline.
🔬 **[Methodology →](https://ovm.sh/methodology.html)** — how releases are verified, benchmarked, and published.

## Products

| Product | Aliases | Source | Binary |
|---|---|---|---|
| Claude Code | `claude`, `cc` | npm `@anthropic-ai/claude-code` (native bin via GCS) | `claude` |
| Codex | `codex`, `cx` | npm `@openai/codex` + GitHub Releases `openai/codex` | `codex` |
| Pi | `pi` | GitHub Releases `earendil-works/pi` | `pi` |

Codex's upstream feed also carries internal build tags; OVM lists only `rust-v…` releases that ship Codex binaries.

## Commands

**Versions**

| Command | Does |
|---|---|
| `ovm select [product] [version]` | Interactive picker — browse, install, switch |
| `ovm use <product> <version>` | Switch to an installed version |
| `ovm install <product> <version>` | Install without switching (`latest` works) |
| `ovm adopt <product> [path]` | Adopt an existing install without deleting it |
| `ovm uninstall <product> <version>` | Remove |
| `ovm update [product]` | Update to the latest release **now** (everything, or one product) |

**Inspect**

| Command | Does |
|---|---|
| `ovm ls <product>` | Installed versions (`--remote`, `--all`) |
| `ovm current [product]` · `ovm which [product]` | Active version · path to its binary |
| `ovm info <product> [version]` | Release notes |
| `ovm stats` | Counts and disk usage per product |

**Maintenance**

| Command | Does |
|---|---|
| `ovm update auto [product] [on\|off\|notify]` | Launch update policy (`self` for OVM itself) |
| `ovm doctor <product> [--fix]` | Check, and optionally repair, install hygiene |
| `ovm cleanup [30\|60\|never]` · `ovm clean` · `ovm archive` | Retention and disk |
| `ovm shortcuts` · `ovm completions <shell>` | Bare `ccy`/`cxy`/`claudex` commands · shell completions |

`ovm autoupdate …` is still accepted as a hidden alias for `ovm update auto …`, so existing scripts keep working.

**What `ovm update` does at the edges** — nothing installed for a product: says so per product and prints the `ovm install` line, rather than reporting a clean "up to date" or starting an install you did not ask for. Offline: it says it could not reach the update service and falls back to the newest release already in your store; a product it cannot resolve at all is reported as failed and the command exits non-zero. Pinned (`ovm use <product> <version>`): a bare `ovm update` reports the pin and leaves it alone, while `ovm update <product>` overrides it and resumes latest-tracking. `dev:` builds have no upstream and are always left alone.

**Launch shortcuts** — `cc` / `cx` / `pi` launch the active version, `ccx` launches claudex. Suffixes stack: `y` = yolo, `f` = fast (priority tier) — so `cxy` is Codex in yolo mode and `ccxyf` is claudex in yolo + fast. Full table: `ovm help launch`.

**Turning update checks off** — `"checkForUpdates": false` in `~/.ovm/config.json` stops OVM checking for new versions entirely: no background refresh, no update banner, no `notify` prompt, and no auto-update download — including from a version cache that was filled earlier. Updates you ask for by name (`ovm cx latest`, `ovm install …`, `ovm update`) still resolve and install — the setting turns off *automatic* checking, not the command you just typed — and `autoUpdate: on` will still move to a newer release **already in your version store**, since that is a local switch rather than a check.

## Updating OVM itself

OVM manages itself the same way it manages products — immutable snapshots and a pointer — with an activation probe that rolls back automatically if a new version can't run.

```bash
ovm self update              # update now (follows the configured channel)
ovm update self              # …same thing, from the `update` verb
ovm self list                # installed OVM snapshots
ovm self rollback            # back to the previous one
ovm self channel alpha       # opt into prereleases (default: stable)
ovm update auto self notify  # ask before updating, instead of doing it silently
```

By default (`on`) an update is **staged in the background** and activated atomically at the start of your next command, which prints `↑ OVM <new> (was <old>)`. Nothing is ever downloaded on the path between you and a launch. If the control plane is ever damaged: `~/.ovm/self/control-previous self repair-control`.

## Keep OVM authoritative

OVM owns the active binary through `~/.ovm/bin` symlinks. The one thing to avoid is letting a tool's **own** auto-updater run alongside it — then two managers fight over the same binary and "active version" stops meaning anything.

For Claude Code the trap is the **native install method**. If `~/.claude.json` has `"installMethod": "native"`, Claude re-downloads versions into `~/.local/share/claude/` and repoints `~/.local/bin/claude` out from under OVM. Setting `"autoUpdates": false` alone does **not** stop it — the install method is the trigger.

```bash
ovm doctor claude --fix      # flips installMethod off native, removes the strays
```

Don't run `claude install` to "repair" install-method warnings — that re-establishes the competing native install. If Claude's `/doctor` reports a native-vs-global mismatch, it's cosmetic: OVM is the real source of the running binary.

> **Bonus gotcha:** Claude Code auto-deletes session transcripts older than `cleanupPeriodDays` in `~/.claude/settings.json` — **30 days** by default. If you like keeping history, raise it: `{ "cleanupPeriodDays": 3650 }`.

## claudex — Claude Code on GPT-5.6

Runs the Claude Code interface against OpenAI models over your own ChatGPT/Codex subscription, via a local translating proxy. It productizes a [recipe shared by OpenAI's Codex lead](https://x.com/thsottiaux/status/2076119366647894371) — unofficial, use at your own risk.

```bash
ovm claudex setup            # one-time: proxy, Codex OAuth, isolated Claude home
claudex                      # Claude Code, thinking in GPT-5.6 Sol
ccxy                         # …in yolo mode
```

Your prompts go **Claude Code UI → local CLIProxyAPI on `127.0.0.1` → OpenAI**. Nothing goes to Anthropic in a claudex session, Anthropic credentials are scrubbed from the child process, and claudex gets its own isolated Claude home so its sessions never mix with your normal history. Manage it with `ovm claudex doctor | update | stop | uninstall [--purge]`.

## The picker

`ovm select` is a TUI: arrows or `j`/`k` to move, `enter` to select, `i` for release notes, `d` to download or delete, `b` to filter to companion (`/buddy`, `/pet`) versions, `r` to toggle real-vs-all Codex releases, `esc` to go back. Each row shows the release date and companion support.

## Version registry

Version lists come from `ovm.sh/api/` (`claude.json`, `codex.json`, `pi.json`, `cliproxyapi.json`, `registry.json`) in a single request, so listing hundreds of versions takes milliseconds instead of hammering upstream APIs. If the registry is unreachable, OVM falls back to direct upstream calls. Refreshes happen in the background, never on the launch path.

## Development

```bash
./scripts/dev-install.sh     # build + install a standalone dev snapshot
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test
```

`dev-install.sh` installs a content-addressed snapshot, so the installed commands keep working even if the checkout moves. Rerun it after changes — rebuilding alone doesn't refresh the installed snapshot. See [CONTRIBUTING.md](CONTRIBUTING.md) and [docs/architecture.md](docs/architecture.md); release process in [RELEASING.md](RELEASING.md).

## Roadmap

- **Custom products** — a declarative plugin system (`~/.ovm/products.d/*.toml`) so new tools don't need a PR.
- **Shell auto-switching** — optional `.ovmrc`-style per-directory pinning.

## License

MIT
