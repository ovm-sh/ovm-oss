//! Terminal rendering and key-loop primitives for the select TUI.
//!
//! Each function in here owns a complete terminal frame: hide cursor → render →
//! read key → restore. They don't touch VersionManager or do I/O beyond the
//! release-notes fetch, which keeps the flow logic in `mod.rs` testable and
//! free of `Term`-based concerns.
use super::{SelfVersionRow, VersionEntry};
use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::sources::github_releases;
use console::{style, Key, Term};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

const DEFAULT_TERMINAL_WIDTH: usize = 80;
const MIN_VERSION_WIDTH: usize = 12;
const ROW_PREFIX_WIDTH: usize = 3;
// Everything in a row except the version cell: kind(7) + date(10) +
// installed(9) + active(6) + companion(5) columns plus five 2-space gaps.
// Must match the column widths in `VersionEntry::display_line`.
const TABLE_FIXED_WIDTH: usize = 47;

/// Outcome of a single pass through the version picker.
pub(super) enum SelectAction {
    Select(usize),
    Delete(usize),
    Cancel,
}

struct PickerScreen<'a> {
    term: &'a Term,
    alternate: bool,
}

impl<'a> PickerScreen<'a> {
    fn enter(term: &'a Term) -> Result<Self> {
        let alternate = term.is_term();
        if alternate {
            write_terminal_escape("\x1b[?1049h\x1b[2J\x1b[H")?;
        }
        term.hide_cursor()?;
        Ok(Self { term, alternate })
    }

    fn clear_frame(&self, last_line_count: usize) -> Result<()> {
        if self.alternate {
            write_terminal_escape("\x1b[H\x1b[2J")
        } else if last_line_count > 0 {
            Ok(self.term.clear_last_lines(last_line_count)?)
        } else {
            Ok(())
        }
    }

    fn finish(&mut self) -> Result<()> {
        self.term.show_cursor()?;
        if self.alternate {
            write_terminal_escape("\x1b[?1049l")?;
            self.alternate = false;
        }
        Ok(())
    }
}

impl Drop for PickerScreen<'_> {
    fn drop(&mut self) {
        let _ = self.term.show_cursor();
        if self.alternate {
            let _ = write_terminal_escape("\x1b[?1049l");
        }
    }
}

fn write_terminal_escape(sequence: &str) -> Result<()> {
    let mut stderr = std::io::stderr();
    stderr.write_all(sequence.as_bytes())?;
    stderr.flush()?;
    Ok(())
}

/// What the product picker can return: a managed product, the claudex
/// integration (dispatched to the `ovm-claudex` plugin — it isn't a
/// versioned product, so it never enters the version picker), or OVM itself
/// (only offered when gated on; drives the `ovm self` version list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductPick {
    Product(Product),
    Claudex,
    Ovm,
}

/// Build the product-picker rows paired with the pick they resolve to. `ovm`
/// is appended only when `include_ovm` is set — the gate applied by the caller
/// (alpha self-update channel or `advanced.selfInPicker`). Kept pure so the
/// gate is unit-testable without a terminal.
fn product_picker_entries(include_ovm: bool) -> Vec<(String, ProductPick)> {
    let mut entries: Vec<(String, ProductPick)> = Product::ALL
        .iter()
        .map(|p| {
            (
                format!(
                    "  {}  {}",
                    p.canonical_name(),
                    style(p.display_name()).dim()
                ),
                ProductPick::Product(*p),
            )
        })
        .collect();
    entries.push((
        format!("  {}  {}", "claudex", style("Claude Code on GPT-5.6").dim()),
        ProductPick::Claudex,
    ));
    if include_ovm {
        entries.push((
            format!("  {}  {}", "ovm", style("Open Version Manager").dim()),
            ProductPick::Ovm,
        ));
    }
    entries
}

/// Pick a product interactively. Returns None if the user pressed Esc.
/// `include_ovm` gates whether OVM itself is offered as a selectable entry.
pub fn pick_product(include_ovm: bool) -> Result<Option<ProductPick>> {
    let term = Term::stderr();
    let entries = product_picker_entries(include_ovm);
    let items: Vec<&str> = entries.iter().map(|(label, _)| label.as_str()).collect();

    let mut screen = PickerScreen::enter(&term)?;

    let mut cursor: usize = 0;

    loop {
        let mut lines: Vec<String> = Vec::new();
        lines.push(String::new());
        lines.push(format!("  {}", style("Select a product").bold()));
        lines.push(String::new());

        for (i, item) in items.iter().enumerate() {
            if i == cursor {
                lines.push(format!("{} {}", style("›").cyan().bold(), item));
            } else {
                lines.push(format!("  {item}"));
            }
        }

        lines.push(String::new());
        lines.push(format!(
            "  {} {} · {} {} · {} {}",
            style("↑↓").bold(),
            "navigate",
            style("enter").bold(),
            "select",
            style("esc").bold(),
            "quit"
        ));

        for line in &lines {
            term.write_line(line)
                .map_err(|e| OvmError::Message(e.to_string()))?;
        }

        let key = term
            .read_key()
            .map_err(|e| OvmError::Message(e.to_string()))?;

        screen.clear_frame(lines.len())?;

        match key {
            Key::ArrowUp | Key::Char('k') => cursor = cursor.saturating_sub(1),
            Key::ArrowDown | Key::Char('j') if cursor < items.len() - 1 => {
                cursor += 1;
            }
            Key::Enter => {
                screen.finish()?;
                let pick = entries
                    .get(cursor)
                    .map(|(_, pick)| *pick)
                    .unwrap_or(ProductPick::Claudex);
                return Ok(Some(pick));
            }
            Key::Escape => {
                screen.finish()?;
                return Ok(None);
            }
            _ => {}
        }
    }
}

