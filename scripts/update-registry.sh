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
import datetime
import json, os, re, subprocess, sys

# npm guarantees semver for every published version, so a version entry that
# does not parse as semver is not a version — it is an error body wearing a
# list ([\"error\"], [\"temporarily unavailable\"]). Those pass every shape check
# there is (a non-empty list of non-empty unpadded strings), and then no real
# release appears in 'current' and all 472 published versions retire in one
# write.
#
# This is the official semver grammar, not an approximation of it, because npm
# enforces the real thing on publish: numeric segments without leading zeros,
# ASCII digits only, prerelease and build as dot-separated NONEMPTY identifiers.
# The loose version accepted '01.2.3', '1.2.3-01', '1.2.3-alpha..1', '1.2.3-.'
# and — via \d, which matches every Unicode decimal digit — '1.2.٣'. npm can
# publish none of them, so each one reaching here means the answer is not a
# version list, which is the one thing this check exists to notice. Unlike the
# registry gate's deliberately loose rule, tightening here cannot brick a
# refresh: what it refuses, npm cannot have published.
# (All 472 currently published claude versions match.)
NPM_SEMVER_RE = re.compile(
    r'^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)'
    r'(?:-(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'
    r'(?:\.(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*)?'
    r'(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$'
)

# A date is a date, or it is absent. 'date' is already optional in a registry
# entry, so absence is a legal state every consumer handles — but a value that
# merely LOOKS sliceable is not: npm answering {'2.1.220': 'temporarily
# unavailable'} wrote date 'temporaril', a field that is not a date and that no
# reader can tell from one. Same deliberate choice as the non-string case
# below: drop the field rather than refuse the run or write nonsense into it.
def leading_calendar_date(value):
    # A shape check alone still admitted 2026-99-99; only a value whose first
    # ten characters parse as a real calendar date is a date.
    try:
        return datetime.date.fromisoformat(value[:10]).isoformat()
    except (ValueError, TypeError):
        return None

# A refresh only ever sees the listing it was handed, and cannot prove that
# listing complete from this side: an error body shaped like a version list, a
# paginated walk truncated by one transient empty page, an upstream index
# rebuilding — each is indistinguishable from 'upstream unpublished
# everything', and each retires the missing releases in a single write. So the
# protection is placed where it works regardless of cause: a run that would
# newly retire more than max(5, 10%) of what was previously published refuses
# to write at all. The floor of 5 leaves small or young registries room for a
# genuine cleanup; the 10% share keeps the ceiling proportional for the large
# ones (claude 472 -> 47, codex 866 -> 86). Both bounds are far above any real
# upstream behaviour observed so far (npm unpublishes are rare and singular,
# GitHub releases are effectively append-only) and far below the mass event a
# truncated listing produces. A genuine upstream mass-unpublish is a deliberate
# act, so unblocking it is deliberate too — and per product, because the
# deliberation was: OVM_ALLOW_MASS_RETIREMENT=claude (see
# mass_retirement_override).
MASS_RETIREMENT_FLOOR = 5
MASS_RETIREMENT_SHARE = 0.10

# The override names the product(s) it covers, and a blanket value is not one
# of them. OVM_ALLOW_MASS_RETIREMENT=1 switched the breaker off for EVERY
# product in the run, so an operator who had checked a genuine claude cleanup by
# hand also waved through a codex listing that one empty page had truncated in
# the same invocation — the precise event the breaker exists to catch, permitted
# by a decision that was never about codex. An operator can vouch for a listing
# they inspected; nobody can vouch for 'whatever else this run happens to find'.
MASS_RETIREMENT_BLANKET = ('1', 'true', 'yes', 'on', 'all', '*')

