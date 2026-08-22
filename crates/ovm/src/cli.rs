use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "ovm",
    version,
    about = "Manage Claude Code, Codex, and Pi versions"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

/// Whether `name` is owned by clap's built-in command tree (including aliases).
/// Plugin routing and help filtering both consult this declaration instead of
/// maintaining a second command-name list that can drift.
pub fn is_builtin_command(name: &str) -> bool {
    Cli::command().find_subcommand(name).is_some()
}

#[derive(Subcommand, Debug)]
pub enum SelfCommands {
    /// Download and activate a newer OVM release
    Update {
        /// Release channel (`stable`, `alpha`, or `beta`); overrides the
        /// persisted `self channel` setting for this run
        #[arg(long)]
        channel: Option<String>,

        /// Install method (`auto`, `direct`, `brew`, `cargo`, or `dev`)
        #[arg(long, default_value = "auto")]
        method: String,

        /// Show the planned update without changing files
        #[arg(long)]
        dry_run: bool,
    },

    /// Show or set the persistent self-update channel (`stable` or `alpha`)
    Channel {
        /// Channel to persist (`stable` or `alpha`); omit to show the current setting
        channel: Option<String>,
    },

    /// Show the active self-managed OVM version
    Current,

    /// List installed self-managed OVM versions
    #[command(alias = "ls")]
    List,

    /// Atomically activate an installed OVM version
    Use { version: String },

    /// Atomically return to the previously active OVM version
    Rollback,

    /// Restore the previous standalone control-plane executable
    RepairControl,

