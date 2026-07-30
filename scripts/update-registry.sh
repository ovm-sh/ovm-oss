#!/usr/bin/env bash
# Update the OVM version registry at docs/api/
#
# Fetches version lists from upstream sources and writes static JSON files
# that are served at ovm.sh/api/
#
# Usage:
#   ./scripts/update-registry.sh           # update all products
#   ./scripts/update-registry.sh claude     # update one product
#
# Output files:
#   docs/api/registry.json              # index of all products
#   docs/api/claude.json                # claude versions + dates
#   docs/api/codex.json                 # codex versions + dates
#
# These are deployed to GitHub Pages and fetched by `ovm switch` etc.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
API_DIR="$SCRIPT_DIR/../docs/api"
mkdir -p "$API_DIR"

if [ $# -eq 0 ]; then
    PRODUCTS=(claude codex pi cliproxyapi)
else
    PRODUCTS=("$@")
fi
TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)
# TIMESTAMP is interpolated into inline Python below — refuse anything that
# isn't a plain ISO-8601 UTC stamp (a hostile TZ/locale could smuggle code).
if ! [[ "$TIMESTAMP" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]]; then
    echo "error: unexpected timestamp format: $TIMESTAMP" >&2
    exit 1
fi

update_claude() {
    echo "  Updating claude..."

    python3 -c "
import json, subprocess, sys

def parse_semver(v):
    parts = v.split('.')
    if len(parts) != 3: return (0, 0, 0)
    try: return tuple(int(p) for p in parts)
    except: return (0, 0, 0)

def load_previous(path):
    try:
        with open(path) as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return None

def compute_retired_versions(previous, current_versions, sort_key):
    if not previous:
        return []
    current = {entry['version'] for entry in current_versions}
    previous_versions = {
        entry['version']: entry
        for entry in previous.get('versions', [])
    }
    retired = {
        entry['version']: entry
        for entry in previous.get('retired_versions', [])
        if entry.get('version') not in current
    }
    for version, entry in previous_versions.items():
        if version in current or version in retired:
            continue
        retired[version] = {
            'version': version,
            'date': entry.get('date', ''),
            'last_seen_at': previous.get('updated_at', ''),
            'retired_at': '$TIMESTAMP',
        }
    return sorted(retired.values(), key=lambda entry: sort_key(entry['version']))

def write_if_changed(path, registry):
    previous = load_previous(path)
    if previous:
        old_semantic = {k: v for k, v in previous.items() if k != 'updated_at'}
        new_semantic = {k: v for k, v in registry.items() if k != 'updated_at'}
        if old_semantic == new_semantic:
            return previous
    with open(path, 'w') as f:
        json.dump(registry, f, indent=2)
        f.write('\n')
    return registry

# Fetch version list
r1 = subprocess.run(
    ['npm', 'view', '@anthropic-ai/claude-code', 'versions', '--json'],
    capture_output=True, text=True, timeout=30
)
if r1.returncode != 0:
    raise SystemExit(f'npm versions lookup failed: {r1.stderr}')
version_list = json.loads(r1.stdout)

# Fetch timestamps
r2 = subprocess.run(
    ['npm', 'view', '@anthropic-ai/claude-code', 'time', '--json'],
    capture_output=True, text=True, timeout=30
)
if r2.returncode != 0:
    raise SystemExit(f'npm time lookup failed: {r2.stderr}')
time_data = json.loads(r2.stdout)

# Fetch dist-tags
r3 = subprocess.run(
    ['npm', 'view', '@anthropic-ai/claude-code', 'dist-tags', '--json'],
    capture_output=True, text=True, timeout=30
)
if r3.returncode != 0:
    raise SystemExit(f'npm dist-tags lookup failed: {r3.stderr}')
dist_tags = json.loads(r3.stdout)

# Merge: version list is canonical, dates from time_data
versions = []
for v in sorted(version_list, key=parse_semver):
    entry = {'version': v}
    if v in time_data:
        entry['date'] = time_data[v][:10]
    versions.append(entry)

path = '$API_DIR/claude.json'
previous = load_previous(path)
retired_versions = compute_retired_versions(previous, versions, parse_semver)
registry = {
    'product': 'claude',
    'display_name': 'Claude Code',
    'source': 'npm:@anthropic-ai/claude-code',
    'updated_at': '$TIMESTAMP',
    'dist_tags': dist_tags,
    'versions': versions,
}
if retired_versions:
    registry['retired_versions'] = retired_versions

registry = write_if_changed(path, registry)

print(f'  {len(registry.get(\"versions\", []))} claude versions written')
"
}

