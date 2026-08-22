//! `ovm-codex-skew` — OVM companion plugin for Codex.
//!
//! OVM invokes this at lifecycle events (pre-launch, post-switch) with an env
//! contract; it can also be run manually as `ovm codex-skew [<codex-binary>]`.
//!
//! Env contract:
//!   OVM_EVENT          — lifecycle event (e.g. `pre-launch`, `post-switch`); advisory
//!   OVM_PRODUCT        — owning product (`codex`); advisory
//!   OVM_VERSION        — the Codex version label being launched/activated
//!   OVM_BINARY         — path to the Codex binary to assess
//!   OVM_REGISTRY_CACHE — directory where `ovm` caches registry documents; the
//!                        served `codex-skew.json` is read from there when present
//!
//! Fail-open contract: this guard is advisory and must NEVER block a launch or
//! switch. It prints at most a warning to stderr and ALWAYS exits 0, whatever
//! goes wrong (no binary, no DB, unreadable files, unusable evidence).

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    let classification_only = first.as_deref() == Some(std::ffi::OsStr::new("--classification"));

    // Resolve the Codex binary to assess: env contract first, then a positional
    // arg for manual use. Anything missing → nothing to do, exit cleanly.
    let binary = std::env::var_os("OVM_BINARY")
        .map(PathBuf::from)
        .or_else(|| {
            if classification_only {
                args.next().map(PathBuf::from)
            } else {
                first.map(PathBuf::from)
            }
        });

    let Some(binary) = binary else {
        if classification_only {
            // Reason first, class last: callers read the class with `tail -1`,
            // so the explanation rides along without changing that contract.
            println!("reason: no Codex binary supplied to assess");
            println!("indeterminate");
        }
        return;
    };

    if classification_only {
        // The static classifier the observatory records as `staticCompatibility`:
        // compiled manifest only, no served evidence — otherwise the ladder's
        // "behavioral vs static" comparison would be comparing evidence with
        // itself.
        let outcome = ovm_codex_skew::assess(&binary);
        // "indeterminate" alone cannot distinguish "the migrations were read
        // and one could not be classified" from "the state DB was unreadable"
        // — one is a property of the release, the other is broken plumbing,
        // and only the first is something a human can adjudicate. Emit the
        // reason so the caller can record which it was.
        if let ovm_codex_skew::AssessmentOutcome::Indeterminate(indeterminate) = &outcome {
            println!("reason: {}", indeterminate.reason);
        }
        println!("{}", outcome.classification());
        return;
    }

    let version = std::env::var("OVM_VERSION").unwrap_or_default();
    let event = std::env::var("OVM_EVENT").unwrap_or_default();

    let evidence = ovm_codex_skew::default_evidence_path()
        .and_then(|path| ovm_codex_skew::load_evidence(&path));
    let guard = ovm_codex_skew::guard(&binary, &version, evidence.as_ref());

    if event == "doctor" {
        // Manual `ovm doctor codex`: a detailed report to stdout, even when clean.
        ovm_codex_skew::print_report(&version, &binary, &guard);
    } else {
        match guard.launch_verdict() {
            ovm_codex_skew::LaunchVerdict::ObservedDegraded(observation) => {
                ovm_codex_skew::print_observed_warning(&version, observation);
            }
            ovm_codex_skew::LaunchVerdict::StaticDegraded(assessment) => {
                ovm_codex_skew::print_degraded_warning(&version, assessment);
            }
            ovm_codex_skew::LaunchVerdict::Silent => {}
        }
    }
    // Implicit exit 0 — fail-open.
}
