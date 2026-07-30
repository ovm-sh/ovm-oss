#!/usr/bin/env zsh
# Version Canary Test Runner
#
# Tests a Claude Code version for feature compatibility.
# Outputs structured JSON to stdout.
#
# Usage:
#   ./scripts/version-canary-test.sh <version>
#   ./scripts/version-canary-test.sh 2.1.112

set -uo pipefail

VERSION="${1:?Usage: version-canary-test.sh <version>}"
OVM_DIR="$HOME/.ovm"
PLATFORM="$(uname -s)-$(uname -m)"
TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
# Caller may point us at the ovm binary so we don't need to know its install
# layout. The canary workflow sets this to its just-built target/release/ovm.
OVM_BIN="${OVM_BIN:-ovm}"

# ── Helpers ──────────────────────────────────────────────────────────────────

# Resolve the path to the `claude` binary for $VERSION. Strategy:
#   1. Ask `ovm which claude` — it owns the layout, so we can't drift from it.
#   2. Fall back to the documented filesystem layouts, oldest first, so a
#      pre-`products/` install still gets a hit.
# Anything else (e.g. fuzzy `find`) risks matching the wrong file.
get_claude_path() {
    local v="$1"

    if command -v "$OVM_BIN" >/dev/null 2>&1; then
        local resolved
        resolved=$("$OVM_BIN" which claude 2>/dev/null | awk 'NR==1{print $1}')
        # Guard: the path must point at this version's tree, otherwise we'd
        # silently test whatever was previously active.
        if [ -n "$resolved" ] && [ -f "$resolved" ] && [[ "$resolved" == *"/$v/"* ]]; then
            echo "$resolved"
            return 0
        fi
    fi

    for c in \
        "$OVM_DIR/products/claude/versions/$v/native/claude" \
        "$OVM_DIR/products/claude/versions/$v/npm/installed/node_modules/.bin/claude" \
        "$OVM_DIR/products/claude/versions/$v/installed/node_modules/.bin/claude" \
        "$OVM_DIR/versions/$v/native/claude" \
        "$OVM_DIR/versions/$v/npm/installed/node_modules/.bin/claude" \
        "$OVM_DIR/versions/$v/installed/node_modules/.bin/claude"; do
        if [ -f "$c" ]; then
            echo "$c"
            return 0
        fi
    done
    return 1
}

# Dump OVM state to stderr when the binary lookup fails so CI logs show why,
# instead of just "no binary found".
dump_ovm_layout() {
    local v="$1"
    echo "  Lookup diagnostics (OVM_BIN=$OVM_BIN, OVM_DIR=$OVM_DIR, version=$v):" >&2
    if command -v "$OVM_BIN" >/dev/null 2>&1; then
        echo "    \$ $OVM_BIN which claude" >&2
        "$OVM_BIN" which claude 2>&1 | sed 's/^/      /' >&2 || true
        echo "    \$ $OVM_BIN current claude" >&2
        "$OVM_BIN" current claude 2>&1 | sed 's/^/      /' >&2 || true
    else
        echo "    (ovm binary not on PATH; set OVM_BIN to point at it)" >&2
    fi
    if [ -d "$OVM_DIR" ]; then
        echo "    OVM_DIR tree (depth 5):" >&2
        find "$OVM_DIR" -maxdepth 5 -print 2>/dev/null | sed 's/^/      /' >&2 || true
    else
        echo "    (OVM_DIR does not exist)" >&2
    fi
}

json_test() {
    local name="$1" result="$2" duration="$3" detail="${4:-}"
    local detail_json="null"
    if [ -n "$detail" ]; then
        detail_json="\"$(echo "$detail" | sed 's/"/\\"/g' | tr '\n' ' ' | head -c 200)\""
    fi
    echo "    {\"name\": \"$name\", \"status\": \"$result\", \"duration_ms\": $duration, \"detail\": $detail_json}"
}

# ── Tests ────────────────────────────────────────────────────────────────────

TESTS=()
PASS_COUNT=0
FAIL_COUNT=0

run_test() {
    local name="$1"
    local start_ms=$(($(date +%s) * 1000))

    # Call test function. Stderr is preserved so diagnostics (e.g.
    # dump_ovm_layout) reach the CI log; individual tests redirect noisy
    # subprocess output themselves.
    local result detail
    result="error"
    detail=""
    "test_$name"
    # test functions set TEST_RESULT and TEST_DETAIL

    local end_ms=$(($(date +%s) * 1000))
    local duration=$(( end_ms - start_ms ))

    TESTS+=("$(json_test "$name" "$TEST_RESULT" "$duration" "$TEST_DETAIL")")

    if [ "$TEST_RESULT" = "pass" ]; then
        PASS_COUNT=$((PASS_COUNT + 1))
        echo "  ✓ $name" >&2
    elif [ "$TEST_RESULT" = "skip" ]; then
        echo "  - $name (skipped: $TEST_DETAIL)" >&2
    else
        FAIL_COUNT=$((FAIL_COUNT + 1))
        echo "  ✗ $name ($TEST_DETAIL)" >&2
    fi
}