def mass_retirement_override(product):
    # (permitted, complaint) for one product, read from the operator's value.
    raw = os.environ.get('OVM_ALLOW_MASS_RETIREMENT', '')
    named = {token.strip().lower() for token in raw.split(',') if token.strip()}
    if not named:
        return False, ''
    blanket = ', '.join(sorted(named.intersection(MASS_RETIREMENT_BLANKET)))
    if blanket:
        return False, (
            f'OVM_ALLOW_MASS_RETIREMENT={raw} permits nothing: {blanket} would '
            f'cover every product in this run, including one truncated by a '
            f'transient upstream failure nobody looked at. Name the product(s) '
            f'it covers instead, comma-separated: '
            f'OVM_ALLOW_MASS_RETIREMENT={product}'
        )
    if product.lower() in named:
        return True, ''
    listed = ', '.join(sorted(named))
    return False, (
        f'OVM_ALLOW_MASS_RETIREMENT names {listed}, not {product}. If this '
        f'cleanup was reviewed too, say so explicitly: '
        f'OVM_ALLOW_MASS_RETIREMENT={raw},{product}'
    )

def guard_mass_retirement(product, previously_published, newly_retired):
    if not newly_retired:
        return
    threshold = max(
        MASS_RETIREMENT_FLOOR, int(len(previously_published) * MASS_RETIREMENT_SHARE)
    )
    if len(newly_retired) <= threshold:
        return
    permitted, complaint = mass_retirement_override(product)
    if permitted:
        print(
            f'warning: retiring {len(newly_retired)} {product} versions in one run '
            f'(threshold {threshold}) — allowed by OVM_ALLOW_MASS_RETIREMENT '
            f'naming {product}',
            file=sys.stderr,
        )
        return
    sample = ', '.join(sorted(newly_retired)[:5])
    refusal = (
        f'{product}: this refresh would newly retire {len(newly_retired)} of '
        f'{len(previously_published)} published versions in one run, over the '
        f'threshold of {threshold} (e.g. {sample}). A truncated or error-shaped '
        f'upstream listing looks exactly like this, so the registry is left '
        f'untouched. If upstream really did unpublish them, rerun with '
        f'OVM_ALLOW_MASS_RETIREMENT={product} — the override has to name the '
        f'product it permits, so allowing this one cannot also wave through '
        f'another product truncated in the same run.'
    )
    if complaint:
        refusal = f'{refusal} {complaint}'
    raise SystemExit(refusal)

def parse_semver(v):
    # Splitting the whole string on '.' only works for bare X.Y.Z: a prerelease
    # like 2.1.220-beta.1 has FOUR parts, fell into the (0, 0, 0) bucket, and so
    # sorted ahead of 0.0.1 and compared equal to every other prerelease. Parse
    # the semver shape instead, matching what the Codex updater's semver_key
    # already does: build metadata is ignored, a stable release sorts AFTER its
    # own prereleases, and numeric prerelease identifiers compare numerically
    # (beta.9 < beta.10) while alphanumeric ones sort after numeric ones.
    # Anything that is not X.Y.Z at all keeps the old total-function behaviour:
    # a constant key, so sorted() leaves those entries in listing order rather
    # than raising mid-refresh.
    core, _, prerelease = v.split('+', 1)[0].partition('-')
    parts = core.split('.')
    if len(parts) != 3:
        return (0, 0, 0, 0, ())
    try:
        major, minor, patch = (int(p) for p in parts)
    except ValueError:
        return (0, 0, 0, 0, ())
    if not prerelease:
        return (major, minor, patch, 1, ())
    prerelease_key = tuple(
        (0, int(part)) if part.isdigit() else (1, part)
        for part in prerelease.split('.')
    )
    return (major, minor, patch, 0, prerelease_key)

