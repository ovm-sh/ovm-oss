# Feature Backlog

Planned features beyond MVP. Prioritized by user impact.

## Queue

| Feature | Description | Status |
|---------|-------------|--------|
| Linux musl builds | Static binaries for Alpine/Docker | planned |
| Auto-update detection | Notify when new versions are available | planned |
| `ovm upgrade` | Upgrade active version to latest in-place | planned |
| Shell integration | Auto-switch version per directory (`.ovm-version` file) | planned |
| Windows support | Symlink alternatives for Windows | researching |
| Built-in benchmarking | Rust-native timing without Node.js dependency | planned |
| Plugin system | User-extensible hooks and commands | deferred |
| Version aliases | `ovm alias stable 2.1.91` | planned |
| Parallel installs | Download multiple versions concurrently | planned |
| MCP topology series | Delayed-mock MCP probe (`OVM_BENCHMARK_MCP_DELAY`) measuring per-version MCP startup concurrency; publish as a benchmark series | in-progress |
| MCP startup anomaly detections | Feed entries when a version's handshake spread breaks the expected delay pattern (e.g. parallel→serial regression) | planned |
| Real-MCP calibration annotation | One-off measurement of real server startup cost published as a site annotation, not a series | planned |
| Require review only for a NEW destructive migration | Qualification pauses unless every downgrade rung's static verdict agrees with its behaviour, so each Codex stable needs a hand-written waiver (0.145.0, 0.146.0, …). Simply dropping the rung check was tried on 2026-08-01 and reverted: `static=degraded` + `behavioral=compatible` is the exact signature of silent feature loss the guard exists to catch, so removing it would let a release that drops data behind an unprobed feature qualify unreviewed. The fix is to narrow *when* review fires, not to delete it — a rung disagreement caused by an already-reviewed breaking migration is expected, while one caused by a destructive migration this release introduces is not. Needs a per-version migration set in the ladder output (the recorded `observe` is shared post-migration DB state, so it cannot answer this today) and a comparison against the previous stable's `destructiveMigrations` fingerprint | planned (post-0.1.0) |
| Flaky test triage under load | `launch_perf` was fixed (median-of-3), but an `ovm-claudex` test also failed once under heavy parallel load on 2026-07-31 and passed standalone; the pre-commit hook did not surface which test. Capture the failing test name in hook output, then make the timing-sensitive proxy tests load-independent. (First named instance: `purge_refuses_a_piped_yes_even_from_a_terminal` lost its child-exit race under the release profile on the CI ubuntu leg during the v0.1.0 cut and was fixed to treat the broken pipe as the refusal it asserts) | planned (post-0.1.0) |
| Per-file Rust coverage floors | One aggregate 70% threshold hides weak modules: `src/node.rs` 0%, `commands/archive.rs` 0%, `commands/doctor.rs` 12%, `commands/which.rs` 26%. Add per-file (or per-module) floors and cover those paths, then raise the workspace floor 70% → 75% | planned (post-0.1.0) |
| Coverage trend tracking | LCOV is retained per run but not summarized on PRs or tracked over time, so a slow decline is invisible until it crosses the floor | planned (post-0.1.0) |
| Per-product site pages | A page per managed product (Claude, Codex, Pi) collecting per-version observations — macOS Gatekeeper revocations, schema migrations, sidecar changes, behavioural regressions — so findings live next to the product instead of only in devlog entries | planned (post-0.1.0) |
| Surface Gatekeeper state to users | Publish per-version macOS signature status in the registry and show it beside the version in `ls`/`info`/picker. Advisory, never blocking — a revoked version stays selectable but is labelled | planned (post-0.1.0) |
| Hero video replacing the terminal replay | Replace `.term-replay` (the live re-typed claudex terminal, currently mobile-only) with a recorded video in the same vein as `/claudex`'s `media/claudex-demo-v3.mp4`: walk the real story, then show real use. Two open decisions: (a) placement — the hero left column is full (wordmark, lede, install, dev log), so a desktop video either displaces one of those or becomes a tab inside the benchmark card, sharing the space and play control the chart already owns; (b) whether it swaps WITH the chart rather than sitting beside it. Needs a real capture pass on a real machine (same production route as the claudex tape), a poster frame for link unfurls, and a size budget — the claudex tape is 628KB against the replay's ~10KB, which is why the replay exists at all. Until then the replay stays as the mobile lead visual. |
| Terminal replay on desktop | The replay is revealed only under `@media (max-width: 800px)`; desktop shows the chart instead, deliberately. If the hero video lands this row dies with it. |
| Real Quelpaw capture for the story/video | Record a real `ovm switch` between two Claude versions (e.g. latest → 2.1.96), run the real `/buddy` in the TUI, and capture the actual generated Quelpaw — footage for the origin-story video and possibly a frame set `ovm story` could replay. Needs a TUI recording pass (VHS or asciinema against the real session) and a decision on showing a live generative response on camera | idea |
| Admit degraded stables to the prerelease channel with a warning | Today the verification gate withholds a new stable Codex outright when its behavioral qualification is `degraded`, making it invisible until evidence turns compatible — an operator cannot install it through the registry surfaces to probe the degradation by hand. Design (2026-08-19): when every failing lane reason is `=degraded` (no `broken`, no `indeterminate`), keep the version entry but stamp `"channel": "prerelease"` plus `"warning": {"kind": "behavioral-degraded", "summary": <withheld_summary>}`; emit a `gate-demoted` detection (publish.ts collapses unknown `gate-*` kinds into the gate family already). The gate must then retarget `latest`/registry.json `latest` away from any warned entry to the newest unwarned survivor, and clear the stamp on a later all-compatible run (stamps re-derive from the cumulative ledger each gate run, so no persistence machinery is needed). Client side: parse the optional `warning` field in `sources/registry.rs` (unknown fields are already ignored by old clients), print a yellow banner on `ovm install`/`use` of a warned version, and tag it in the picker. Revocation semantics for already-published versions move the same way (demote, not revoke) or they flip-flop between runs. Deliberately not built now: the one occurrence (0.148.0, 2026-08-18) was a contract false positive, fixed at the source; build this the first time a genuine degrade parks a release | design only |
| Always write `latest_prerelease` in the codex registry | `update-registry.sh` omits `dist_tags.latest_prerelease` whenever the newest version overall is a stable, so after the 2026-08-18 refresh the tag vanished; the hosted version-watch fallback reads that key for `ours_codex_alpha` and would perceive a permanent codex-alpha change during a mini outage (harmless extra dispatches, but noisy). Compute it as the newest prerelease entry and write it whenever any prerelease exists | planned |
| Mockable `latest` resolution for tests | `bootstrap_first_launch`'s install-latest path works (trace: resolves, downloads, activates, launches) but cannot be tested unattended: `latest` resolution ignores `OVM_CODEX_RELEASES_URL`/`OVM_REGISTRY_BASE_URL` and reaches the real registry, so the test installs a real Codex which then demands a TTY. Add a resolution seam the mocks can intercept, then un-ignore `first_launch_installs_latest_when_the_machine_has_nothing` | planned |

## Process

1. New feature ideas go in the **Queue** table above
2. When work begins, move to `in-progress` status
3. When shipped, move the row to `docs/features/archive/` with a date and brief summary
4. Update `CHANGELOG.md` with the user-facing change

## Archive

Shipped features are documented in [`archive/`](archive/).