/// Render a single OVM self-version row: version cell, a `dev`/`release` kind
/// tag, and a `current`/`previous` marker. Pure (returns the styled string) so
/// the marker logic is unit-testable via `strip_ansi_codes`.
fn render_self_row(row: &SelfVersionRow, version_width: usize) -> String {
    let kind_cell = format!("{:<7}", if row.is_dev() { "dev" } else { "release" });
    let kind_str = style(kind_cell).dim().to_string();

    let version_cell = super::fixed_width_cell(&row.version, version_width);
    let version_str = if row.current {
        style(version_cell).green().bold().to_string()
    } else {
        version_cell
    };

    let marker = if row.current {
        style("current").green().bold().to_string()
    } else if row.previous {
        style("previous").yellow().to_string()
    } else {
        String::new()
    };

    format!("{kind_str}  {version_str}  {marker}")
}

/// Second-level picker for OVM's own versions, reached from the product picker's
/// `ovm` entry. Returns the chosen index into `rows`, or None on Esc (back to
/// the product picker). Modeled on `pick_product` for a consistent two-level
/// feel; the caller drives the actual `ovm self use` switch.
pub(super) fn pick_self_version(
    rows: &[SelfVersionRow],
    can_go_back: bool,
) -> Result<Option<usize>> {
    let term = Term::stderr();
    let version_width = rows
        .iter()
        .map(|row| row.version.len())
        .max()
        .unwrap_or(0)
        .max(12);
    let mut cursor = rows.iter().position(|row| row.current).unwrap_or(0);

    let mut screen = PickerScreen::enter(&term)?;

    loop {
        let mut lines: Vec<String> = Vec::new();
        lines.push(String::new());
        lines.push(format!("  {}", style("Select an OVM version").bold()));
        lines.push(String::new());

        for (i, row) in rows.iter().enumerate() {
            let rendered = render_self_row(row, version_width);
            if i == cursor {
                lines.push(format!("{}  {}", style("›").cyan().bold(), rendered));
            } else {
                lines.push(format!("   {rendered}"));
            }
        }

        lines.push(String::new());
        let back_label = if can_go_back { "back" } else { "quit" };
        lines.push(format!(
            "  {} {} · {} {} · {} {}",
            style("↑↓").bold(),
            "navigate",
            style("enter").bold(),
            "select",
            style("esc").bold(),
            back_label
        ));

        for line in &lines {
            term.write_line(line)
                .map_err(|e| OvmError::Message(e.to_string()))?;
        }

        let key = term
            .read_key()
            .map_err(|e| OvmError::Message(e.to_string()))?;

        screen.clear_frame(lines.len())?;

        match key {
            Key::ArrowUp | Key::Char('k') => cursor = cursor.saturating_sub(1),
            Key::ArrowDown | Key::Char('j') if cursor + 1 < rows.len() => {
                cursor += 1;
            }
            Key::Enter => {
                screen.finish()?;
                return Ok(Some(cursor));
            }
            Key::Escape | Key::Char('q') => {
                screen.finish()?;
                return Ok(None);
            }
            _ => {}
        }
    }
}

/// Whether the product has a companion feature worth filtering on.
fn has_companion_filter(product: Product) -> bool {
    matches!(product, Product::Claude | Product::Codex)
}

fn is_codex_prerelease(entry: &VersionEntry) -> bool {
    Product::Codex
        .parsed_release_version(&entry.version)
        .is_some_and(|version| !version.pre.is_empty())
}

/// Result delivered by a background refresh: `Some` versions+dates on success,
/// `None` if the registry was unreachable.
pub(super) type FetchResult = Option<(Vec<String>, HashMap<String, String>)>;

/// Handle to an in-flight background registry refresh.
pub(super) struct RefreshHandle {
    pub rx: Receiver<FetchResult>,
}

/// Mutable picker session state that survives re-entry across a delete.
pub(super) struct PickerSession {
    pub product: Product,
    pub installed: Vec<String>,
    pub current: Option<String>,
    pub can_go_back: bool,
    /// Unix seconds of the last successful version check, if any.
    pub last_checked: Option<u64>,
    /// Set once a background refresh comes back empty (registry unreachable).
    pub offline: bool,
    /// In-flight background refresh, cleared once it resolves.
    pub refresh: Option<RefreshHandle>,
    /// In-process async installs started with `d`. Dropped jobs are cancelled
    /// when the picker exits; successful jobs only mark rows installed.
    pub downloads: DownloadJobs,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DownloadDisplay {
    Running,
    Failed,
}

enum DownloadJob {
    Running {
        child: Child,
        cleanup_path: Option<PathBuf>,
    },
    Failed(String),
}

#[derive(Default)]
pub(super) struct DownloadJobs {
    jobs: HashMap<String, DownloadJob>,
}

impl DownloadJobs {
    fn status(&self, version: &str) -> Option<DownloadDisplay> {
        match self.jobs.get(version) {
            Some(DownloadJob::Running { .. }) => Some(DownloadDisplay::Running),
            Some(DownloadJob::Failed(_)) => Some(DownloadDisplay::Failed),
            None => None,
        }
    }

    fn counts(&self) -> (usize, usize) {
        self.jobs
            .values()
            .fold((0, 0), |(running, failed), job| match job {
                DownloadJob::Running { .. } => (running + 1, failed),
                DownloadJob::Failed(_) => (running, failed + 1),
            })
    }

    fn first_failure(&self) -> Option<&str> {
        self.jobs.values().find_map(|job| match job {
            DownloadJob::Failed(message) => Some(message.as_str()),
            DownloadJob::Running { .. } => None,
        })
    }

    fn start(&mut self, product: Product, version: &str) -> Result<()> {
        if matches!(self.jobs.get(version), Some(DownloadJob::Running { .. })) {
            return Ok(());
        }

        self.jobs.remove(version);
        let exe = std::env::current_exe()?;
        let cleanup_path = partial_download_path(product, version);
        let child = Command::new(exe)
            .args(["install", product.canonical_name(), version])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| OvmError::Message(format!("failed to start download: {error}")))?;
        self.jobs.insert(
            version.to_string(),
            DownloadJob::Running {
                child,
                cleanup_path,
            },
        );
        Ok(())
    }