update_codex() {
    echo "  Updating codex..."

    python3 -c "
import json, os, re, subprocess, sys, time, urllib.request

SEMVER_RE = re.compile(r'^rust-v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$')
CODEX_ASSET_RE = re.compile(r'^codex-(?:aarch64|x86_64)-(?:apple-darwin(?:-unsigned)?|unknown-linux-musl)\.tar\.gz$')

def github_headers():
    headers = {'User-Agent': 'ovm-registry'}
    token = os.environ.get('GITHUB_TOKEN')
    if token:
        headers['Authorization'] = f'Bearer {token}'
    return headers

def load_previous(path):
    try:
        with open(path) as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return None

def compute_retired_versions(previous, current_versions, sort_key):
    if not previous:
        return []
    current = {entry['version'] for entry in current_versions}
    previous_versions = {
        entry['version']: entry
        for entry in previous.get('versions', [])
    }
    retired = {
        entry['version']: entry
        for entry in previous.get('retired_versions', [])
        if entry.get('version') not in current
    }
    for version, entry in previous_versions.items():
        if version in current or version in retired:
            continue
        retired[version] = {
            'version': version,
            'date': entry.get('date', ''),
            'last_seen_at': previous.get('updated_at', ''),
            'retired_at': '$TIMESTAMP',
        }
    return sorted(retired.values(), key=lambda entry: sort_key(entry['version']))

def write_if_changed(path, registry):
    previous = load_previous(path)
    if previous:
        old_semantic = {k: v for k, v in previous.items() if k != 'updated_at'}
        new_semantic = {k: v for k, v in registry.items() if k != 'updated_at'}
        if old_semantic == new_semantic:
            return previous
    with open(path, 'w') as f:
        json.dump(registry, f, indent=2)
        f.write('\n')
    return registry

def is_installable_codex_release(release):
    tag = release.get('tag_name', '')
    if not SEMVER_RE.match(tag):
        return False
    return any(CODEX_ASSET_RE.match(asset.get('name', '')) for asset in release.get('assets', []))

def semver_key(tag):
    value = tag.removeprefix('rust-v')
    value = value.split('+', 1)[0]
    core, _, prerelease = value.partition('-')
    major, minor, patch = [int(part) for part in core.split('.')]
    if not prerelease:
        return (major, minor, patch, 1, [])
    prerelease_key = [
        (0, int(part)) if part.isdigit() else (1, part)
        for part in prerelease.split('.')
    ]
    return (major, minor, patch, 0, prerelease_key)

# Fetch all releases from GitHub API (paginated)
versions = []
page = 1
while True:
    url = f'https://api.github.com/repos/openai/codex/releases?per_page=100&page={page}'
    req = urllib.request.Request(url, headers=github_headers())
    # Transient GitHub 5xx/timeouts must not fail the whole refresh
    # (2026-07-29: a single 504 redded the nightly publish).
    releases = None
    for attempt in range(4):
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                releases = json.loads(resp.read())
            break
        except Exception as e:
            if attempt == 3:
                print(f'GitHub API error after {attempt + 1} attempts: {e}', file=sys.stderr)
                raise SystemExit(1)
            print(f'GitHub API error (attempt {attempt + 1}, retrying): {e}', file=sys.stderr)
            time.sleep(5 * 2 ** attempt)

    if not releases:
        break

    for r in releases:
        tag = r.get('tag_name', '')
        date = (r.get('published_at') or '')[:10]
        if is_installable_codex_release(r):
            versions.append({'version': tag, 'date': date})

    page += 1

if not versions:
    raise SystemExit('No installable Codex versions found; refusing to overwrite registry')

versions.sort(key=lambda entry: semver_key(entry['version']))
stable_versions = [
    entry['version']
    for entry in versions
    if '-' not in entry['version'].removeprefix('rust-v')
]
latest_any = max((entry['version'] for entry in versions), key=semver_key) if versions else ''
latest_stable = max(stable_versions, key=semver_key) if stable_versions else latest_any
dist_tags = {'latest': latest_stable}
if latest_any and latest_any != latest_stable:
    dist_tags['latest_prerelease'] = latest_any

path = '$API_DIR/codex.json'
previous = load_previous(path)
retired_versions = compute_retired_versions(previous, versions, semver_key)
registry = {
    'product': 'codex',
    'display_name': 'Codex',
    'source': 'github:openai/codex',
    'updated_at': '$TIMESTAMP',
    'dist_tags': dist_tags,
    'versions': versions,
}
if retired_versions:
    registry['retired_versions'] = retired_versions

registry = write_if_changed(path, registry)

print(f'  {len(registry.get(\"versions\", []))} codex versions written')
"
}

