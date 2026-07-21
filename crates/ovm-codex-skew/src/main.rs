//! `ovm-codex-skew` — OVM companion plugin for Codex.
//!
//! OVM invokes this at lifecycle events (pre-launch, post-switch) with an env
//! contract; it can also be run manually as `ovm codex-skew [<codex-binary>]`.
//!
//! Env contract:
//!   OVM_EVENT   — lifecycle event (e.g. `pre-launch`, `post-switch`); advisory
//!   OVM_PRODUCT — owning product (`codex`); advisory
//!   OVM_VERSION — the Codex version label being launched/activated
//!   OVM_BINARY  — path to the Codex binary to assess
//!
//! Fail-open contract: this guard is advisory and must NEVER block a launch or
//! switch. It prints at most a warning to stderr and ALWAYS exits 0, whatever
//! goes wrong (no binary, no DB, unreadable files).

use std::path::PathBuf;

fn main() {
    // Resolve the Codex binary to assess: env contract first, then a positional
    // arg for manual use. Anything missing → nothing to do, exit cleanly.
    let binary = std::env::var_os("OVM_BINARY")
        .map(PathBuf::from)
        .or_else(|| std::env::args_os().nth(1).map(PathBuf::from));

    let Some(binary) = binary else {
        return;
    };

    // Can't assess a binary that isn't there — treating an unreadable file as
    // "knows no migrations" would falsely read as degraded.
    if !binary.exists() {
        return;
    }

    let version = std::env::var("OVM_VERSION").unwrap_or_default();
    let event = std::env::var("OVM_EVENT").unwrap_or_default();

    let assessment = ovm_codex_skew::assess(&binary);

    if event == "doctor" {
        // Manual `ovm doctor codex`: a detailed report to stdout, even when clean.
        ovm_codex_skew::print_report(&version, &binary, assessment.as_ref());
    } else if let Some(assessment) = assessment {
        // Lifecycle (pre-launch / post-switch): a terse warning, only when degraded.
        if assessment.degraded() {
            ovm_codex_skew::print_degraded_warning(&version, &assessment);
        }
    }
    // Implicit exit 0 — fail-open.
}