    fn poll(
        &mut self,
        entries: &mut [VersionEntry],
        installed: &mut Vec<String>,
        product: Product,
    ) -> bool {
        let mut finished = Vec::new();
        let mut changed = false;
        for (version, job) in self.jobs.iter_mut() {
            let DownloadJob::Running { child, .. } = job else {
                continue;
            };
            match child.try_wait() {
                Ok(Some(_)) => {
                    finished.push(version.clone());
                    changed = true;
                }
                Ok(None) => {}
                Err(error) => {
                    *job = DownloadJob::Failed(error.to_string());
                    changed = true;
                }
            }
        }

        for version in finished {
            let Some(DownloadJob::Running { child, .. }) = self.jobs.remove(&version) else {
                continue;
            };
            match child.wait_with_output() {
                Ok(output) if output.status.success() => {
                    if let Some(entry) = entries.iter_mut().find(|entry| entry.version == version) {
                        entry.installed = true;
                    }
                    if !installed.iter().any(|installed| installed == &version) {
                        installed.push(version);
                        product.sort_versions(installed);
                    }
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let message = stderr
                        .lines()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or("download failed")
                        .trim()
                        .to_string();
                    self.jobs.insert(version, DownloadJob::Failed(message));
                }
                Err(error) => {
                    self.jobs
                        .insert(version, DownloadJob::Failed(error.to_string()));
                }
            }
        }

        changed
    }
}