def require_version_list(payload, source):
    # npm does not only answer with a list of versions. It exits 0 and prints a
    # JSON OBJECT on some failures ({'error': {...}}), and it prints a BARE
    # STRING for a package with a single version. Both transform without ever
    # raising: sorted() over a dict yields its keys, sorted() over a string
    # yields its characters, and the empty-list guard downstream sees a
    # non-empty list of garbage and lets it through — at which point every real
    # release is absent from 'current' and gets retired in one write. So the
    # shape is checked BEFORE the transform, not the emptiness of the result
    # after it.
    if not isinstance(payload, list):
        raise SystemExit(
            f'{source}: expected a JSON list of versions, got '
            f'{type(payload).__name__} ({payload!r:.200}); refusing to overwrite registry'
        )
    if not payload:
        raise SystemExit(f'{source}: empty version list; refusing to overwrite registry')
    for position, item in enumerate(payload):
        if not isinstance(item, str) or not item.strip():
            raise SystemExit(
                f'{source}: entry {position} is not a version string ({item!r}); '
                f'refusing to overwrite registry'
            )
        if item != item.strip():
            # Machine-written input; padding means something mangled it, and a
            # padded version matches nothing downstream.
            raise SystemExit(
                f'{source}: entry {position} has surrounding whitespace ({item!r}); '
                f'refusing to overwrite registry'
            )
        if not NPM_SEMVER_RE.match(item):
            # Shape was never enough: a list of non-empty unpadded strings is
            # exactly what an error body degrades into, and every one of those
            # strings would then be absent from 'current'.
            raise SystemExit(
                f'{source}: entry {position} is not a semver version ({item!r}); '
                f'npm publishes only semver, so this is an error body in disguise; '
                f'refusing to overwrite registry'
            )
    return payload

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
    newly_retired = []
    for version, entry in previous_versions.items():
        if version in current or version in retired:
            continue
        newly_retired.append(version)
        retired[version] = {
            'version': version,
            'date': entry.get('date', ''),
            'last_seen_at': previous.get('updated_at', ''),
            'retired_at': '$TIMESTAMP',
        }
    guard_mass_retirement('claude', previous_versions, newly_retired)
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
version_list = require_version_list(json.loads(r1.stdout), 'npm versions lookup')

# Fetch timestamps
r2 = subprocess.run(
    ['npm', 'view', '@anthropic-ai/claude-code', 'time', '--json'],
    capture_output=True, text=True, timeout=30
)
if r2.returncode != 0:
    raise SystemExit(f'npm time lookup failed: {r2.stderr}')
time_data = json.loads(r2.stdout)
if not isinstance(time_data, dict):
    raise SystemExit(
        f'npm time lookup: expected a JSON object, got {type(time_data).__name__}; '
        f'refusing to overwrite registry'
    )

# Fetch dist-tags
r3 = subprocess.run(
    ['npm', 'view', '@anthropic-ai/claude-code', 'dist-tags', '--json'],
    capture_output=True, text=True, timeout=30
)
if r3.returncode != 0:
    raise SystemExit(f'npm dist-tags lookup failed: {r3.stderr}')
dist_tags = json.loads(r3.stdout)
if not isinstance(dist_tags, dict) or not all(
    isinstance(tag, str) and isinstance(target, str) for tag, target in dist_tags.items()
):
    raise SystemExit(
        f'npm dist-tags lookup: expected a JSON object of tag -> version, got '
        f'{dist_tags!r:.200}; refusing to overwrite registry'
    )

# Merge: version list is canonical, dates from time_data
versions = []
for v in sorted(version_list, key=parse_semver):
    entry = {'version': v}
    published_at = time_data.get(v)
    # A non-string time value is treated as ABSENT rather than refusing the
    # run, and the asymmetry is the point. 'date' is already optional in a
    # registry entry — npm's time map does not cover every version — so a
    # missing date is a legal, harmless state every consumer already handles.
    # Slicing a non-string, by contrast, writes what it slices: {'2.1.220':
    # ['...']} yields an ARRAY-valued 'date', which the Rust registry reader
    # (crates/ovm/src/sources/registry.rs, where dates are Strings) rejects on
    # deserialize — one malformed value from npm would brick the entire Claude
    # registry for every client. Dropping one date costs a display field;
    # refusing the run would trade a client-visible brick for a publish outage
    # over a value the file does not need.
    #
    # A string that does not OPEN with a real calendar date is treated the same
    # way, and for the same reason: slicing it writes what it slices, so
    # 'temporarily unavailable' became the date 'temporaril', and a shape check
    # alone would still publish 2026-99-99. Everything else is date-absent.
    if isinstance(published_at, str):
        parsed = leading_calendar_date(published_at)
        if parsed is not None:
            entry['date'] = parsed
    versions.append(entry)