# ── Test: binary_exists ──────────────────────────────────────────────────────

test_binary_exists() {
    local claude_path
    if claude_path=$(get_claude_path "$VERSION"); then
        TEST_RESULT="pass"
        TEST_DETAIL="$claude_path"
    else
        TEST_RESULT="fail"
        TEST_DETAIL="no binary found for $VERSION"
        dump_ovm_layout "$VERSION"
    fi
}

# ── Test: version_output ─────────────────────────────────────────────────────

test_version_output() {
    local claude_path
    claude_path=$(get_claude_path "$VERSION") || {
        TEST_RESULT="skip"
        TEST_DETAIL="binary not found"
        return
    }

    local output
    output=$("$claude_path" --version 2>&1) || true

    if echo "$output" | grep -qE "[0-9]+\.[0-9]+\.[0-9]+"; then
        local detected
        detected=$(echo "$output" | grep -oE "[0-9]+\.[0-9]+\.[0-9]+" | head -1)
        TEST_RESULT="pass"
        TEST_DETAIL="$detected"
    else
        TEST_RESULT="fail"
        TEST_DETAIL="no version string in output"
    fi
}

# ── Test: help_output ────────────────────────────────────────────────────────

test_help_output() {
    local claude_path
    claude_path=$(get_claude_path "$VERSION") || {
        TEST_RESULT="skip"
        TEST_DETAIL="binary not found"
        return
    }

    local output exit_code
    output=$("$claude_path" --help 2>&1) || true
    exit_code=$?

    if echo "$output" | grep -qi "usage\|options\|commands\|claude"; then
        TEST_RESULT="pass"
        TEST_DETAIL=""
    else
        TEST_RESULT="fail"
        TEST_DETAIL="help output missing expected content (exit $exit_code)"
    fi
}

# ── Test: buddy_command ──────────────────────────────────────────────────────

test_buddy_command() {
    local claude_path
    claude_path=$(get_claude_path "$VERSION") || {
        TEST_RESULT="skip"
        TEST_DETAIL="binary not found"
        return
    }

    if ! command -v expect &>/dev/null; then
        TEST_RESULT="skip"
        TEST_DETAIL="expect not available"
        return
    fi

    local response_file
    response_file=$(mktemp /tmp/canary-buddy-XXXXXX)

    expect -c "
        set timeout 45
        log_user 0
        spawn $claude_path

        set ready 0
        expect {
            -timeout 30
            -re {.+} {
                set ready 1
                exp_continue -continue_timer
            }
            timeout {
                if {!\$ready} {
                    set fp [open \"$response_file\" w]
                    puts \$fp \"STARTUP_TIMEOUT\"
                    close \$fp
                    catch { close }
                    catch { wait }
                    exit
                }
            }
        }

        sleep 1
        send \"/buddy\r\"

        set response {}
        expect {
            -timeout 10
            -re {.+} {
                append response \$expect_out(buffer)
                exp_continue -continue_timer
            }
            timeout {}
        }

        set fp [open \"$response_file\" w]
        puts \$fp \$response
        close \$fp

        send \"/exit\r\"
        sleep 1
        catch { close }
        catch { wait }
    " 2>/dev/null

    local response
    response=$(cat "$response_file" 2>/dev/null)
    rm -f "$response_file"

    # Explicit fail signals (order matters — check fail first to avoid false positives)
    if echo "$response" | grep -qi "Unknown skill.*buddy\|unknown.*buddy\|Available commands\|not a valid"; then
        TEST_RESULT="fail"
        TEST_DETAIL="command not recognized"
    elif echo "$response" | grep -qi "Quelpaw"; then
        TEST_RESULT="pass"
        TEST_DETAIL="Quelpaw found"
    elif echo "$response" | grep -qi "STARTUP_TIMEOUT"; then
        TEST_RESULT="error"
        TEST_DETAIL="startup timeout"
    elif [ -z "$response" ]; then
        TEST_RESULT="error"
        TEST_DETAIL="no response captured"
    else
        TEST_RESULT="fail"
        TEST_DETAIL="no buddy indicators in response"
    fi
}

# ── Main ─────────────────────────────────────────────────────────────────────

echo "Testing Claude Code $VERSION..." >&2
echo "" >&2

run_test binary_exists
run_test version_output
run_test help_output
run_test buddy_command

# Overall
overall="pass"
if [ "$FAIL_COUNT" -gt 0 ]; then
    overall="fail"
fi

echo "" >&2
echo "$PASS_COUNT passed, $FAIL_COUNT failed" >&2

# Output JSON to stdout
tests_json=$(printf '%s\n' "${TESTS[@]}" | paste -sd',' -)

cat <<EOF
{
  "version": "$VERSION",
  "timestamp": "$TIMESTAMP",
  "platform": "$PLATFORM",
  "tests": [
$tests_json
  ],
  "overall": "$overall"
}
EOF