impl Drop for DownloadJobs {
    fn drop(&mut self) {
        for job in self.jobs.values_mut() {
            if let DownloadJob::Running {
                child,
                cleanup_path,
            } = job
            {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(path) = cleanup_path {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}

fn partial_download_path(product: Product, version: &str) -> Option<PathBuf> {
    let dirs = crate::config::OvmDirs::new().ok()?;
    let product_dirs = dirs.product_dirs(product);
    match product {
        Product::Claude => Some(product_dirs.native_bin(version).with_extension("part")),
        Product::Codex => Some(product_dirs.release_bin(version).with_extension("tar.gz")),
        Product::Pi => Some(product_dirs.release_bundle_dir(version).join("pi.tar.gz")),
    }
}

/// A rendered line in the picker: a non-selectable section header, or a
/// selectable version row pointing at an index in the master `entries` list.
enum Row {
    Header(&'static str),
    Version(usize),
}

struct VersionPickerFrame<'a> {
    entries: &'a [VersionEntry],
    rows: &'a [Row],
    product: Product,
    cursor: usize,
    offset: usize,
    visible: usize,
    buddy_filter: bool,
    show_all_releases: bool,
    can_go_back: bool,
    terminal_width: usize,
    status_line: Option<String>,
    downloads: &'a DownloadJobs,
}

/// Build the two-section row layout: every installed version pinned under an
/// "installed" header (always shown, never filtered), then the full history
/// under an "all versions" header (honoring the buddy / release filters).
/// Installed versions appear in both sections by design — "what you have" stays
/// one glance away no matter how far the history scrolls.
fn build_rows(
    entries: &[VersionEntry],
    product: Product,
    buddy_filter: bool,
    show_all_releases: bool,
) -> Vec<Row> {
    let mut rows = Vec::new();

    let has_installed = entries.iter().any(|entry| entry.installed);
    if has_installed {
        rows.push(Row::Header("installed"));
        for (index, entry) in entries.iter().enumerate() {
            if entry.installed {
                rows.push(Row::Version(index));
            }
        }
    }

    let history: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| !buddy_filter || entry.has_companion == Some(true))
        .filter(|(_, entry)| {
            // Dev builds never parse as release semver, so they pass the
            // prerelease check and always stay visible.
            !matches!(product, Product::Codex) || show_all_releases || !is_codex_prerelease(entry)
        })
        .map(|(index, _)| index)
        .collect();

    if !history.is_empty() {
        rows.push(Row::Header(if has_installed {
            "all versions"
        } else {
            "versions"
        }));
        for index in history {
            rows.push(Row::Version(index));
        }
    }

    rows
}

/// Entry index under a row, or None if it is a header.
fn entry_index(rows: &[Row], cursor: usize) -> Option<usize> {
    match rows.get(cursor) {
        Some(Row::Version(index)) => Some(*index),
        _ => None,
    }
}

fn first_selectable(rows: &[Row]) -> usize {
    rows.iter()
        .position(|row| matches!(row, Row::Version(_)))
        .unwrap_or(0)
}

/// Cursor on the active version if present, else the first selectable row.
fn active_or_first(rows: &[Row], entries: &[VersionEntry]) -> usize {
    rows.iter()
        .position(|row| matches!(row, Row::Version(i) if entries[*i].active))
        .unwrap_or_else(|| first_selectable(rows))
}

/// Row index of the first row rendering `version`, if any. Used to keep the
/// cursor on the same version across a filter toggle or a live refresh.
fn locate_version(rows: &[Row], entries: &[VersionEntry], version: &str) -> Option<usize> {
    rows.iter()
        .position(|row| matches!(row, Row::Version(i) if entries[*i].version == version))
}

/// Move the cursor to the next selectable row in the given direction, skipping
/// headers. Stays put at the ends.
fn step_cursor(rows: &[Row], cursor: usize, down: bool) -> usize {
    let mut index = cursor;
    loop {
        index = if down {
            if index + 1 >= rows.len() {
                return cursor;
            }
            index + 1
        } else {
            if index == 0 {
                return cursor;
            }
            index - 1
        };
        if matches!(rows[index], Row::Version(_)) {
            return index;
        }
    }
}

/// Human-friendly "time since" for the last successful version check.
fn relative_time(fetched_at: u64) -> String {
    let secs = crate::update_cache::now_secs().saturating_sub(fetched_at);
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// The status hint shown under the list: checking, offline, or last-checked.
fn status_line(session: &PickerSession) -> Option<String> {
    if session.refresh.is_some() {
        return Some(format!("  {} checking for updates…", style("⟳").cyan()));
    }
    if session.offline {
        return Some(format!(
            "  {} offline · showing cached versions",
            style("!").yellow()
        ));
    }
    session.last_checked.map(|at| {
        format!(
            "  {} updated {}",
            style("✓").green().dim(),
            style(relative_time(at)).dim()
        )
    })
}

/// What `wait_for_input` resolved to: a keypress, or a finished refresh.
enum Wait {
    Key(Key),
    Refreshed(FetchResult),
}

/// Wait for the next picker event. While a background refresh is still in flight
/// and we're inside the live window, briefly block on the refresh channel so a
/// fast result paints on its own; past the window (or with no refresh in flight)
/// block on the keyboard, folding a late result in on the next keypress.
fn wait_for_input(
    term: &Term,
    refresh: &Option<RefreshHandle>,
    live_deadline: Instant,
) -> Result<Wait> {
    loop {
        if let Some(handle) = refresh {
            let now = Instant::now();
            if now < live_deadline {
                let slice = (live_deadline - now).min(Duration::from_millis(120));
                match handle.rx.recv_timeout(slice) {
                    Ok(outcome) => return Ok(Wait::Refreshed(outcome)),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return Ok(Wait::Refreshed(None)),
                }
            } else {
                match handle.rx.try_recv() {
                    Ok(outcome) => return Ok(Wait::Refreshed(outcome)),
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => return Ok(Wait::Refreshed(None)),
                }
            }
        }
        let key = term
            .read_key()
            .map_err(|e| OvmError::Message(e.to_string()))?;
        return Ok(Wait::Key(key));
    }
}

/// Custom interactive selector with `i` for info, `d` to delete, `b` to filter
/// to companion (buddy / pet) versions only. Renders installed versions instantly
/// from the local cache and folds a background refresh in live when it lands.
pub(super) fn interactive_select(
    entries: &mut Vec<VersionEntry>,
    session: &mut PickerSession,
) -> Result<SelectAction> {
    let term = Term::stderr();
    let visible = 15; // max visible rows
    let mut buddy_filter = false;
    let mut show_all_releases = false;
    let mut rows = build_rows(entries, session.product, buddy_filter, show_all_releases);
    let mut cursor = active_or_first(&rows, entries);
    let mut offset = cursor.saturating_sub(visible / 2);

    let mut screen = PickerScreen::enter(&term)?;
    // Live-refresh window: while the first check is in flight, wait briefly so a
    // fast result paints without a keypress. Bounded so the picker never feels
    // frozen — after this, a late result folds in on the next keypress instead.
    let live_deadline = Instant::now() + Duration::from_millis(1000);

    loop {
        if session
            .downloads
            .poll(entries, &mut session.installed, session.product)
        {
            rows = build_rows(entries, session.product, buddy_filter, show_all_releases);
        }

        // If filters emptied the history and nothing is installed either, drop
        // the filters so the user never stares at an empty list.
        if !rows.iter().any(|row| matches!(row, Row::Version(_))) {
            buddy_filter = false;
            show_all_releases = true;
            rows = build_rows(entries, session.product, buddy_filter, show_all_releases);
            cursor = active_or_first(&rows, entries);
            offset = cursor.saturating_sub(visible / 2);
        }

        // Keep the cursor on a selectable row (headers may have shifted).
        if !matches!(rows.get(cursor), Some(Row::Version(_))) {
            cursor = first_selectable(&rows);
        }

        // Clamp scroll offset.
        if cursor < offset {
            offset = cursor;
        }
        if cursor >= offset + visible {
            offset = cursor - visible + 1;
        }

        let lines = render_version_picker_frame(VersionPickerFrame {
            entries,
            rows: &rows,
            product: session.product,
            cursor,
            offset,
            visible,
            buddy_filter,
            show_all_releases,
            can_go_back: session.can_go_back,
            terminal_width: terminal_width(&term),
            status_line: status_line(session),
            downloads: &session.downloads,
        });

        for line in &lines {
            term.write_line(line)
                .map_err(|e| OvmError::Message(e.to_string()))?;
        }

        let event = wait_for_input(&term, &session.refresh, live_deadline)?;
        screen.clear_frame(lines.len())?;

        let key = match event {
            // A refresh with versions replaces the list. An *empty* result is
            // treated as "no fresh data" (a registry glitch shouldn't blank the
            // picker and trap the user with nothing selectable) — keep what we
            // already show and just flip the status to offline.
            Wait::Refreshed(Some((versions, dates))) if !versions.is_empty() => {
                let keep = entry_index(&rows, cursor).map(|i| entries[i].version.clone());
                *entries = super::build_entries(
                    session.product,
                    &session.installed,
                    session.current.as_deref(),
                    &versions,
                    &dates,
                );
                session.refresh = None;
                session.offline = false;
                session.last_checked = Some(crate::update_cache::now_secs());
                rows = build_rows(entries, session.product, buddy_filter, show_all_releases);
                cursor = keep
                    .and_then(|version| locate_version(&rows, entries, &version))
                    .unwrap_or_else(|| active_or_first(&rows, entries));
                offset = cursor.saturating_sub(visible / 2);
                continue;
            }
            Wait::Refreshed(_) => {
                session.refresh = None;
                session.offline = true;
                continue;
            }
            Wait::Key(key) => key,
        };

        // Entry under the cursor, if any. May be None if the list is momentarily
        // empty — navigation and quit still work; only entry actions are skipped,
        // so the picker is never inescapable.
        let cursor_entry = entry_index(&rows, cursor);

        match key {
            Key::ArrowUp | Key::Char('k') => {
                cursor = step_cursor(&rows, cursor, false);
            }
            Key::ArrowDown | Key::Char('j') => {
                cursor = step_cursor(&rows, cursor, true);
            }
            Key::Enter => {
                if let Some(index) = cursor_entry {
                    if session.downloads.status(&entries[index].version)
                        == Some(DownloadDisplay::Running)
                    {
                        continue;
                    }
                    screen.finish()?;
                    return Ok(SelectAction::Select(index));
                }
            }
            Key::Escape | Key::Char('q') => {
                screen.finish()?;
                return Ok(SelectAction::Cancel);
            }
            Key::Char('i') | Key::Char('I') => {
                if let Some(index) = cursor_entry {
                    show_release_notes(&term, &entries[index].version, session.product)?;
                }
            }
            Key::Char('d') | Key::Char('D') => {
                if let Some(index) = cursor_entry {
                    let entry = &entries[index];
                    if !entry.installed {
                        session.downloads.start(session.product, &entry.version)?;
                    } else if !entry.active {
                        let prompt = format!(
                            "Delete {} {}?",
                            session.product.display_name(),
                            style(&entry.version).bold()
                        );
                        if confirm_inline(&term, &prompt)? {
                            screen.finish()?;
                            return Ok(SelectAction::Delete(index));
                        }
                    }
                }
            }
            Key::Char('b') | Key::Char('B') if has_companion_filter(session.product) => {
                let keep = entry_index(&rows, cursor).map(|i| entries[i].version.clone());
                buddy_filter = !buddy_filter;
                rows = build_rows(entries, session.product, buddy_filter, show_all_releases);
                cursor = keep
                    .and_then(|version| locate_version(&rows, entries, &version))
                    .unwrap_or_else(|| active_or_first(&rows, entries));
                offset = cursor.saturating_sub(visible / 2);
            }
            Key::Char('r') | Key::Char('R') if matches!(session.product, Product::Codex) => {
                let keep = entry_index(&rows, cursor).map(|i| entries[i].version.clone());
                show_all_releases = !show_all_releases;
                rows = build_rows(entries, session.product, buddy_filter, show_all_releases);
                cursor = keep
                    .and_then(|version| locate_version(&rows, entries, &version))
                    .unwrap_or_else(|| active_or_first(&rows, entries));
                offset = cursor.saturating_sub(visible / 2);
            }
            _ => {}
        }
    }
}

fn render_version_picker_frame(frame: VersionPickerFrame<'_>) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push(String::new());
    let companion_label = match frame.product {
        Product::Claude => "buddy",
        Product::Codex => "pet",
        Product::Pi => "",
    };

    let end = (frame.offset + frame.visible).min(frame.rows.len());
    let desired_version_width = frame
        .entries
        .iter()
        .map(|entry| entry.version.len())
        .max()
        .unwrap_or(0)
        .max(24);
    let max_version_width = frame
        .terminal_width
        .saturating_sub(ROW_PREFIX_WIDTH + TABLE_FIXED_WIDTH + 1);
    let minimum = MIN_VERSION_WIDTH.min(max_version_width.max(1));
    let version_width = desired_version_width.min(max_version_width).max(minimum);

    let mut mode_parts: Vec<String> = Vec::new();
    if matches!(frame.product, Product::Codex) {
        mode_parts.push(
            if frame.show_all_releases {
                "all releases"
            } else {
                "real releases"
            }
            .to_string(),
        );
    }
    if frame.buddy_filter && !companion_label.is_empty() {
        mode_parts.push(format!("{companion_label} only"));
    }
    if !mode_parts.is_empty() {
        lines.push(format!(
            "  showing {}",
            style(mode_parts.join(" · ")).yellow()
        ));
        lines.push(String::new());
    }

    lines.push(format!(
        "    {:<7}  {:<version_width$}  {:<10}  {:<9}  {:<6}  {}",
        "kind", "version", "date", "installed", "active", companion_label
    ));
    lines.push(String::new());
    if frame.offset > 0 {
        lines.push(format!("    {}", style("↑ more").dim()));
    }

    for (row_index, row) in frame.rows.iter().enumerate().take(end).skip(frame.offset) {
        match row {
            Row::Header(label) => {
                lines.push(format!("  {}", style(label).bold().dim()));
            }
            Row::Version(entry_idx) => {
                let entry = &frame.entries[*entry_idx];
                let download = frame.downloads.status(&entry.version);
                if row_index == frame.cursor {
                    lines.push(format!(
                        "{}  {}",
                        style("›").cyan().bold(),
                        entry.display_line(version_width, download)
                    ));
                } else {
                    lines.push(format!(
                        "   {}",
                        entry.display_line(version_width, download)
                    ));
                }
            }
        }
    }

    if end < frame.rows.len() {
        lines.push(format!("    {}", style("↓ more").dim()));
    }

    lines.push(String::new());
    if let Some(status) = &frame.status_line {
        lines.push(status.clone());
    }
    let (running, failed) = frame.downloads.counts();
    if running > 0 || failed > 0 {
        let mut parts = Vec::new();
        if running > 0 {
            parts.push(format!("{} {} downloading", style("↓").cyan(), running));
        }
        if failed > 0 {
            parts.push(format!("{} {} failed", style("!").yellow(), failed));
        }
        let detail = frame
            .downloads
            .first_failure()
            .map(|message| format!(" {}", style(message).dim()))
            .unwrap_or_default();
        lines.push(format!("  {}{}", parts.join(" · "), detail));
    }

    let back_label = if frame.can_go_back { "back" } else { "quit" };
    let cursor_entry = entry_index(frame.rows, frame.cursor).map(|index| &frame.entries[index]);
    let compact_footer = frame.terminal_width < 90;
    let delete_hint = match cursor_entry {
        Some(entry) if frame.downloads.status(&entry.version) == Some(DownloadDisplay::Running) => {
            format!(" · {} {}", style("d").bold(), "downloading")
        }
        Some(entry) if frame.downloads.status(&entry.version) == Some(DownloadDisplay::Failed) => {
            format!(" · {} {}", style("d").bold(), "retry")
        }
        Some(entry) if entry.installed && !entry.active => {
            let label = if compact_footer { "del" } else { "delete" };
            format!(" · {} {}", style("d").bold(), label)
        }
        Some(entry) if !entry.installed => {
            let label = if compact_footer { "dl" } else { "download" };
            format!(" · {} {}", style("d").bold(), label)
        }
        _ => String::new(),
    };
    let buddy_hint = if has_companion_filter(frame.product) {
        let label = if frame.buddy_filter {
            "all"
        } else {
            companion_label
        };
        format!(" · {} {}", style("b").bold(), label)
    } else {
        String::new()
    };
    let release_hint = if matches!(frame.product, Product::Codex) {
        let label = if frame.show_all_releases {
            "real"
        } else {
            "all"
        };
        let suffix = if compact_footer { "" } else { " releases" };
        format!(" · {} {}{}", style("r").bold(), label, suffix)
    } else {
        String::new()
    };
    let move_label = if compact_footer { "move" } else { "navigate" };
    lines.push(format!(
        "  {} {} · {} {} · {} {}{}{}{} · {} {}",
        style("↑↓").bold(),
        move_label,
        style("enter").bold(),
        "select",
        style("i").bold(),
        "info",
        delete_hint,
        buddy_hint,
        release_hint,
        style("esc").bold(),
        back_label
    ));

    lines
}

fn terminal_width(term: &Term) -> usize {
    term.size_checked()
        .map(|(_, cols)| cols as usize)
        .filter(|&cols| cols > 0)
        .unwrap_or(DEFAULT_TERMINAL_WIDTH)
}

/// Render an inline y/N confirm. Returns true on `y` or `Y`.
pub(super) fn confirm_inline(term: &Term, prompt: &str) -> Result<bool> {
    term.write_line("")
        .map_err(|e| OvmError::Message(e.to_string()))?;
    term.write_line(&format!(
        "  {} {} {}",
        style("?").yellow().bold(),
        prompt,
        style("[y/N]").dim()
    ))
    .map_err(|e| OvmError::Message(e.to_string()))?;

    let key = term
        .read_key()
        .map_err(|e| OvmError::Message(e.to_string()))?;
    let confirmed = matches!(key, Key::Char('y') | Key::Char('Y'));

    term.clear_last_lines(2)
        .map_err(|e| OvmError::Message(e.to_string()))?;
    Ok(confirmed)
}

/// Display release notes inline, wait for any key to dismiss.
pub(super) fn show_release_notes(term: &Term, version: &str, product: Product) -> Result<()> {
    term.write_line(&format!(
        "  {} Fetching release notes for {}...",
        style("→").dim(),
        style(version).bold()
    ))
    .map_err(|e| OvmError::Message(e.to_string()))?;

    let notes = github_releases::get_release_notes(product, version)?;

    // Clear the "fetching" line
    term.clear_last_lines(1)
        .map_err(|e| OvmError::Message(e.to_string()))?;

    let mut lines = Vec::new();
    lines.push(String::new());
    lines.push(format!(
        "  {} {} {}",
        product.display_name(),
        style(version).green().bold(),
        style("release notes").dim()
    ));
    lines.push(String::new());

    match notes {
        Some(body) => {
            let max_lines = 30;
            for (count, line) in body.lines().enumerate() {
                if count >= max_lines {
                    lines.push(format!("  {}", style("... (truncated)").dim()));
                    break;
                }
                if line.starts_with("## ") {
                    lines.push(format!("  {}", style(line).bold()));
                } else {
                    lines.push(format!("  {line}"));
                }
            }
        }
        None => {
            lines.push(format!("  {}", style("No release notes found.").dim()));
        }
    }

    lines.push(String::new());
    lines.push(format!("  {}", style("press any key to go back").dim()));

    let line_count = lines.len();
    for line in &lines {
        term.write_line(line)
            .map_err(|e| OvmError::Message(e.to_string()))?;
    }

    // Wait for any key
    let _ = term.read_key();

    // Clear the release notes
    term.clear_last_lines(line_count)
        .map_err(|e| OvmError::Message(e.to_string()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        build_rows, first_selectable, is_codex_prerelease, product_picker_entries, relative_time,
        render_self_row, render_version_picker_frame, status_line, DownloadJobs, PickerSession,
        ProductPick, RefreshHandle, Row, VersionPickerFrame,
    };
    use crate::commands::select::{SelfVersionRow, VersionEntry};
    use crate::product::Product;
    use crate::update_cache::now_secs;

    fn plain(line: &str) -> String {
        console::strip_ansi_codes(line).into_owned()
    }

    #[test]
    fn product_picker_omits_ovm_by_default() {
        let entries = product_picker_entries(false);
        assert!(
            !entries.iter().any(|(_, pick)| *pick == ProductPick::Ovm),
            "stable/no-flag picker must not list OVM"
        );
        let labels: Vec<String> = entries.iter().map(|(label, _)| plain(label)).collect();
        assert!(!labels.iter().any(|label| label.contains("ovm")));
        // Existing products + claudex remain present and unchanged.
        assert!(entries
            .iter()
            .any(|(_, pick)| *pick == ProductPick::Product(Product::Claude)));
        assert!(entries
            .iter()
            .any(|(_, pick)| *pick == ProductPick::Claudex));
    }

    #[test]
    fn product_picker_includes_ovm_when_gated_on() {
        let entries = product_picker_entries(true);
        assert!(entries.iter().any(|(_, pick)| *pick == ProductPick::Ovm));
        let ovm_label = entries
            .iter()
            .find(|(_, pick)| *pick == ProductPick::Ovm)
            .map(|(label, _)| plain(label))
            .expect("ovm row");
        assert!(ovm_label.contains("ovm"), "{ovm_label}");
        assert!(ovm_label.contains("Open Version Manager"), "{ovm_label}");
    }

    fn self_row(version: &str, current: bool, previous: bool) -> SelfVersionRow {
        SelfVersionRow {
            version: version.to_string(),
            current,
            previous,
        }
    }

    #[test]
    fn self_row_marks_current_and_tags_release() {
        let rendered = plain(&render_self_row(&self_row("0.2.0", true, false), 16));
        assert!(rendered.contains("release"), "{rendered}");
        assert!(rendered.contains("0.2.0"), "{rendered}");
        assert!(rendered.contains("current"), "{rendered}");
        assert!(!rendered.contains("previous"), "{rendered}");
    }

    #[test]
    fn self_row_marks_previous_rollback_target() {
        let rendered = plain(&render_self_row(&self_row("0.1.9", false, true), 16));
        assert!(rendered.contains("previous"), "{rendered}");
        assert!(!rendered.contains("current"), "{rendered}");
    }

    #[test]
    fn self_row_tags_dev_snapshots() {
        let rendered = plain(&render_self_row(
            &self_row("dev-abc123def456", false, false),
            20,
        ));
        assert!(rendered.contains("dev"), "{rendered}");
        assert!(rendered.contains("dev-abc123def456"), "{rendered}");
        assert!(!rendered.contains("current"), "{rendered}");
        assert!(!rendered.contains("previous"), "{rendered}");
    }

    #[test]
    fn self_version_row_is_dev_only_for_dev_prefix() {
        assert!(self_row("dev-abc123", false, false).is_dev());
        assert!(!self_row("0.2.0", false, false).is_dev());
    }

    fn entry(version: &str, installed: bool, date: Option<&str>) -> VersionEntry {
        VersionEntry {
            version: version.to_string(),
            date: date.map(String::from),
            installed,
            active: false,
            has_companion: None,
        }
    }

    fn pet_entry(version: &str, installed: bool, date: Option<&str>) -> VersionEntry {
        VersionEntry {
            has_companion: Some(true),
            ..entry(version, installed, date)
        }
    }

    fn plain_frame(lines: Vec<String>) -> String {
        lines
            .into_iter()
            .map(|line| console::strip_ansi_codes(&line).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Versions of the selectable rows that fall under the history header
    /// ("all versions" / "versions") — i.e. what the filters actually drive.
    fn history_versions<'a>(rows: &[Row], entries: &'a [VersionEntry]) -> Vec<&'a str> {
        let start = rows
            .iter()
            .position(|row| matches!(row, Row::Header(label) if *label != "installed"))
            .expect("history header");
        rows[start + 1..]
            .iter()
            .filter_map(|row| match row {
                Row::Version(index) => Some(entries[*index].version.as_str()),
                Row::Header(_) => None,
            })
            .collect()
    }

    fn render(
        entries: &[VersionEntry],
        product: Product,
        buddy_filter: bool,
        show_all_releases: bool,
        terminal_width: usize,
    ) -> String {
        let rows = build_rows(entries, product, buddy_filter, show_all_releases);
        plain_frame(render_version_picker_frame(VersionPickerFrame {
            entries,
            rows: &rows,
            product,
            cursor: first_selectable(&rows),
            offset: 0,
            visible: 15,
            buddy_filter,
            show_all_releases,
            can_go_back: true,
            terminal_width,
            status_line: None,
            downloads: &DownloadJobs::default(),
        }))
    }

    #[test]
    fn codex_prerelease_detection_uses_release_semver() {
        assert!(!is_codex_prerelease(&entry("dev:local-build", true, None)));
        assert!(!is_codex_prerelease(&entry(
            "rust-v0.137.0",
            false,
            Some("2026-06-01")
        )));
        assert!(is_codex_prerelease(&entry(
            "rust-v0.138.0-alpha.1",
            false,
            Some("2026-06-02")
        )));
    }

    #[test]
    fn installed_versions_appear_in_their_own_section_and_in_history() {
        let entries = vec![
            entry("2.1.121", false, Some("2026-05-20")),
            entry("2.1.112", true, Some("2026-05-13")),
            entry("2.1.96", true, Some("2026-04-02")),
        ];

        let rows = build_rows(&entries, Product::Claude, false, false);

        // First section is "installed" and lists exactly the installed versions.
        assert!(matches!(rows[0], Row::Header("installed")));
        let installed: Vec<&str> = rows
            .iter()
            .take_while(|row| !matches!(row, Row::Header("all versions")))
            .filter_map(|row| match row {
                Row::Version(index) => Some(entries[*index].version.as_str()),
                Row::Header(_) => None,
            })
            .collect();
        assert_eq!(installed, vec!["2.1.112", "2.1.96"]);

        // History section repeats them (duplicates by design) inside the full list.
        let history = history_versions(&rows, &entries);
        assert_eq!(history, vec!["2.1.121", "2.1.112", "2.1.96"]);
    }

    #[test]
    fn no_installed_section_when_nothing_is_installed() {
        let entries = vec![
            entry("2.1.121", false, Some("2026-05-20")),
            entry("2.1.120", false, Some("2026-05-19")),
        ];

        let rows = build_rows(&entries, Product::Claude, false, false);
        assert!(matches!(rows[0], Row::Header("versions")));
        assert!(!rows
            .iter()
            .any(|row| matches!(row, Row::Header("installed"))));
    }

    #[test]
    fn codex_real_releases_view_hides_prereleases_but_keeps_dev_builds() {
        let entries = vec![
            entry("dev:local-build", true, None),
            entry("rust-v0.135.0", true, None),
            entry("rust-v0.138.0-alpha.1", false, Some("2026-06-02")),
            entry("rust-v0.137.0", false, Some("2026-06-01")),
        ];

        let rows = build_rows(&entries, Product::Codex, false, false);
        assert_eq!(
            history_versions(&rows, &entries),
            vec!["dev:local-build", "rust-v0.135.0", "rust-v0.137.0"]
        );
    }

    #[test]
    fn codex_all_releases_view_shows_alpha_rows() {
        let entries = vec![
            entry("rust-v0.138.0-alpha.1", false, Some("2026-06-02")),
            entry("rust-v0.137.0", false, Some("2026-06-01")),
        ];

        let rows = build_rows(&entries, Product::Codex, false, true);
        assert_eq!(
            history_versions(&rows, &entries),
            vec!["rust-v0.138.0-alpha.1", "rust-v0.137.0"]
        );
    }

    #[test]
    fn codex_real_releases_frame_matches_expected_selector_ui() {
        let entries = vec![
            pet_entry("dev:local-build", true, None),
            pet_entry("rust-v0.135.0", true, None),
            pet_entry("rust-v0.138.0-alpha.1", false, Some("2026-06-02")),
            pet_entry("rust-v0.137.0", false, Some("2026-06-01")),
        ];

        let frame = render(&entries, Product::Codex, false, false, 120);

        assert!(frame.contains("kind     version"));
        assert!(frame.contains("installed"));
        assert!(frame.contains("all versions"));
        assert!(frame.contains("showing real releases"));
        assert!(frame.contains("pet"));
        assert!(frame.contains("dev      dev:local-build"));
        assert!(frame.contains("release  rust-v0.135.0"));
        assert!(frame.contains("release  rust-v0.137.0"));
        assert!(frame.contains("r all releases"));
        assert!(frame.contains("b pet"));
        assert!(
            !frame.contains("rust-v0.138.0-alpha.1"),
            "real-release view should hide prereleases:\n{frame}"
        );
        assert!(
            !frame.contains("/pet") && !frame.contains("= pet"),
            "pet label should not be duplicated as a legend:\n{frame}"
        );
    }

    #[test]
    fn codex_all_releases_frame_exposes_prereleases_and_toggle_back() {
        let entries = vec![
            pet_entry("rust-v0.138.0-alpha.1", false, Some("2026-06-02")),
            pet_entry("rust-v0.137.0", false, Some("2026-06-01")),
        ];

        let frame = render(&entries, Product::Codex, false, true, 120);

        assert!(frame.contains("showing all releases"));
        assert!(frame.contains("pet"));
        assert!(frame.contains("release  rust-v0.138.0-alpha.1"));
        assert!(frame.contains("release  rust-v0.137.0"));
        assert!(frame.contains("r real releases"));
    }

    #[test]
    fn footer_shows_download_for_uninstalled_cursor_row() {
        let entries = vec![entry("2.1.121", false, Some("2026-05-20"))];

        let frame = render(&entries, Product::Claude, false, false, 120);

        assert!(frame.contains("d download"), "{frame}");
        assert!(!frame.contains("d delete"), "{frame}");
    }

    #[test]
    fn footer_shows_delete_for_installed_inactive_cursor_row() {
        let entries = vec![entry("2.1.121", true, Some("2026-05-20"))];

        let frame = render(&entries, Product::Claude, false, false, 120);

        assert!(frame.contains("d delete"), "{frame}");
        assert!(!frame.contains("d download"), "{frame}");
    }

    #[test]
    fn codex_frame_truncates_long_versions_to_fit_terminal_width() {
        let entries = vec![
            pet_entry("dev:mochi-thread-unsubscribe-resume-20260607", true, None),
            pet_entry("dev:mochi-thread-unsubscribe-only-20260607", true, None),
            pet_entry("rust-v0.137.0", false, Some("2026-06-01")),
            pet_entry("rust-v0.136.0", false, Some("2026-05-29")),
        ];

        let frame = render(&entries, Product::Codex, false, false, 80);

        assert!(frame.contains("dev:mochi-thread-unsubscribe…"), "{frame}");
        assert!(
            frame.lines().all(|line| line.chars().count() < 80),
            "frame should not wrap at 80 columns:\n{frame}"
        );
        assert!(
            !frame.lines().any(|line| line.trim() == "active  pet"),
            "columns should not wrap into a second header line:\n{frame}"
        );
    }

    fn session(
        refresh: Option<RefreshHandle>,
        offline: bool,
        last_checked: Option<u64>,
    ) -> PickerSession {
        PickerSession {
            product: Product::Claude,
            installed: Vec::new(),
            current: None,
            can_go_back: true,
            last_checked,
            offline,
            refresh,
            downloads: DownloadJobs::default(),
        }
    }

    #[test]
    fn status_line_shows_checking_while_refresh_in_flight() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let line = status_line(&session(
            Some(RefreshHandle { rx }),
            false,
            Some(now_secs()),
        ))
        .expect("status");
        assert!(line.contains("checking for updates"), "{line}");
    }

    #[test]
    fn status_line_shows_offline_after_failed_refresh() {
        let line = status_line(&session(None, true, None)).expect("status");
        assert!(line.contains("offline"), "{line}");
        assert!(line.contains("cached"), "{line}");
    }

    #[test]
    fn status_line_shows_last_checked_when_idle_and_fresh() {
        let line = status_line(&session(None, false, Some(now_secs()))).expect("status");
        assert!(line.contains("updated"), "{line}");
        assert!(line.contains("just now"), "{line}");
    }

    #[test]
    fn status_line_absent_when_never_checked() {
        assert!(status_line(&session(None, false, None)).is_none());
    }

    #[test]
    fn relative_time_buckets_by_magnitude() {
        let now = now_secs();
        assert_eq!(relative_time(now), "just now");
        assert_eq!(relative_time(now.saturating_sub(5 * 60)), "5m ago");
        assert_eq!(relative_time(now.saturating_sub(3 * 3600)), "3h ago");
        assert_eq!(relative_time(now.saturating_sub(2 * 86_400)), "2d ago");
    }
}