# The other products refuse an empty upstream listing; Claude must too, or a
# transient empty npm answer retires every published release in one write.
if not versions:
    raise SystemExit('No Claude versions found; refusing to overwrite registry')

# A dist tag may only advertise a version this file publishes. npm hands the
# tags over verbatim, so an unpublished target would leave a client resolving
# the tag to a version with no registry entry — the same dangling tag the gate
# refuses to write on its way out. '' is how both layers spell 'no version'.
published = {entry['version'] for entry in versions}
# npm always answers with a 'latest' tag pointing at a published version, so
# its absence is not 'this package has no latest' — it is the answer not being
# a dist-tags document at all. {'message': 'temporarily unavailable'} is a
# str->str object, passes the type check above, and then filters down to
# {'message': ''}: a registry published with no latest pointer, which every
# client resolving 'claude latest' reads as nothing to install.
if dist_tags.get('latest') not in published:
    raise SystemExit(
        f'npm dist-tags lookup: no \"latest\" tag naming a published version '
        f'(got {dist_tags!r:.200}); refusing to overwrite registry'
    )
dist_tags = {
    tag: (target if target in published else '')
    for tag, target in dist_tags.items()
}

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
import json, os, re, subprocess, sys, time, urllib.error, urllib.parse, urllib.request

SEMVER_RE = re.compile(r'^rust-v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$')
CODEX_ASSET_RE = re.compile(r'^codex-(?:aarch64|x86_64)-(?:apple-darwin(?:-unsigned)?|unknown-linux-musl)\.tar\.gz$')
ANONYMOUS_EXACT_TAG_LIMIT = 40

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

# Mass-retirement circuit breaker. Same rule and same numbers as update_claude
# (see the rationale there); it matters even more here, because the GitHub
# updaters walk paginated listings and treat ANY empty page as the end of the
# list — so one transient empty page truncates the walk and everything past it
# reads as unpublished.
MASS_RETIREMENT_FLOOR = 5
MASS_RETIREMENT_SHARE = 0.10

# The override names the product(s) it covers, and a blanket value is not one
# of them. OVM_ALLOW_MASS_RETIREMENT=1 switched the breaker off for EVERY
# product in the run, so an operator who had checked a genuine claude cleanup by
# hand also waved through a codex listing that one empty page had truncated in
# the same invocation — the precise event the breaker exists to catch, permitted
# by a decision that was never about codex. An operator can vouch for a listing
# they inspected; nobody can vouch for 'whatever else this run happens to find'.
MASS_RETIREMENT_BLANKET = ('1', 'true', 'yes', 'on', 'all', '*')

def mass_retirement_override(product):
    # (permitted, complaint) for one product, read from the operator's value.
    raw = os.environ.get('OVM_ALLOW_MASS_RETIREMENT', '')
    named = {token.strip().lower() for token in raw.split(',') if token.strip()}
    if not named:
        return False, ''
    blanket = ', '.join(sorted(named.intersection(MASS_RETIREMENT_BLANKET)))
    if blanket:
        return False, (
            f'OVM_ALLOW_MASS_RETIREMENT={raw} permits nothing: {blanket} would '
            f'cover every product in this run, including one truncated by a '
            f'transient upstream failure nobody looked at. Name the product(s) '
            f'it covers instead, comma-separated: '
            f'OVM_ALLOW_MASS_RETIREMENT={product}'
        )
    if product.lower() in named:
        return True, ''
    listed = ', '.join(sorted(named))
    return False, (
        f'OVM_ALLOW_MASS_RETIREMENT names {listed}, not {product}. If this '
        f'cleanup was reviewed too, say so explicitly: '
        f'OVM_ALLOW_MASS_RETIREMENT={raw},{product}'
    )

