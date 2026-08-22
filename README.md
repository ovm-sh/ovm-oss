# OVM

**Open Version Manager for AI coding tools** — install, switch, and launch multiple versions of **Claude Code**, **Codex**, and **Pi** side by side. Like `nvm` or `rbenv`, but for the CLIs you code with every day.

```console
$ ovm select codex              # browse every version, switch instantly
$ ovm cx                        # launch the active one
$ ovm use codex rust-v0.130.0   # a bad release shipped? drop back in one second
```

- 🔒 **Pin and roll back** — a bad upstream release can't strand you.
- ⚡ **Switch instantly** — releases, alphas, and your own dev builds, side by side.
- 🛡️ **Evidence before promotion** — a release must pass its documented verification tier before OVM offers it.

**macOS** is manually tested and supported. **Linux** is covered by CI. Windows is not supported.

---

## Install

```bash
curl -fsSL https://ovm.sh/install | sh
```

The installer adds `~/.ovm/bin` to your shell config and tells you which files it changed. Open a new terminal and `ovm --version` should work.

The installer and the [GitHub release bundles](https://github.com/ovm-sh/ovm-oss/releases) are the supported channels. npm, Homebrew, and crates.io are prepared but **not published** — don't use package-manager instructions yet.

## Quick start

```bash
ovm hatch                    # first run: install Claude, Codex & claudex, hatch a companion
ovm select                   # interactive: pick any product, any version
ovm install codex latest     # install without switching
ovm use claude 2.1.91        # switch to an installed version
ovm ls codex --all           # installed + available
ovm current                  # what's active, everything at a glance

ovm cc / cx / pi             # launch the active version
ovm ccy / cxy                # …in yolo mode
ovm adopt codex              # take over an existing install without deleting it
```

## Why OVM exists

Claude Code 2.1.80 shipped `/buddy`: you typed it, and a companion hatched — a name, a rarity, an ASCII creature that rode along in your terminal. Thirteen releases later, 2.1.97 removed it, with nothing in the changelog. 2.1.96 is the last release that can still hatch one, and running 2.1.96 today means not taking whatever the installer hands you. That's the whole idea. (`ovm hatch` still hatches you one.)

Everything else followed. Once versions are pinnable they are also *comparable* — which release got slower, which one quietly changed behavior, which upgrade is breaking rather than merely newer. AI coding tools ship several releases a week; nobody is reading all of them for you.

## How it works

One design choice drives everything: **versions are immutable directories, and the active one is a pointer.**

```
~/.ovm/bin/codex → ovm            # launcher on your PATH
                     ↓
   products/codex/current  ──→  versions/rust-v0.145.0/…   ← active
                               versions/rust-v0.130.0/…   ← still here, one flip away
                               versions/dev:my-fix/…      ← your build, same treatment
```

**Switching** rewrites one symlink — nothing is downloaded, deleted, or rebuilt, which is why rollback always works.

**Installing** stages, verifies, then publishes atomically with a completion marker written last, so an interrupted install is invisible rather than half-usable. Every download is checked against the digest its publisher ships where one exists (Claude Code's release manifest, npm SRI, Pi's `SHA256SUMS`), and always against the byte length the server and release metadata declare. macOS binaries additionally get a code-signing team check.

**Verifying** goes deeper than "the binary runs", because that check once lied to us: Codex 0.144.0 moved shell execution into a sidecar binary our installer didn't know to fetch, so `--version` and `login` both worked while every tool call died at the first shell command. Codex was fine; our install of it wasn't. Coding agents now get tool-use probes at the product's documented tier, not an exit code.

**Launching** reads local state only. "Is there a newer version?" is answered from a cache that a detached background process refreshes, so a wedged network can never delay the tool you're launching.

📖 **[Full technical overview with diagrams →](https://ovm.sh/how-it-works.html)** — storage, switching, the atomic self-update, and the release pipeline.
🔬 **[Methodology →](https://ovm.sh/methodology.html)** — how releases are verified, benchmarked, and published.

## Products

| Product | Aliases | Source | Binary |
|---|---|---|---|
| Claude Code | `claude`, `cc` | npm `@anthropic-ai/claude-code` (native bin via GCS) | `claude` |
| Codex | `codex`, `cx` | npm `@openai/codex` + GitHub Releases `openai/codex` | `codex` |
| Pi | `pi` | GitHub Releases `earendil-works/pi` | `pi` |

Codex's upstream feed also carries internal build tags; OVM lists only `rust-v…` releases that ship Codex binaries.

GitHub permits only a small anonymous API quota per shared IP. If Codex or Pi metadata requests are rate-limited, set `OVM_GITHUB_TOKEN`. OVM ignores ambient `GITHUB_TOKEN` and sends the explicit token only to `https://api.github.com`, never to product URL overrides.

## Commands

**Versions**

| Command | Does |
|---|---|
| `ovm select [product] [version]` | Interactive picker — browse, install, switch |
| `ovm use <product> <version>` | Switch to an installed version |
| `ovm install <product> <version>` | Install without switching (`latest` works) |
| `ovm adopt <product> [path]` | Adopt an existing install without deleting it |
| `ovm uninstall <product> <version>` | Remove |
| `ovm update [product]` | Update to the latest release **now** |

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
| `ovm statusline` | Put Echo in the Claude Code statusline |

**Launch shortcuts** — `cc` / `cx` / `pi` launch the active version, `ccx` launches claudex. Suffixes stack: `y` = yolo, `f` = fast (priority tier) — so `cxy` is Codex in yolo mode, `ccxyf` is claudex in yolo + fast. Full table: `ovm help launch`.

**What `ovm update` does at the edges**

- Nothing installed: says so per product and prints the `ovm install` line, rather than reporting a clean "up to date".
- Offline: falls back to the newest release already in your store; a product it cannot resolve at all fails the command.
- Pinned: a bare `ovm update` reports the pin and leaves it alone; `ovm update <product>` overrides it and resumes latest-tracking.
- `dev:` builds have no upstream and are always left alone.

**Turning update checks off** — `"checkForUpdates": false` in `~/.ovm/config.json` stops all automatic checking: no background refresh, no banner, no `notify` prompt, no auto-download. Updates you ask for by name still work — the setting turns off *automatic* checking, not the command you just typed.

## Updating OVM itself

OVM manages itself the way it manages products — immutable snapshots and a pointer — with an activation probe that rolls back automatically if a new version can't run.

```bash
ovm self update              # update now (follows the configured channel)
ovm self list                # installed OVM snapshots
ovm self rollback            # back to the previous one
ovm self channel alpha       # opt into prereleases (default: stable)
ovm update auto self notify  # ask before updating, instead of doing it silently
ovm self uninstall           # remove OVM (keeps your installed products)
```

By default an update is **staged in the background** and activated atomically at the start of your next command, which prints `↑ OVM <new> (was <old>)`. Nothing is ever downloaded on the path between you and a launch.

**Uninstalling** reverses the installer: the `# >>> ovm >>>` PATH block, the launchers in `~/.ovm/bin`, and OVM's own snapshots. Your installed versions and `~/.ovm/config.json` are **kept**; `--purge` removes the whole `~/.ovm` tree. It asks for a typed confirmation.

## Keep OVM authoritative

OVM owns the active binary through `~/.ovm/bin` symlinks. The one thing to avoid is letting a tool's **own** auto-updater run alongside it — then two managers fight over the same binary and "active version" stops meaning anything.

For Claude Code the trap is the **native install method**. If `~/.claude.json` has `"installMethod": "native"`, Claude re-downloads versions into `~/.local/share/claude/` and repoints `~/.local/bin/claude` out from under OVM. Setting `"autoUpdates": false` alone does **not** stop it — the install method is the trigger.

```bash
ovm doctor claude --fix      # flips installMethod off native, removes the strays
```

Don't run `claude install` to "repair" install-method warnings — that re-establishes the competing native install.

> **Bonus gotcha:** Claude Code auto-deletes session transcripts older than `cleanupPeriodDays` in `~/.claude/settings.json` — **30 days** by default. If you like keeping history, raise it: `{ "cleanupPeriodDays": 3650 }`.

## claudex — Claude Code on GPT-5.6

Runs the Claude Code interface against OpenAI models over your own ChatGPT/Codex subscription, via a local translating proxy. It productizes a [recipe shared by OpenAI's Codex lead](https://x.com/thsottiaux/status/2076119366647894371) — unofficial, use at your own risk.

```bash
ovm claudex setup            # one-time: proxy, Codex OAuth, isolated Claude home
claudex                      # Claude Code, thinking in GPT-5.6 Sol
ccxy                         # …in yolo mode
```

Your prompts go **Claude Code UI → local CLIProxyAPI on `127.0.0.1` → OpenAI**. Nothing goes to Anthropic in a claudex session, Anthropic credentials are scrubbed from the child process, and claudex gets its own isolated Claude home. Manage it with `ovm claudex doctor | update | stop | uninstall`.

## The picker

`ovm select` is a TUI: arrows or `j`/`k` to move, `enter` to select, `i` for release notes, `d` to download or delete, `b` to filter to companion (`/buddy`, `/pet`) versions, `r` to toggle real-vs-all Codex releases, `esc` to go back.

## Version registry

Version lists come from `ovm.sh/api/`, so listing hundreds of versions takes milliseconds instead of hammering upstream APIs. A detached worker conditionally checks a ~1 KB aggregate index whose ETag makes the common case a bodyless `304`; only changed products trigger a full download. The foreground launch never waits on the network.

## Your own builds

OVM runs your patched Codex or Pi beside the official releases. A local build imports as a **dev version** — switchable and launchable like any other, and never touched by auto-update.

```bash
git clone git@github.com:<you>/codex.git && cd codex/codex-rs
cargo build --release
ovm install codex dev --dev mypatch --binary target/release/codex
ovm use codex dev:mypatch            # or one-off: ovm cx --ovm-version dev:mypatch
```

Re-import the same label to refresh it after a rebuild, or pass `--link` to point at the build instead of copying it — then every rebuild is immediately what `dev:mypatch` runs, at the cost of breaking if the checkout moves. Pi ships as a bundle, so import it with `--bundle`. Claude Code isn't open source, so there is nothing to fork.

Codex 0.144.0 and later spawn a `codex-code-mode-host` sidecar for shell commands. Build that target too and import both with `--bundle <dir>`, or your dev version will fail every shell command while looking perfectly installed.

Full loop, including how to keep a fork current: [docs/fork-build-import.md](docs/fork-build-import.md).

## Development

```bash
./scripts/dev-install.sh     # build + install a standalone dev snapshot
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test
```

`dev-install.sh` installs a content-addressed snapshot, so the installed commands keep working even if the checkout moves. Rerun it after changes — rebuilding alone doesn't refresh the snapshot, and validating through the installed `ovm` (not `./target/debug/ovm`) is what exercises the real control plane and bundled plugins.

See [CONTRIBUTING.md](CONTRIBUTING.md), [docs/architecture.md](docs/architecture.md), and [RELEASING.md](RELEASING.md).

## Roadmap

- **Custom products** — a declarative plugin system (`~/.ovm/products.d/*.toml`) so new tools don't need a PR.
- **Shell auto-switching** — optional `.ovmrc`-style per-directory pinning.

## License

MIT

```
                     *########******++
               ###################******+++:
            #@@@@@@@@@@@@@@@#########*****++++:
         #@@@@@@@@@@@@@@@@@@@@########******+++:::
       @@@@@@@@@@@@@@@@@@@@@@@@########******++++::.
     #@@@@@@@@@@@@@@@@@@@@@@@@@#########*****+++++::..
    @@@@@@@@@@@@@@@@@@@@@@@@@@@@########******++++:::..
  #@@@@██████@@████████@@███@@@@####████████**██████::...
  @@@@@██@@██@@@@@██@@@@@███@@@#####███#*███**██++::::...
 #@@@@@██████@@@@@██@@@@@███@@######████████*+██████::....
##@@@@@██@@██@@@@@██@@@@@███@#######███**███*+++++██::.....
##@@@@@██@@██@@@@@██@@@@@█████████##███**███++██████:......
##@@@@@@@@@@@@@@@@@@@@@@@@##########*******++++++::::......
###@@@@██████@@████████@#██████####*████████++██████.......
*####@@██@@@@@@███@@███##███###███**███**+++++██:::........
 *#####██##@@@#███##███##███###███**████████+:██████......
  *####██######███##███##███***███**███+++++:::::.██.....
  +**##██████##████████##██████****+████████::██████.....
    *****##########*************++++++++:::::..........
     +***********************++++++++::::::...........
       +++**************++++++++++:::::::...........
         :+++++++++++++++++++++:::::::............
            :::+++++++++::::::::::.............
               .::::::::::::................
                     .................
```

<sub>Built by **Atlas Codes** — with mochi, echo and quelpaw. Run `ovm hatch` and it'll tell you why.</sub>
