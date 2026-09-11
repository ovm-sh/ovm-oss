#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HELPER="$ROOT/scripts/bundle-manifest.sh"
MANIFEST="$ROOT/crates/ovm/ovm-bundle-v1.tsv"
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

"$HELPER" validate "$MANIFEST"
binaries=$("$HELPER" binaries "$MANIFEST" | tr '\n' ' ' | sed 's/ $//')
[[ "$binaries" == "ovm ovm-codex-skew ovm-claudex ovm-limits" ]] || { echo "ASSERT FAILED at -e:12" >&2; exit 1; }
side_packages=$("$HELPER" side-packages "$MANIFEST" | tr '\n' ' ' | sed 's/ $//')
[[ "$side_packages" == "ovm-codex-skew ovm-claudex ovm-limits" ]] || { echo "ASSERT FAILED at -e:14" >&2; exit 1; }
[[ $("$HELPER" main-package "$MANIFEST") == "ovm" ]] || { echo "ASSERT FAILED at -e:15" >&2; exit 1; }

cat > "$TMP_DIR/future.tsv" <<'EOF'
ovm-bundle-v1
main	ovm	ovm
side	ovm-codex-skew	ovm-codex-skew
side	ovm-claudex	ovm-claudex
side	ovm-future	ovm-future
EOF
"$HELPER" validate "$TMP_DIR/future.tsv"
[[ $("$HELPER" binaries "$TMP_DIR/future.tsv" | wc -l | tr -d ' ') == "4" ]] || { echo "ASSERT FAILED at -e:25" >&2; exit 1; }

cat > "$TMP_DIR/duplicate.tsv" <<'EOF'
ovm-bundle-v1
main	ovm	ovm
side	ovm-side	ovm-side
side	ovm-side	ovm-other
EOF
if "$HELPER" validate "$TMP_DIR/duplicate.tsv" 2>/dev/null; then
    echo "duplicate binary unexpectedly accepted" >&2
    exit 1
fi

cat > "$TMP_DIR/unsafe.tsv" <<'EOF'
ovm-bundle-v1
main	ovm	ovm
side	../ovm-side	ovm-side
EOF
if "$HELPER" validate "$TMP_DIR/unsafe.tsv" 2>/dev/null; then
    echo "unsafe binary unexpectedly accepted" >&2
    exit 1
fi

cat > "$TMP_DIR/no-main.tsv" <<'EOF'
ovm-bundle-v1
side	ovm-side	ovm-side
EOF
if "$HELPER" validate "$TMP_DIR/no-main.tsv" 2>/dev/null; then
    echo "manifest without main unexpectedly accepted" >&2
    exit 1
fi

# Exercise version inheritance through Cargo, then run the real release script
# against a throwaway workspace. Git writes and compilation are stubbed; metadata
# remains real so a version bump has to reach every package and Cargo.lock.
python3 - "$ROOT" "$TMP_DIR" <<'PY_VERSIONS'
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

root, temporary = map(Path, sys.argv[1:])
fixture = temporary / "version-workspace"
fixture.mkdir()
shutil.copy(root / "Cargo.toml", fixture / "Cargo.toml")
shutil.copy(root / "README.md", fixture / "README.md")
packages = subprocess.check_output(
    [str(root / "scripts/bundle-manifest.sh"), "packages"], text=True, cwd=root
).splitlines()
# Every workspace member needs a stub, not only the bundle's binaries: the
# copied root Cargo.toml lists them all, and a library crate the binaries
# share (ovm-tui) is a member without being a package the bundle ships.
import re
members = re.search(r"members\s*=\s*\[(.*?)\]", (root / "Cargo.toml").read_text(), re.S).group(1)
members = [m.strip().strip('"').removeprefix("crates/") for m in members.split(",") if m.strip()]
assert set(packages) <= set(members), (packages, members)
for package in members:
    crate = fixture / "crates" / package
    (crate / "src").mkdir(parents=True)
    # Preserve the real package version declaration, while removing dependencies
    # and test targets that this metadata-only fixture does not need.
    source = (root / "crates" / package / "Cargo.toml").read_text()
    package_section = source.split("\n[", 1)[0]
    assert "version.workspace = true" in package_section, package
    (crate / "Cargo.toml").write_text(package_section + "\n")
    if package in packages:
        (crate / "src/main.rs").write_text("fn main() {}\n")
    else:
        (crate / "src/lib.rs").write_text("")
real_cargo = shutil.which("cargo")
assert real_cargo, "cargo is required for the version contract"
subprocess.run([real_cargo, "generate-lockfile", "--offline"], cwd=fixture, check=True)
metadata_args = [real_cargo, "metadata", "--offline", "--no-deps", "--format-version", "1"]
initial = json.loads(subprocess.check_output(metadata_args, cwd=fixture))
initial_versions = {p["version"] for p in initial["packages"]}
assert len(initial_versions) == 1, initial_versions
initial_version = initial_versions.pop()

scripts = fixture / "scripts"
scripts.mkdir()
for name in ("release.sh", "bundle-manifest.sh"):
    shutil.copy(root / "scripts" / name, scripts / name)
shutil.copy(root / "crates/ovm/ovm-bundle-v1.tsv", fixture / "crates/ovm/ovm-bundle-v1.tsv")
(fixture / "CHANGELOG.md").write_text("## [999.1.0]\n")
bin_dir = fixture / "test-bin"
bin_dir.mkdir()
(bin_dir / "git").write_text("#!/bin/sh\nif [ \"$1\" = rev-parse ]; then echo main; fi\n")
# Put another package first to prove release version selection does not depend
# on Cargo's package ordering. Every metadata invocation still runs real Cargo.
(bin_dir / "cargo").write_text("""#!/usr/bin/env python3
import json, os, subprocess, sys
if sys.argv[1] == 'update':
    raise SystemExit(subprocess.call([os.environ['REAL_CARGO'], *sys.argv[1:]]))
if sys.argv[1] == 'metadata':
    result = subprocess.run([os.environ['REAL_CARGO'], *sys.argv[1:]], capture_output=True)
    if result.returncode:
        sys.stderr.buffer.write(result.stderr)
        raise SystemExit(result.returncode)
    data = json.loads(result.stdout)
    data['packages'].insert(0, {'name': 'unrelated', 'version': '777.8.9'})
    print(json.dumps(data))
""")
for path in bin_dir.iterdir():
    path.chmod(0o755)
env = dict(os.environ, PATH=str(bin_dir) + os.pathsep + os.environ["PATH"], REAL_CARGO=real_cargo)
result = subprocess.run(
    ["sh", str(scripts / "release.sh"), "999.1.0"], cwd=fixture,
    env=env, input="y\n", capture_output=True, text=True,
)
assert result.returncode == 0, result.stdout + result.stderr
assert f"Current version: {initial_version}" in result.stdout, result.stdout
updated = json.loads(subprocess.check_output([*metadata_args, "--locked"], cwd=fixture))
assert {p["version"] for p in updated["packages"]} == {"999.1.0"}, updated
assert 'version = "999.1.0"' in (fixture / "Cargo.lock").read_text()
# An invalid version must fail before changing the workspace version.
manifest_before = (fixture / "Cargo.toml").read_text()
invalid = subprocess.run(
    ["sh", str(scripts / "release.sh"), "1.2.3/bad"], cwd=fixture,
    env=env, input="y\n", capture_output=True, text=True,
)
assert invalid.returncode != 0, invalid.stdout
assert (fixture / "Cargo.toml").read_text() == manifest_before
PY_VERSIONS

echo "bundle-manifest: ok"