def guard_mass_retirement(product, previously_published, newly_retired):
    if not newly_retired:
        return
    threshold = max(
        MASS_RETIREMENT_FLOOR, int(len(previously_published) * MASS_RETIREMENT_SHARE)
    )
    if len(newly_retired) <= threshold:
        return
    permitted, complaint = mass_retirement_override(product)
    if permitted:
        print(
            f'warning: retiring {len(newly_retired)} {product} versions in one run '
            f'(threshold {threshold}) — allowed by OVM_ALLOW_MASS_RETIREMENT '
            f'naming {product}',
            file=sys.stderr,
        )
        return
    sample = ', '.join(sorted(newly_retired)[:5])
    refusal = (
        f'{product}: this refresh would newly retire {len(newly_retired)} of '
        f'{len(previously_published)} published versions in one run, over the '
        f'threshold of {threshold} (e.g. {sample}). A truncated or error-shaped '
        f'upstream listing looks exactly like this, so the registry is left '
        f'untouched. If upstream really did unpublish them, rerun with '
        f'OVM_ALLOW_MASS_RETIREMENT={product} — the override has to name the '
        f'product it permits, so allowing this one cannot also wave through '
        f'another product truncated in the same run.'
    )
    if complaint:
        refusal = f'{refusal} {complaint}'
    raise SystemExit(refusal)

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
    newly_retired = []
    for version, entry in previous_versions.items():
        if version in current or version in retired:
            continue
        newly_retired.append(version)
        retired[version] = {
            'version': version,
            'date': entry.get('date', ''),
            'last_seen_at': previous.get('updated_at', ''),
            'retired_at': '$TIMESTAMP',
        }
    guard_mass_retirement('codex', previous_versions, newly_retired)
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

def is_github_listing_cap(error):
    if error.code != 422:
        return False
    try:
        payload = json.loads(error.read())
    except (json.JSONDecodeError, UnicodeDecodeError):
        return False
    return isinstance(payload, dict) and str(payload.get('message', '')).startswith(
        'Only the first 1000 results are available'
    )

def exact_installable_release(tag):
    encoded = urllib.parse.quote(tag, safe='')
    url = f'https://api.github.com/repos/openai/codex/releases/tags/{encoded}'
    req = urllib.request.Request(url, headers=github_headers())
    for attempt in range(4):
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                release = json.loads(resp.read())
            break
        except urllib.error.HTTPError as error:
            if error.code == 404:
                return False
            if attempt == 3:
                print(f'GitHub exact-tag error after {attempt + 1} attempts: {error}', file=sys.stderr)
                raise SystemExit(1)
            print(f'GitHub exact-tag error (attempt {attempt + 1}, retrying): {error}', file=sys.stderr)
            time.sleep(5 * 2 ** attempt)
        except Exception as error:
            if attempt == 3:
                print(f'GitHub exact-tag error after {attempt + 1} attempts: {error}', file=sys.stderr)
                raise SystemExit(1)
            print(f'GitHub exact-tag error (attempt {attempt + 1}, retrying): {error}', file=sys.stderr)
            time.sleep(5 * 2 ** attempt)
    if not isinstance(release, dict) or release.get('tag_name') != tag:
        raise SystemExit(
            f'GitHub exact-tag response for {tag} was not that release '
            f'({release!r:.200}); refusing to overwrite registry'
        )
    return is_installable_codex_release(release)

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
listing_capped = False
oldest_release_date = None
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
        except urllib.error.HTTPError as e:
            if is_github_listing_cap(e):
                listing_capped = True
                break
            if attempt == 3:
                print(f'GitHub API error after {attempt + 1} attempts: {e}', file=sys.stderr)
                raise SystemExit(1)
            print(f'GitHub API error (attempt {attempt + 1}, retrying): {e}', file=sys.stderr)
            time.sleep(5 * 2 ** attempt)
        except Exception as e:
            if attempt == 3:
                print(f'GitHub API error after {attempt + 1} attempts: {e}', file=sys.stderr)
                raise SystemExit(1)
            print(f'GitHub API error (attempt {attempt + 1}, retrying): {e}', file=sys.stderr)
            time.sleep(5 * 2 ** attempt)

    if listing_capped:
        break

    # A GitHub error body ({'message': 'API rate limit exceeded'}) parses fine
    # and is not empty, and iterating it yields strings whose .get() blows up
    # with an AttributeError nobody can read. Name the shape instead.
    if not isinstance(releases, list):
        raise SystemExit(
            f'GitHub releases response was {type(releases).__name__}, not a list '
            f'({releases!r:.200}); refusing to overwrite registry'
        )

    if not releases:
        break

    for r in releases:
        tag = r.get('tag_name', '')
        date = (r.get('published_at') or '')[:10]
        if re.fullmatch(r'\d{4}-\d{2}-\d{2}', date):
            oldest_release_date = min(oldest_release_date or date, date)
        if is_installable_codex_release(r):
            versions.append({'version': tag, 'date': date})

    page += 1