write_index() {
    echo "  Writing registry index..."

    python3 -c "
import json, os, glob

api_dir = '$API_DIR'
products = []

for path in sorted(glob.glob(os.path.join(api_dir, '*.json'))):
    name = os.path.basename(path)
    if name == 'registry.json':
        continue
    try:
        with open(path) as f:
            data = json.load(f)
        products.append({
            'product': data['product'],
            'display_name': data['display_name'],
            'source': data['source'],
            'latest': data.get('dist_tags', {}).get('latest', ''),
            'version_count': len(data.get('versions', [])),
            'retired_count': len(data.get('retired_versions', [])),
            'updated_at': data.get('updated_at', ''),
        })
    except (json.JSONDecodeError, KeyError) as e:
        print(f'  Warning: skipping {name}: {e}')

index = {
    'schema_version': 1,
    'updated_at': '$TIMESTAMP',
    'base_url': 'https://ovm.sh/api',
    'products': products,
}

path = os.path.join(api_dir, 'registry.json')
try:
    with open(path) as f:
        previous = json.load(f)
except (FileNotFoundError, json.JSONDecodeError):
    previous = None

if previous:
    old_semantic = {k: v for k, v in previous.items() if k != 'updated_at'}
    new_semantic = {k: v for k, v in index.items() if k != 'updated_at'}
    if old_semantic == new_semantic:
        index = previous
    else:
        with open(path, 'w') as f:
            json.dump(index, f, indent=2)
            f.write('\n')
else:
    with open(path, 'w') as f:
        json.dump(index, f, indent=2)
        f.write('\n')

print(f'  {len(products)} products indexed')
"
}

echo ""
echo "  OVM Version Registry Update"
echo "  ============================"
echo ""

update_pi() {
    echo "  Updating pi..."

    python3 -c "
import json, os, sys, time, urllib.request

def github_headers():
    headers = {'User-Agent': 'ovm-registry'}
    token = os.environ.get('GITHUB_TOKEN')
    if token:
        headers['Authorization'] = f'Bearer {token}'
    return headers

def parse_pi_version(v):
    try:
        return tuple(int(part) for part in v.split('.'))
    except ValueError:
        return (0, 0, 0)

def load_previous(path):
    try:
        with open(path) as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return None

def compute_retired_versions(previous, current_versions):
    if not previous:
        return []
    current = {entry['version'] for entry in current_versions}
    previous_versions = {
        entry['version']: entry
        for entry in previous.get('versions', [])
    }
    retired = {
        entry['version']: entry
        for entry in previous.get('retired_versions', [])
        if entry.get('version') not in current
    }
    for version, entry in previous_versions.items():
        if version in current or version in retired:
            continue
        retired[version] = {
            'version': version,
            'date': entry.get('date', ''),
            'last_seen_at': previous.get('updated_at', ''),
            'retired_at': '$TIMESTAMP',
        }
    return sorted(retired.values(), key=lambda entry: parse_pi_version(entry['version']))

def write_if_changed(path, registry):
    previous = load_previous(path)
    if previous:
        old_semantic = {k: v for k, v in previous.items() if k != 'updated_at'}
        new_semantic = {k: v for k, v in registry.items() if k != 'updated_at'}
        if old_semantic == new_semantic:
            return previous
    with open(path, 'w') as f:
        json.dump(registry, f, indent=2)
        f.write('\n')
    return registry

versions = []
page = 1
while True:
    url = f'https://api.github.com/repos/earendil-works/pi/releases?per_page=100&page={page}'
    req = urllib.request.Request(url, headers=github_headers())
    # Transient GitHub 5xx/timeouts must not fail the whole refresh
    # (2026-07-29: a single 504 redded the nightly publish).
    releases = None
    for attempt in range(4):
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                releases = json.loads(resp.read())
            break
        except Exception as e:
            if attempt == 3:
                print(f'GitHub API error after {attempt + 1} attempts: {e}', file=sys.stderr)
                raise SystemExit(1)
            print(f'GitHub API error (attempt {attempt + 1}, retrying): {e}', file=sys.stderr)
            time.sleep(5 * 2 ** attempt)

    if not releases:
        break

    for r in releases:
        tag = r.get('tag_name', '')
        date = (r.get('published_at') or '')[:10]
        if tag:
            # Strip v prefix for storage
            version = tag.lstrip('v') if tag.startswith('v') else tag
            versions.append({'version': version, 'date': date})

    page += 1

if not versions:
    raise SystemExit('No Pi versions found; refusing to overwrite registry')

versions.reverse()

path = '$API_DIR/pi.json'
previous = load_previous(path)
retired_versions = compute_retired_versions(previous, versions)
registry = {
    'product': 'pi',
    'display_name': 'Pi',
    'source': 'github:earendil-works/pi',
    'updated_at': '$TIMESTAMP',
    'dist_tags': {'latest': versions[-1]['version'] if versions else ''},
    'versions': versions,
}
if retired_versions:
    registry['retired_versions'] = retired_versions

registry = write_if_changed(path, registry)

print(f'  {len(registry.get(\"versions\", []))} pi versions written')
"
}

