# Security Policy

## Supported Versions

OVM is pre-1.0. Only the latest release receives security fixes.

## Reporting a Vulnerability

**Please do not open public GitHub issues for security problems.**

Report security concerns privately through [GitHub Security Advisories](https://github.com/ovm-sh/ovm-oss/security/advisories/new).

Include:
- A description of the issue
- Steps to reproduce
- Affected version(s)
- Any proof-of-concept code

We'll acknowledge within 72 hours and work with you on a disclosure timeline.

## Scope

In scope:
- Arbitrary code execution via malicious version manifests or plugins
- Credential / token leakage through OVM's storage or logs
- Path-traversal via crafted version names or archive contents
- Registry or upstream-API injection attacks

Out of scope:
- Vulnerabilities in downloaded Claude Code / Codex / Pi binaries (report those upstream)
- Social-engineering against the maintainers
- Issues in dependencies — please report to the upstream project

## Safe Defaults

- OVM only executes binaries placed by the user (via `install`) or picked up from `$PATH` (plugins)
- Downloads are verified by SHA-256 hash where upstream provides one; npm tarballs are verified against the registry's SHA-512 Subresource-Integrity metadata
- On macOS, downloaded Claude Code and Codex binaries are verified with `codesign` against the publisher's expected Apple Developer ID team (Anthropic / OpenAI) before install; a missing or mismatched signature aborts the install (bypass with `OVM_SKIP_SIGNATURE_VERIFY=1`)
- Download URLs from release/registry metadata are restricted to HTTPS and an allow-list of expected hosts, and redirects may not downgrade off HTTPS
- No arbitrary code from version manifests is executed — manifests are data only
- All archive extraction validates entry paths against traversal attacks (`..` components and absolute paths are rejected) and rejects symlink/hardlink/special entries
- npm installs use `--ignore-scripts` to prevent post-install code execution from packages
- All HTTP traffic uses HTTPS with configured timeouts — with one deliberate exception: claudex's launcher talks plain HTTP to its own CLIProxyAPI sidecar on `127.0.0.1` only (loopback never leaves the machine; the sidecar itself speaks HTTPS upstream). The sidecar binds localhost-only with a random per-install key, and the launcher refuses to send traffic to any listener that doesn't authenticate as ours
- No user input is interpolated into shell commands — binary paths are resolved, not constructed from strings
- Pre-commit hooks block leaked filesystem paths and common secret patterns before every commit

## Verifying Release Provenance

OVM's own release archives carry a GitHub build-provenance attestation, signed
keylessly by the release workflow. To verify an archive was built by this
project's CI (requires an authenticated [GitHub CLI](https://cli.github.com/)):

```bash
gh attestation verify ovm-<target>.tar.gz --repo ovm-sh/ovm-oss
```

`install.sh` verifies the SHA-256 checksum but intentionally does **not** invoke
`gh` on your behalf; run the command above yourself for provenance verification.