    /// Remove OVM itself: the installer's shell PATH block, the `~/.ovm/bin`
    /// launchers, and OVM's own snapshots
    #[command(
        long_about = "Remove OVM itself — the reverse of the direct installer.\n\
        \n\
        Removes the `# >>> ovm >>>` PATH block the installer appended to your shell \
        profiles (nothing else in those files is touched), the launchers and side-binary \
        shims in ~/.ovm/bin, and OVM's own snapshots under ~/.ovm/self.\n\
        \n\
        Installed Claude/Codex/Pi versions and your config are KEPT, so a reinstall \
        picks up where you left off. `--purge` removes the whole ~/.ovm tree instead.\n\
        \n\
        Asks for a typed confirmation; `--yes` skips it. A non-interactive shell \
        without `--yes` refuses rather than assuming consent."
    )]
    Uninstall {
        /// Also remove every managed product install and OVM's config (the whole ~/.ovm tree)
        #[arg(long)]
        purge: bool,

        /// Skip the typed confirmation
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Install a managed version
    Install {
        /// Product name or alias
        product: String,

        /// Version to install
        version: String,

        /// Install Claude from npm instead of the native binary
        #[arg(long)]
        npm: bool,

        /// Install a local dev build under the version name dev:<label>
        #[arg(long)]
        dev: Option<String>,

        /// Install a dev build from a standalone binary path
        #[arg(long, conflicts_with = "bundle")]
        binary: Option<PathBuf>,

        /// Install a dev build from a bundle directory containing the product binary
        #[arg(long)]
        bundle: Option<PathBuf>,

        /// Keep the dev install linked to the local source instead of copying it
        #[arg(long)]
        link: bool,
    },

    /// Switch to an installed version
    Use {
        /// Product name or alias
        product: String,

        /// Version to activate
        version: String,
    },

    /// Adopt an existing app install into OVM without deleting the original
    Adopt {
        /// Product name or alias
        product: String,

        /// Existing binary path (default: first non-OVM binary on PATH)
        path: Option<PathBuf>,
    },

    /// List versions (installed by default; use --remote or --all to include available)
    #[command(alias = "ls")]
    List {
        /// Product name or alias
        product: String,

        /// Show versions available to install (remote) instead of installed
        #[arg(long, conflicts_with = "all")]
        remote: bool,

        /// Show all versions (installed + available)
        #[arg(long)]
        all: bool,
    },

    /// Show the active version (all products if no product given)
    Current {
        /// Product name or alias (optional — omit for full status)
        product: Option<String>,
    },

    /// Show OVM's version and the version of every product it manages
    #[command(
        long_about = "Show OVM's own version and, for every managed product, the active \
        version, how many are installed, and anything that would stop it moving (a pin, or \
        a local dev build).\n\
        \n\
        Reads local state only — no network, so it is always instant and works offline.\n\
        \n\
        \x20 ovm --version    just OVM's version, for scripts\n\
        \x20 ovm current      the active version alone, one line per product\n\
        \x20 ovm list <p>     every installed version of one product"
    )]
    Version,

    /// Show the path to the active binary (all products if no product given)
    Which {
        /// Product name or alias (optional — omit for all products)
        product: Option<String>,
    },

    /// Uninstall a version
    Uninstall {
        /// Product name or alias
        product: String,

        /// Version to uninstall
        #[arg(required_unless_present = "all")]
        version: Option<String>,

        /// Remove every installed version, the active one included, and clear
        /// the selection — the way to fully leave a product
        #[arg(long, conflicts_with = "version")]
        all: bool,

        /// Skip the typed confirmation for --all
        #[arg(long, short = 'y', requires = "all")]
        yes: bool,
    },

    /// Clean raw archives to save disk space
    Clean {
        /// Product name or alias
        product: String,

        /// Optional version to clean
        version: Option<String>,

        /// Clean all versions for the selected product
        #[arg(long)]
        all: bool,
    },

    /// Configure automatic cleanup of old inactive installs
    Cleanup {
        /// Retention (`30`, `60`, `never`), or `now` to review and remove the backlog
        retention: Option<String>,
    },

    /// Archive versions by removing extracted binaries while keeping raw artifacts when present
    Archive {
        /// Product name or alias
        product: String,

        /// Optional version to archive
        version: Option<String>,

        /// Archive all versions below this release version
        #[arg(long)]
        below: Option<String>,
    },

    /// Pick a version interactively (browse, install, switch)
    #[command(alias = "switch")]
    Select {
        /// Product name (claude, codex, pi) or alias (cc, cx). `ovm` selects OVM
        /// itself, but only on the alpha channel or with advanced.selfInPicker.
        /// Omit to pick a product first.
        product: Option<String>,

        /// Optional version — if given, switches directly (prompts to install if missing)
        version: Option<String>,
    },

    /// Put Echo in the Claude Code statusline
    Statusline,

    /// Show installed/archived counts, active version, and disk usage per product
    Stats,

    /// A tale of two cats and an echo (archived — `ovm hatch` is the way in now)
    #[command(hide = true)]
    Story {
        /// Play straight through without waiting for input
        #[arg(long)]
        fast: bool,
    },

    /// Hatch your setup — the story with install stops, or a tldr fast path
    ///
    /// Named for what it does: `/buddy` hatched a creature, this hatches the
    /// toolchain around it.
    Hatch,

    /// Install bare launch shortcuts (ccy, cxy, ccx, ccxy, claudex) as ~/.local/bin shims — no shell rc edits
    Shortcuts {
        /// Install without prompting
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Show release notes for a version
    Info {
        /// Product name or alias
        product: String,

        /// Version to inspect (default: current)
        version: Option<String>,
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Update installed products to the latest release now
    #[command(
        long_about = "Update installed products to their latest release, right now.\n\
        \n\
        \x20 ovm update                      every installed product\n\
        \x20 ovm update <product>            just that one (claude, codex, pi)\n\
        \x20 ovm update self                 OVM itself (same as `ovm self update`)\n\
        \x20 ovm update auto on|off|notify   launch-time policy for everything\n\
        \x20 ovm update auto <product> off   launch-time policy for one product\n\
        \x20 ovm update auto self notify     launch-time policy for OVM itself\n\
        \n\
        \n\
        Latest is resolved upstream, because you asked for it by name — unlike a \
        launch under `autoUpdate: on`, which only ever reads the local cache. \
        The resulting on-disk state is identical.\n\
        \n\
        Edge cases:\n\
        \x20 nothing installed  reported per product, with the `ovm install` line to run; \
        never a silent success, and never an unrequested first install.\n\
        \x20 offline            the resolver says so and falls back to the newest release \
        already in your store; a product it cannot resolve at all is reported as failed \
        and the command exits non-zero.\n\
        \x20 pinned             `ovm update` reports the pin and leaves it; \
        `ovm update <product>` overrides and clears it, resuming latest-tracking.\n\
        \x20 dev builds         `dev:` versions have no upstream and are left alone.\n\
        \x20 checkForUpdates    `false` turns off *automatic* checking, not this command; \
        an update you typed still resolves and installs."
    )]
    Update {
        /// `auto`, a product name, or `self`; omit to update everything
        first: Option<String>,

        /// Policy (`on`, `off`, `notify`) or product name after `auto`
        second: Option<String>,

        /// Policy (`on`, `off`, `notify`) after `auto <product>`
        third: Option<String>,

        /// Update everything found without asking (implied when not a terminal)
        #[arg(short = 'y', long, conflicts_with = "check")]
        yes: bool,

        /// Report what would update and change nothing
        #[arg(long)]
        check: bool,
    },

    /// Configure whether launches auto-update to latest releases
    ///
    /// Hidden alias for `ovm update auto` — kept working for existing scripts,
    /// docs and muscle memory. New surface: `ovm update auto on|off|notify`.
    #[command(name = "autoupdate", alias = "auto-update", hide = true)]
    AutoUpdate {
        /// Policy (`off` or `on`) or product name
        first: Option<String>,

        /// Product policy (`off` or `on`) when first is a product
        second: Option<String>,
    },

    /// Manage OVM's own installed versions
    #[command(name = "self")]
    SelfManage {
        #[command(subcommand)]
        command: SelfCommands,
    },

    /// Update the ovm binary itself (alias for `ovm self update`)
    #[command(name = "self-update", alias = "selfupdate")]
    SelfUpdate {
        /// Release channel (`stable`, `alpha`, or `beta`); overrides the
        /// persisted `self channel` setting for this run
        #[arg(long)]
        channel: Option<String>,

        /// Install method (`auto`, `direct`, `brew`, `cargo`, or `dev`)
        #[arg(long, default_value = "auto")]
        method: String,

        /// Print the package-manager commands without running them
        #[arg(long)]
        dry_run: bool,
    },

    /// Check whether a version will run degraded against the on-disk state DB
    #[command(alias = "check")]
    Doctor {
        /// Product name or alias
        product: String,

        /// Version to check (default: the active version)
        version: Option<String>,

        /// Repair issues that can be fixed automatically (Claude install hygiene)
        #[arg(long)]
        fix: bool,
    },
}