update_cliproxyapi() {
    echo "  Updating cliproxyapi..."

    python3 -c "
import json, os, re, sys, time, urllib.request

# Only releases that actually ship a platform tarball are installable by the
# managed claudex proxy; source-only tags must not enter the registry.
ASSET_RE = re.compile(r'^CLIProxyAPI_[0-9][^_]*_(?:darwin|linux)_(?:aarch64|amd64)\.tar\.gz$')
# The managed proxy tracks stable releases: claudex compares versions with
# semver and installs bare MAJOR.MINOR.PATCH builds, so skip dev/pre tags.
STABLE_RE = re.compile(r'^\d+\.\d+\.\d+$')

def github_headers():
    headers = {'User-Agent': 'ovm-registry'}
    token = os.environ.get('GITHUB_TOKEN')
    if token:
        headers['Authorization'] = f'Bearer {token}'
    return headers

def parse_version(v):
    try:
        return tuple(int(part) for part in v.split('.'))
    except ValueError:
        return (0, 0, 0)

def load_previous(path):
    try:
        with open(path) as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return None

def compute_retired_versions(previous, current_versions):
    if not previous:
        return []
    current = {entry['version'] for entry in current_versions}
    previous_versions = {
        entry['version']: entry
        for entry in previous.get('versions', [])
    }
    retired = {
        entry['version']: entry
        for entry in previous.get('retired_versions', [])
        if entry.get('version') not in current
    }
    for version, entry in previous_versions.items():
        if version in current or version in retired:
            continue
        retired[version] = {
            'version': version,
            'date': entry.get('date', ''),
            'last_seen_at': previous.get('updated_at', ''),
            'retired_at': '$TIMESTAMP',
        }
    return sorted(retired.values(), key=lambda entry: parse_version(entry['version']))

def write_if_changed(path, registry):
    previous = load_previous(path)
    if previous:
        old_semantic = {k: v for k, v in previous.items() if k != 'updated_at'}
        new_semantic = {k: v for k, v in registry.items() if k != 'updated_at'}
        if old_semantic == new_semantic:
            return previous
    with open(path, 'w') as f:
        json.dump(registry, f, indent=2)
        f.write('\n')
    return registry

def is_installable(release):
    return any(ASSET_RE.match(asset.get('name', '')) for asset in release.get('assets', []))

versions = []
page = 1
while True:
    url = f'https://api.github.com/repos/router-for-me/CLIProxyAPI/releases?per_page=100&page={page}'
    req = urllib.request.Request(url, headers=github_headers())
    # Transient GitHub 5xx/timeouts must not fail the whole refresh
    # (2026-07-29: a single 504 redded the nightly publish).
    releases = None
    for attempt in range(4):
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                releases = json.loads(resp.read())
            break
        except Exception as e:
            if attempt == 3:
                print(f'GitHub API error after {attempt + 1} attempts: {e}', file=sys.stderr)
                raise SystemExit(1)
            print(f'GitHub API error (attempt {attempt + 1}, retrying): {e}', file=sys.stderr)
            time.sleep(5 * 2 ** attempt)

    if not releases:
        break

    for r in releases:
        tag = r.get('tag_name', '')
        date = (r.get('published_at') or '')[:10]
        # Tags are vMAJOR.MINOR.PATCH; store the bare version.
        version = tag[1:] if tag.startswith('v') else tag
        if STABLE_RE.match(version) and is_installable(r):
            versions.append({'version': version, 'date': date})

    page += 1

if not versions:
    raise SystemExit('No installable CLIProxyAPI versions found; refusing to overwrite registry')

versions.sort(key=lambda entry: parse_version(entry['version']))

path = '$API_DIR/cliproxyapi.json'
previous = load_previous(path)
retired_versions = compute_retired_versions(previous, versions)
registry = {
    'product': 'cliproxyapi',
    'display_name': 'CLIProxyAPI',
    'source': 'github:router-for-me/CLIProxyAPI',
    'updated_at': '$TIMESTAMP',
    'dist_tags': {'latest': versions[-1]['version'] if versions else ''},
    'versions': versions,
}
if retired_versions:
    registry['retired_versions'] = retired_versions

registry = write_if_changed(path, registry)

print(f'  {len(registry.get(\"versions\", []))} cliproxyapi versions written')
"
}

for product in "${PRODUCTS[@]}"; do
    case "$product" in
        claude) update_claude ;;
        codex)  update_codex ;;
        pi)     update_pi ;;
        cliproxyapi) update_cliproxyapi ;;
        *)      echo "  Unknown product: $product" ;;
    esac
done

write_index

echo ""
echo "  Done. Files at: $API_DIR/"
ls -la "$API_DIR"/*.json
echo ""