if not versions:
    raise SystemExit('No installable Codex versions found; refusing to overwrite registry')

path = '$API_DIR/codex.json'
previous = load_previous(path)
if listing_capped:
    if not previous:
        raise SystemExit(
            'GitHub releases listing capped at 1000 results, but no previous Codex '
            'registry exists to supply the older history; refusing to write an '
            'incomplete registry'
        )
    fetched = {entry['version'] for entry in versions}
    oldest_installable = min(
        (semver_key(entry['version']) for entry in versions),
        default=None,
    )

    def definitely_before_cap(entry):
        version = entry.get('version')
        date = entry.get('date')
        return (
            isinstance(version, str)
            and SEMVER_RE.match(version)
            and isinstance(date, str)
            and oldest_release_date is not None
            and oldest_installable is not None
            and date <= oldest_release_date
            and semver_key(version) <= oldest_installable
        )

    active_missing = [
        entry for entry in previous.get('versions', [])
        if entry.get('version') not in fetched
    ]
    retired_missing = [
        entry for entry in previous.get('retired_versions', [])
        if entry.get('version') not in fetched
    ]
    exact_candidates = [
        entry for entry in active_missing if not definitely_before_cap(entry)
    ] + retired_missing
    if not os.environ.get('GITHUB_TOKEN') and len(exact_candidates) > ANONYMOUS_EXACT_TAG_LIMIT:
        raise SystemExit(
            f'GitHub releases listing is capped and {len(exact_candidates)} exact-tag '
            f'checks are needed, over the anonymous safety limit of '
            f'{ANONYMOUS_EXACT_TAG_LIMIT}. Set GITHUB_TOKEN so the refresh can '
            f'reconcile capped history without exhausting the 60-request quota.'
        )

    preserved = 0
    for entry in active_missing:
        version = entry.get('version')
        if not definitely_before_cap(entry) and not exact_installable_release(version):
            continue
        versions.append(entry.copy())
        fetched.add(version)
        preserved += 1
    restored = 0
    for entry in retired_missing:
        version = entry.get('version')
        if not exact_installable_release(version):
            continue
        active_entry = {
            key: value for key, value in entry.items()
            if key not in ('last_seen_at', 'retired_at')
        }
        versions.append(active_entry)
        fetched.add(version)
        restored += 1
    print(
        f'warning: GitHub releases listing capped at 1000 results; preserved '
        f'{preserved} older Codex versions and restored {restored} from retired history',
        file=sys.stderr,
    )

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

# Mass-retirement circuit breaker. Same rule and same numbers as update_claude
# (see the rationale there); it matters even more here, because the GitHub
# updaters walk paginated listings and treat ANY empty page as the end of the
# list — so one transient empty page truncates the walk and everything past it
# reads as unpublished.
MASS_RETIREMENT_FLOOR = 5
MASS_RETIREMENT_SHARE = 0.10

