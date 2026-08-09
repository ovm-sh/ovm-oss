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
| Truthful `clean --all` wording | `ovm clean <p> --all` prints `✓ Cleaned all <p> versions, freed 0 B` while removing only cached artifacts and leaving the installed version — the command is doing its documented job (cache removal) but the message claims version removal, the same absence-rendered-as-a-verdict shape the devlogs keep cataloguing. Say what was actually cleaned ("cached artifacts for N versions") | planned |
| A way to fully leave a product | `uninstall` refuses the active version and `use` demands another installed version to switch to, so removing a product's only install requires `rm -rf ~/.ovm/products/<p>` by hand (hit for real on 2026-08-08). Add `ovm uninstall <p> --all` (or let uninstalling the last version clear the selection) with a confirmation | planned |
| `install` should mention an existing foreign install | `ovm install <p> latest` with an unmanaged install on PATH installs cleanly but says nothing about the foreign copy, so the user never sees the adoption/cleanup guidance that `launch`/`adopt` print. A one-line pointer ("an unmanaged <p> exists at <path>; see `ovm adopt`") closes the gap without changing behavior | planned |
| Real Quelpaw capture for the story/video | Record a real `ovm switch` between two Claude versions (e.g. latest → 2.1.96), run the real `/buddy` in the TUI, and capture the actual generated Quelpaw — footage for the origin-story video and possibly a frame set `ovm story` could replay. Needs a TUI recording pass (VHS or asciinema against the real session) and a decision on showing a live generative response on camera | idea |
| Mockable `latest` resolution for tests | `bootstrap_first_launch`'s install-latest path works (trace: resolves, downloads, activates, launches) but cannot be tested unattended: `latest` resolution ignores `OVM_CODEX_RELEASES_URL`/`OVM_REGISTRY_BASE_URL` and reaches the real registry, so the test installs a real Codex which then demands a TTY. Add a resolution seam the mocks can intercept, then un-ignore `first_launch_installs_latest_when_the_machine_has_nothing` | planned |

## Process

1. New feature ideas go in the **Queue** table above
2. When work begins, move to `in-progress` status
3. When shipped, move the row to `docs/features/archive/` with a date and brief summary
4. Update `CHANGELOG.md` with the user-facing change

## Archive

Shipped features are documented in [`archive/`](archive/).
