use clap::{Parser, Subcommand};
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
        version: String,
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
        /// Retention (`30`, `60`, or `never`)
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

    /// Show installed/archived counts, active version, and disk usage per product
    Stats,

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

    /// Configure whether launches auto-update to latest releases
    #[command(name = "autoupdate", alias = "auto-update")]
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