# The override names the product(s) it covers, and a blanket value is not one
# of them. OVM_ALLOW_MASS_RETIREMENT=1 switched the breaker off for EVERY
# product in the run, so an operator who had checked a genuine claude cleanup by
# hand also waved through a codex listing that one empty page had truncated in
# the same invocation — the precise event the breaker exists to catch, permitted
# by a decision that was never about codex. An operator can vouch for a listing
# they inspected; nobody can vouch for 'whatever else this run happens to find'.
MASS_RETIREMENT_BLANKET = ('1', 'true', 'yes', 'on', 'all', '*')

def mass_retirement_override(product):
    # (permitted, complaint) for one product, read from the operator's value.
    raw = os.environ.get('OVM_ALLOW_MASS_RETIREMENT', '')
    named = {token.strip().lower() for token in raw.split(',') if token.strip()}
    if not named:
        return False, ''
    blanket = ', '.join(sorted(named.intersection(MASS_RETIREMENT_BLANKET)))
    if blanket:
        return False, (
            f'OVM_ALLOW_MASS_RETIREMENT={raw} permits nothing: {blanket} would '
            f'cover every product in this run, including one truncated by a '
            f'transient upstream failure nobody looked at. Name the product(s) '
            f'it covers instead, comma-separated: '
            f'OVM_ALLOW_MASS_RETIREMENT={product}'
        )
    if product.lower() in named:
        return True, ''
    listed = ', '.join(sorted(named))
    return False, (
        f'OVM_ALLOW_MASS_RETIREMENT names {listed}, not {product}. If this '
        f'cleanup was reviewed too, say so explicitly: '
        f'OVM_ALLOW_MASS_RETIREMENT={raw},{product}'
    )

def guard_mass_retirement(product, previously_published, newly_retired):
    if not newly_retired:
        return
    threshold = max(
        MASS_RETIREMENT_FLOOR, int(len(previously_published) * MASS_RETIREMENT_SHARE)
    )
    if len(newly_retired) <= threshold:
        return
    permitted, complaint = mass_retirement_override(product)
    if permitted:
        print(
            f'warning: retiring {len(newly_retired)} {product} versions in one run '
            f'(threshold {threshold}) — allowed by OVM_ALLOW_MASS_RETIREMENT '
            f'naming {product}',
            file=sys.stderr,
        )
        return
    sample = ', '.join(sorted(newly_retired)[:5])
    refusal = (
        f'{product}: this refresh would newly retire {len(newly_retired)} of '
        f'{len(previously_published)} published versions in one run, over the '
        f'threshold of {threshold} (e.g. {sample}). A truncated or error-shaped '
        f'upstream listing looks exactly like this, so the registry is left '
        f'untouched. If upstream really did unpublish them, rerun with '
        f'OVM_ALLOW_MASS_RETIREMENT={product} — the override has to name the '
        f'product it permits, so allowing this one cannot also wave through '
        f'another product truncated in the same run.'
    )
    if complaint:
        refusal = f'{refusal} {complaint}'
    raise SystemExit(refusal)

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
    newly_retired = []
    for version, entry in previous_versions.items():
        if version in current or version in retired:
            continue
        newly_retired.append(version)
        retired[version] = {
            'version': version,
            'date': entry.get('date', ''),
            'last_seen_at': previous.get('updated_at', ''),
            'retired_at': '$TIMESTAMP',
        }
    guard_mass_retirement('pi', previous_versions, newly_retired)
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

    # A GitHub error body ({'message': 'API rate limit exceeded'}) parses fine
    # and is not empty, and iterating it yields strings whose .get() blows up
    # with an AttributeError nobody can read. Name the shape instead.
    if not isinstance(releases, list):
        raise SystemExit(
            f'GitHub releases response was {type(releases).__name__}, not a list '
            f'({releases!r:.200}); refusing to overwrite registry'
        )

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

# Mass-retirement circuit breaker. Same rule and same numbers as update_claude
# (see the rationale there); it matters even more here, because the GitHub
# updaters walk paginated listings and treat ANY empty page as the end of the
# list — so one transient empty page truncates the walk and everything past it
# reads as unpublished.
MASS_RETIREMENT_FLOOR = 5
MASS_RETIREMENT_SHARE = 0.10

