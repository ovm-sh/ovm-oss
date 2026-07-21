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

## Process

1. New feature ideas go in the **Queue** table above
2. When work begins, move to `in-progress` status
3. When shipped, move the row to `docs/features/archive/` with a date and brief summary
4. Update `CHANGELOG.md` with the user-facing change

## Archive

Shipped features are documented in [`archive/`](archive/).