# The override names the product(s) it covers, and a blanket value is not one
# of them. OVM_ALLOW_MASS_RETIREMENT=1 switched the breaker off for EVERY
# product in the run, so an operator who had checked a genuine claude cleanup by
# hand also waved through a codex listing that one empty page had truncated in
# the same invocation — the precise event the breaker exists to catch, permitted
# by a decision that was never about codex. An operator can vouch for a listing
# they inspected; nobody can vouch for 'whatever else this run happens to find'.
MASS_RETIREMENT_BLANKET = ('1', 'true', 'yes', 'on', 'all', '*')

def mass_retirement_override(product):
    # (permitted, complaint) for one product, read from the operator's value.
    raw = os.environ.get('OVM_ALLOW_MASS_RETIREMENT', '')
    named = {token.strip().lower() for token in raw.split(',') if token.strip()}
    if not named:
        return False, ''
    blanket = ', '.join(sorted(named.intersection(MASS_RETIREMENT_BLANKET)))
    if blanket:
        return False, (
            f'OVM_ALLOW_MASS_RETIREMENT={raw} permits nothing: {blanket} would '
            f'cover every product in this run, including one truncated by a '
            f'transient upstream failure nobody looked at. Name the product(s) '
            f'it covers instead, comma-separated: '
            f'OVM_ALLOW_MASS_RETIREMENT={product}'
        )
    if product.lower() in named:
        return True, ''
    listed = ', '.join(sorted(named))
    return False, (
        f'OVM_ALLOW_MASS_RETIREMENT names {listed}, not {product}. If this '
        f'cleanup was reviewed too, say so explicitly: '
        f'OVM_ALLOW_MASS_RETIREMENT={raw},{product}'
    )

def guard_mass_retirement(product, previously_published, newly_retired):
    if not newly_retired:
        return
    threshold = max(
        MASS_RETIREMENT_FLOOR, int(len(previously_published) * MASS_RETIREMENT_SHARE)
    )
    if len(newly_retired) <= threshold:
        return
    permitted, complaint = mass_retirement_override(product)
    if permitted:
        print(
            f'warning: retiring {len(newly_retired)} {product} versions in one run '
            f'(threshold {threshold}) — allowed by OVM_ALLOW_MASS_RETIREMENT '
            f'naming {product}',
            file=sys.stderr,
        )
        return
    sample = ', '.join(sorted(newly_retired)[:5])
    refusal = (
        f'{product}: this refresh would newly retire {len(newly_retired)} of '
        f'{len(previously_published)} published versions in one run, over the '
        f'threshold of {threshold} (e.g. {sample}). A truncated or error-shaped '
        f'upstream listing looks exactly like this, so the registry is left '
        f'untouched. If upstream really did unpublish them, rerun with '
        f'OVM_ALLOW_MASS_RETIREMENT={product} — the override has to name the '
        f'product it permits, so allowing this one cannot also wave through '
        f'another product truncated in the same run.'
    )
    if complaint:
        refusal = f'{refusal} {complaint}'
    raise SystemExit(refusal)

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
    newly_retired = []
    for version, entry in previous_versions.items():
        if version in current or version in retired:
            continue
        newly_retired.append(version)
        retired[version] = {
            'version': version,
            'date': entry.get('date', ''),
            'last_seen_at': previous.get('updated_at', ''),
            'retired_at': '$TIMESTAMP',
        }
    guard_mass_retirement('cliproxyapi', previous_versions, newly_retired)
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

    # A GitHub error body ({'message': 'API rate limit exceeded'}) parses fine
    # and is not empty, and iterating it yields strings whose .get() blows up
    # with an AttributeError nobody can read. Name the shape instead.
    if not isinstance(releases, list):
        raise SystemExit(
            f'GitHub releases response was {type(releases).__name__}, not a list '
            f'({releases!r:.200}); refusing to overwrite registry'
        )

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
