#!/bin/bash
# tools/test-beta-update-markers.sh
# Execute the real beta/redeploy scripts with fake services and an owned fixture.
set -eu
SOURCE="$(cd "$(dirname "$0")/.." && pwd)"
TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/audiolib-beta-markers.XXXXXX")"
case "$TEST_ROOT" in "${TMPDIR:-/tmp}"/audiolib-beta-markers.*) ;; *) exit 2 ;; esac
cleanup() { rm -rf -- "$TEST_ROOT"; }
trap cleanup EXIT
mkdir -p "$TEST_ROOT/repo/tools" "$TEST_ROOT/repo/server/deploy" "$TEST_ROOT/mock-bin"
cp "$SOURCE/tools/beta-refresh.sh" "$TEST_ROOT/repo/tools/"
cp "$SOURCE/server/deploy/redeploy-server.sh" "$TEST_ROOT/repo/server/deploy/"
# Existing Python is used only to emulate macOS plutil and exercise the optional
# Linux reader here; no parser is installed or added to application packages.
TEST_PYTHON="${TEST_PYTHON:-$(command -v python || command -v python3)}"
export TEST_PYTHON TEST_ROOT
cat > "$TEST_ROOT/mock-bin/plutil" <<'EOF'
#!/bin/sh
printf '%s\n' plutil >> "$BETA_APP_DIR/parser-events"
[ "${MOCK_JSON_READER:-plutil}" != python ] || exit 1
exec "$TEST_PYTHON" "$TEST_ROOT/plutil.py" "$@"
EOF
cat > "$TEST_ROOT/plutil.py" <<'EOF'
import json
import sys

args = sys.argv[1:]
assert len(args) == 9 and args[0] == '-extract' and args[2:4] == ['raw', '-expect']
assert args[5:] == ['-o', '-', '--', '-']
value = json.load(sys.stdin)
for key in args[1].split('.'):
    assert type(value) is dict
    value = value[key]
expected = {'string': str, 'integer': int, 'dictionary': dict}[args[4]]
assert type(value) is expected
print('\n'.join(sorted(value)) if expected is dict else value)
EOF
cat > "$TEST_ROOT/mock-bin/python3" <<'EOF'
#!/bin/sh
printf '%s\n' python3 >> "$BETA_APP_DIR/parser-events"
[ "${MOCK_JSON_READER:-plutil}" != unavailable ] || exit 1
exec "$TEST_PYTHON" "$@"
EOF
cat > "$TEST_ROOT/mock-bin/git" <<'EOF'
#!/bin/sh
case " $* " in
    *" rev-parse "*) printf '%s\n' f54f036 ;;
    *" tag "*) printf '%s\n' "${MOCK_TAG:-}" ;;
esac
EOF
cat > "$TEST_ROOT/mock-bin/curl" <<'EOF'
#!/bin/sh
case " $* " in
    *'/scans/current'*) printf '%s\n' '{"scan":null}'; exit 0 ;;
esac
loaded="${MOCK_LAUNCHD_LOADED:-1}"
if [ -f "$BETA_APP_DIR/launchd-loaded" ]; then loaded="$(cat "$BETA_APP_DIR/launchd-loaded")"; fi
[ "$loaded" = 1 ] || exit 22
number=0
if [ -f "$BETA_APP_DIR/start-count" ]; then number="$(cat "$BETA_APP_DIR/start-count")"; fi
if [ "$number" = 1 ] && [ -n "${MOCK_HEALTH_FILE:-}" ]; then
    if [ "${MOCK_DROP_AFTER_FIRST:-0}" = 1 ]; then
        count=0
        [ ! -f "$BETA_APP_DIR/health-count" ] || count="$(cat "$BETA_APP_DIR/health-count")"
        count=$((count + 1)); printf '%s\n' "$count" > "$BETA_APP_DIR/health-count"
        [ "$count" = 1 ] || exit 22
    fi
    [ -f "$MOCK_HEALTH_FILE" ] || exit 22
    cat "$MOCK_HEALTH_FILE"
    exit 0
fi
version="$("$BETA_APP_DIR/bin/audiolib-server" --version)"; version="${version#audiolib-server }"
commit=f54f036
if [ "${MOCK_REDEPLOY_FAIL:-0}" = 1 ] && [ "$number" = 1 ]; then commit=wrong; fi
printf '{"status":"ok","version":"%s","commit":"%s","api_version":"v1","schema_version":6}\n' "$version" "$commit"
EOF
cat > "$TEST_ROOT/mock-bin/launchctl" <<'EOF'
#!/bin/sh
printf '%s|' "$@" >> "$BETA_APP_DIR/launchctl-events"
printf '\n' >> "$BETA_APP_DIR/launchctl-events"
domain="gui/$(id -u)"
loaded="${MOCK_LAUNCHD_LOADED:-1}"
if [ -f "$BETA_APP_DIR/launchd-loaded" ]; then loaded="$(cat "$BETA_APP_DIR/launchd-loaded")"; fi
case "${1:-}" in
    print)
        [ "$#" = 2 ] && [ "$2" = "$domain/synthetic.beta" ] || exit 7
        [ "$loaded" = 1 ]; exit $? ;;
    bootout)
        [ "$#" = 2 ] && [ "$2" = "$domain/synthetic.beta" ] && [ "$loaded" = 1 ] || exit 7
        printf '%s\n' 0 > "$BETA_APP_DIR/launchd-loaded"; exit 0 ;;
    bootstrap)
        [ "$#" = 3 ] && [ "$2" = "$domain" ] && [ -f "$3" ] || exit 7
        # Like launchd, refuse to bootstrap a job that is still loaded.
        [ "$loaded" = 0 ] || exit 5
        [ "$3" = "$LAUNCHD_PLIST" ] || exit 7 ;;
    *) exit 7 ;;
esac
number=0
if [ -f "$BETA_APP_DIR/start-count" ]; then number="$(cat "$BETA_APP_DIR/start-count")"; fi
number=$((number + 1)); printf '%s\n' "$number" > "$BETA_APP_DIR/start-count"
version="$("$BETA_APP_DIR/bin/audiolib-server" --version)"; version="${version#audiolib-server }"
channel='<missing>'; installed='<missing>'
if [ -f "$BETA_APP_DIR/bin/update-channel.txt" ]; then channel="$(cat "$BETA_APP_DIR/bin/update-channel.txt")"; fi
if [ -f "$BETA_APP_DIR/bin/update-version.txt" ]; then installed="$(cat "$BETA_APP_DIR/bin/update-version.txt")"; fi
printf '%s|%s|%s|%s\n' "$number" "$version" "$channel" "$installed" >> "$BETA_APP_DIR/start-events"
if [ "$installed" != '<missing>' ] && [ "${installed%%-*}" != "$version" ]; then
    echo 'UpdateClient would reject the marker at process startup' >&2; exit 8
fi
if [ "$1" = bootstrap ]; then
    case "${MOCK_BOOTSTRAP_FAIL:-never}:$number" in first:1|always:*) exit 6 ;; esac
fi
printf '%s\n' 1 > "$BETA_APP_DIR/launchd-loaded"
EOF
cat > "$TEST_ROOT/mock-bin/pgrep" <<'EOF'
#!/bin/sh
exit 1
EOF
# The fixture never listens on a real port (and must not probe a real 8790).
cat > "$TEST_ROOT/mock-bin/lsof" <<'EOF'
#!/bin/sh
exit 1
EOF
cat > "$TEST_ROOT/server-binary" <<'EOF'
#!/bin/sh
[ "$#" -eq 1 ] && [ "$1" = '--version' ] || exit 9
printf '%s\n' 'audiolib-server 0.6.2'
EOF
chmod +x "$TEST_ROOT/mock-bin/git" "$TEST_ROOT/mock-bin/curl" "$TEST_ROOT/mock-bin/launchctl" "$TEST_ROOT/mock-bin/pgrep" "$TEST_ROOT/mock-bin/lsof" "$TEST_ROOT/mock-bin/plutil" "$TEST_ROOT/mock-bin/python3" "$TEST_ROOT/server-binary"
export PATH="$TEST_ROOT/mock-bin:$PATH"
run_case() {
    name="$1"; tag="$2"; expected="$3"; fail="$4"; old_markers="$5"
    loaded="${6:-1}"
    app="$TEST_ROOT/$name/app"; data="$TEST_ROOT/$name/data"; work="$TEST_ROOT/$name/work"
    mkdir -p "$app/bin" "$data" "$work"
    plist="$app/Launch Agents/synthetic.beta.plist"
    mkdir -p "$app/Launch Agents"
    printf '%s\n' 'original beta plist fixture' > "$plist"
    cp "$plist" "$plist.expected"
    printf '%s\n' 'bind = "127.0.0.1:8791"' > "$app/config.toml"
    printf '%s\n' 'synthetic-token' > "$app/token"
    printf '%s\n' '#!/bin/sh' 'printf "%s\n" "audiolib-server 0.6.1"' > "$app/bin/audiolib-server"
    chmod +x "$app/bin/audiolib-server"
    if [ "$old_markers" = 1 ]; then
        printf '%s\n' stable > "$app/bin/update-channel.txt"
        printf '%s\n' 0.6.1 > "$app/bin/update-version.txt"
    fi
    status=0
    BETA_APP_DIR="$app" BETA_DATA_DIR="$data" SERVICE_NAME=synthetic.beta WORK_DIR="$work" \
        MOCK_TAG="$tag" MOCK_REDEPLOY_FAIL="$fail" MOCK_LAUNCHD_LOADED="$loaded" LAUNCHD_PLIST="$plist" \
        bash "$TEST_ROOT/repo/tools/beta-refresh.sh" --no-build "$TEST_ROOT/server-binary" >/dev/null 2>&1 || status=$?
    grep -Fx "1|0.6.2|beta|$expected" "$app/start-events" >/dev/null || { cat "$work"/beta-refresh-*.log >&2; echo 'Candidate startup did not see its exact new markers' >&2; exit 1; }
    if [ "$fail" = 1 ]; then
        [ "$status" -ne 0 ] || { echo 'Failed deployment was accepted' >&2; exit 1; }
        if [ "$old_markers" = 1 ]; then
            grep -Fx '2|0.6.1|stable|0.6.1' "$app/start-events" >/dev/null &&
                [ "$(cat "$app/bin/update-channel.txt")" = stable ] && [ "$(cat "$app/bin/update-version.txt")" = 0.6.1 ] || {
                echo 'Rollback process started with mismatched release markers' >&2; exit 1;
            }
        else
            grep -Fx '2|0.6.1|<missing>|<missing>' "$app/start-events" >/dev/null &&
                [ ! -e "$app/bin/update-channel.txt" ] && [ ! -e "$app/bin/update-version.txt" ] || {
                echo 'Rollback invented markers for a previously unmarked install' >&2; exit 1;
            }
        fi
    else
        [ "$status" -eq 0 ] && [ "$(cat "$app/bin/update-channel.txt")" = beta ] &&
            [ "$(cat "$app/bin/update-version.txt")" = "$expected" ] || {
            cat "$work"/beta-refresh-*.log >&2; echo 'Beta metadata deployment failed' >&2; exit 1;
        }
    fi
    if [ "$loaded" = 0 ]; then
        grep -Fx "bootstrap|gui/$(id -u)|$plist|" "$app/launchctl-events" >/dev/null && cmp -s "$plist" "$plist.expected" || {
            echo 'Beta refresh did not reuse the quiesced service plist' >&2; exit 1;
        }
    fi
}
run_case development '' 0.6.2-0 0 1
run_case candidate v0.6.2-beta.12 0.6.2-beta.12 0 1
run_case stable v0.6.2 0.6.2 0 1
run_case wrong-version v0.6.3-beta.1 0.6.2-0 0 1
run_case rollback v0.6.2-beta.12 0.6.2-beta.12 1 1
run_case unmarked-rollback v0.6.2-beta.12 0.6.2-beta.12 1 0
run_case quiesced-beta v0.6.2-beta.12 0.6.2-beta.12 0 1 0
# POSIX hosts must let an unprivileged service account read public markers
# written by a root-owned deployment. Git Bash emulates rather than enforces it.
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) ;; *)
    for marker in update-channel.txt update-version.txt; do
        mode="$(ls -l "$TEST_ROOT/candidate/app/bin/$marker" | cut -c 1-10)"
        [ "${mode:7:1}" = r ] || { echo 'New public marker is not world-readable' >&2; exit 1; }
    done
esac
# A caller which does not manage markers may still update an unmarked install.
app="$TEST_ROOT/direct/app"; data="$TEST_ROOT/direct/data"; work="$TEST_ROOT/direct/work"
mkdir -p "$app/bin" "$data" "$work"
printf '%s\n' '#!/bin/sh' 'printf "%s\n" "audiolib-server 0.6.1"' > "$app/bin/audiolib-server"
chmod +x "$app/bin/audiolib-server"
printf '%s\n' 'direct plist fixture' > "$app/synthetic.beta.plist"
export LAUNCHD_PLIST="$app/synthetic.beta.plist"
BETA_APP_DIR="$app" INSTALL_DIR="$app/bin" DATA_DIR="$data" BACKUP_DIR="$work/backups" \
    SERVICE_MANAGER=launchd SERVICE_NAME=synthetic.beta EXPECT_COMMIT=f54f036 \
    sh "$TEST_ROOT/repo/server/deploy/redeploy-server.sh" "$TEST_ROOT/server-binary" >/dev/null 2>&1
grep -Fx '1|0.6.2|<missing>|<missing>' "$app/start-events" >/dev/null || { echo 'Unmarked generic deployment changed its existing behavior' >&2; exit 1; }
cp "$app/bin/audiolib-server.prev" "$app/bin/audiolib-server"
printf '%s\n' stable > "$app/bin/update-channel.txt"
printf '%s\n' 0.6.1 > "$app/bin/update-version.txt"
before="$(cksum "$app/bin/audiolib-server")"
status=0
BETA_APP_DIR="$app" INSTALL_DIR="$app/bin" DATA_DIR="$data" BACKUP_DIR="$work/backups" \
    SERVICE_MANAGER=launchd SERVICE_NAME=synthetic.beta EXPECT_COMMIT=f54f036 \
    sh "$TEST_ROOT/repo/server/deploy/redeploy-server.sh" "$TEST_ROOT/server-binary" >"$work/rejected.log" 2>&1 || status=$?
[ "$status" -ne 0 ] && grep -F 'set both UPDATE_CHANNEL and UPDATE_VERSION' "$work/rejected.log" >/dev/null &&
    [ "$before" = "$(cksum "$app/bin/audiolib-server")" ] && [ "$(cat "$app/start-count")" = 1 ] || {
    echo 'A binary-only base-version change left stale metadata or restarted the service' >&2; exit 1;
}

unset LAUNCHD_PLIST

# An offline migration can leave launchd quiesced. Only the original plist may
# bootstrap it. A loaded job is booted out, waited for, swapped and bootstrapped
# from that same plist; without the plist nothing is touched at all.
run_launchd_case() {
    name="$1"; loaded="$2"; bootstrap_fail="$3"; has_plist="$4"; succeeds="$5"; starts="$6"
    health_file="${7:-}"; retained="${8:-0}"; reader="${9:-plutil}"; expected_commit="${10-f54f036}"
    wait_secs="${11:-1}"; drop_after_first="${12:-0}"
    app="$TEST_ROOT/$name/app"; data="$TEST_ROOT/$name/data"; work="$TEST_ROOT/$name/work"
    plist="$app/Launch Agents/synthetic.beta.plist"
    mkdir -p "$app/bin" "$app/Launch Agents" "$data" "$work"
    printf '%s\n' '#!/bin/sh' 'printf "%s\n" "audiolib-server 0.6.1"' > "$app/bin/audiolib-server"
    chmod +x "$app/bin/audiolib-server"
    printf '%s\n' stable > "$app/bin/update-channel.txt"
    printf '%s\n' 0.6.1 > "$app/bin/update-version.txt"
    printf '%s\n' 'existing launcher fixture' > "$app/bin/audiolib-launcher"
    printf '%s\n' 'existing settings fixture' > "$app/config.toml"
    printf '%s\n' 'existing library fixture (never opened)' > "$data/library.db"
    if [ "$has_plist" = 1 ]; then printf '%s\n' 'original plist fixture' > "$plist"; fi
    for file in "$app/bin/audiolib-launcher" "$app/config.toml" "$data/library.db"; do cp "$file" "$file.expected"; done
    if [ "$has_plist" = 1 ]; then cp "$plist" "$plist.expected"; fi
    status=0
    BETA_APP_DIR="$app" INSTALL_DIR="$app/bin" DATA_DIR="$data" BACKUP_DIR="$work/backups" \
        SERVICE_MANAGER=launchd SERVICE_NAME=synthetic.beta EXPECT_COMMIT="$expected_commit" EXPECT_SCHEMA_VERSION=6 WAIT_SECS="$wait_secs" \
        LAUNCHD_PLIST="$plist" MOCK_LAUNCHD_LOADED="$loaded" MOCK_BOOTSTRAP_FAIL="$bootstrap_fail" \
        MOCK_HEALTH_FILE="$health_file" MOCK_JSON_READER="$reader" MOCK_DROP_AFTER_FIRST="$drop_after_first" \
        UPDATE_CHANNEL=beta UPDATE_VERSION=0.6.2-beta.12 \
        sh "$TEST_ROOT/repo/server/deploy/redeploy-server.sh" "$TEST_ROOT/server-binary" >"$work/redeploy.log" 2>&1 || status=$?
    if { [ "$succeeds" = 1 ] && [ "$status" -ne 0 ]; } || { [ "$succeeds" = 0 ] && [ "$status" = 0 ]; }; then
        cat "$work/redeploy.log" >&2; echo "Unexpected deployment status for $name" >&2; exit 1
    fi
    grep -Fx "print|gui/$(id -u)/synthetic.beta|" "$app/launchctl-events" >/dev/null || {
        echo 'Deployment did not check the existing GUI launchd job' >&2; exit 1;
    }
    actual_starts=0
    if [ -f "$app/start-count" ]; then actual_starts="$(cat "$app/start-count")"; fi
    [ "$actual_starts" = "$starts" ] || { echo "Unexpected restart count for $name" >&2; exit 1; }
    ! grep -F 'kickstart|' "$app/launchctl-events" >/dev/null || { echo 'Deployment still used kickstart' >&2; exit 1; }
    if [ "$has_plist" = 1 ] && [ "$loaded" = 1 ]; then
        # bootout must precede the first bootstrap of the exact existing plist.
        first_out="$(grep -n -Fx "bootout|gui/$(id -u)/synthetic.beta|" "$app/launchctl-events" | head -n 1 | cut -d: -f1)"
        first_in="$(grep -n -Fx "bootstrap|gui/$(id -u)|$plist|" "$app/launchctl-events" | head -n 1 | cut -d: -f1)"
        [ -n "$first_out" ] && [ -n "$first_in" ] && [ "$first_out" -lt "$first_in" ] || {
            echo 'Loaded job was not booted out before bootstrapping its existing plist' >&2; exit 1;
        }
    elif [ "$has_plist" = 1 ]; then
        grep -Fx "bootstrap|gui/$(id -u)|$plist|" "$app/launchctl-events" >/dev/null || {
            echo 'Quiesced job did not bootstrap the exact existing plist' >&2; exit 1;
        }
    else
        ! grep -E '^(bootstrap|bootout)\|' "$app/launchctl-events" >/dev/null && [ ! -e "$plist" ] || {
            echo 'Missing plist triggered an invented service definition or restart' >&2; exit 1;
        }
        grep -F 'existing plist was not found' "$work/redeploy.log" >/dev/null || exit 1
    fi
    if [ "$succeeds" = 1 ]; then
        grep -Fx '1|0.6.2|beta|0.6.2-beta.12' "$app/start-events" >/dev/null || exit 1
        [ "$(cat "$app/bin/update-channel.txt")" = beta ] && [ "$(cat "$app/bin/update-version.txt")" = 0.6.2-beta.12 ] || exit 1
    elif [ "$retained" = 1 ]; then
        [ "$("$app/bin/audiolib-server" --version)" = 'audiolib-server 0.6.2' ] &&
            [ "$(cat "$app/bin/update-channel.txt")" = beta ] && [ "$(cat "$app/bin/update-version.txt")" = 0.6.2-beta.12 ] &&
            [ "$("$app/bin/audiolib-server.prev" --version)" = 'audiolib-server 0.6.1' ] || {
            echo 'Migration timeout changed the candidate or its retained rollback binary' >&2; exit 1;
        }
        grep -F 'NOT VERIFIED:' "$work/redeploy.log" >/dev/null &&
            grep -F 'migration still running; deployment acceptance failed and candidate was retained' "$work/redeploy.log" >/dev/null &&
            ! grep -E 'ROLLBACK:|OK: new binary' "$work/redeploy.log" >/dev/null || {
            echo 'Active migration was rolled back or accepted as a successful deployment' >&2; exit 1;
        }
        backup_binary="$(find "$work/backups" -name audiolib-server.bin -type f)"
        [ -n "$backup_binary" ] && [ "$("$backup_binary" --version)" = 'audiolib-server 0.6.1' ] &&
            [ "$(cat "$(dirname "$backup_binary")/update-version.txt")" = 0.6.1 ] || {
            echo 'Migration timeout did not retain the original backup and markers' >&2; exit 1;
        }
        if [ "$reader" = python ]; then
            grep -Fx python3 "$app/parser-events" >/dev/null || exit 1
        else
            grep -Fx plutil "$app/parser-events" >/dev/null && ! grep -Fx python3 "$app/parser-events" >/dev/null || {
                echo 'macOS structured-field route unexpectedly relied on Python fallback' >&2; exit 1;
            }
        fi
    else
        [ "$("$app/bin/audiolib-server" --version)" = 'audiolib-server 0.6.1' ] &&
            [ "$(cat "$app/bin/update-channel.txt")" = stable ] && [ "$(cat "$app/bin/update-version.txt")" = 0.6.1 ] || {
            echo 'Failed launchd deployment did not restore its binary and markers' >&2; exit 1;
        }
        if [ "$starts" = 2 ]; then
            grep -Fx '1|0.6.2|beta|0.6.2-beta.12' "$app/start-events" >/dev/null &&
                grep -Fx '2|0.6.1|stable|0.6.1' "$app/start-events" >/dev/null || {
                echo 'Rollback bootstrap ran before the old binary and markers were restored' >&2; exit 1;
            }
        fi
    fi
    for file in "$app/bin/audiolib-launcher" "$app/config.toml" "$data/library.db"; do cmp -s "$file" "$file.expected" || exit 1; done
    if [ "$has_plist" = 1 ]; then cmp -s "$plist" "$plist.expected" || { echo 'Deployment modified the original plist' >&2; exit 1; }; fi
    ! grep -F 'private-path-do-not-log' "$work/redeploy.log" >/dev/null || { echo 'Health response contents leaked into deployment logs' >&2; exit 1; }
}
run_launchd_case loaded 1 never 1 1 1
run_launchd_case loaded-no-plist 1 never 0 0 0
run_launchd_case quiesced 0 never 1 1 1
run_launchd_case full-expect-commit 1 never 1 1 1 '' 0 plutil f54f036aa7c0b9d6e5f4a3b2c1d0e9f8a7b6c5d4
run_launchd_case wrong-full-commit 1 never 1 0 2 '' 0 plutil f54f037aa7c0b9d6e5f4a3b2c1d0e9f8a7b6c5d4
run_launchd_case missing-plist 0 never 0 0 0
run_launchd_case bootstrap-rollback 0 first 1 0 2
run_launchd_case bootstrap-unavailable 0 always 1 0 2

# The real health contract has root status/identity and an optional upgrade object.
# plutil keypaths must not be fooled by nested status/identity or escaped strings.
cat > "$TEST_ROOT/indexing.json" <<'EOF'
{"compatibility_catalog_revision":1,"upgrade":{"phase":"indexing","completed":5,"total":10},"status":"starting","version":"0.6.2","public_url":null,"api_version":"v1","schema_version":6,"commit":"f54f036"}
EOF
cat > "$TEST_ROOT/preparing.json" <<'EOF'
{"status":"starting","upgrade":{"phase":"preparing"},"version":"0.6.2","public_url":"https://example.invalid/?text=\"status\":\"ok\"","api_version":"v1","schema_version":6,"commit":"f54f036"}
EOF
cat > "$TEST_ROOT/starting.json" <<'EOF'
{"status":"starting","version":"0.6.2","api_version":"v1","schema_version":6,"commit":"f54f036"}
EOF
cat > "$TEST_ROOT/shadow-error.json" <<'EOF'
{"status":"error","upgrade":{"phase":"indexing","status":"ok"},"version":"0.6.2","api_version":"v1","schema_version":6,"commit":"f54f036","message":"private-path-do-not-log"}
EOF
cat > "$TEST_ROOT/shadow-starting.json" <<'EOF'
{"status":"starting","upgrade":{"phase":"indexing","completed":0,"total":10,"status":"ok"},"version":"0.6.2","api_version":"v1","schema_version":6,"commit":"f54f036"}
EOF
cat > "$TEST_ROOT/wrong-commit.json" <<'EOF'
{"status":"starting","version":"0.6.2","api_version":"v1","schema_version":6,"commit":"wrong","upgrade":{"phase":"indexing","commit":"f54f036"}}
EOF
cat > "$TEST_ROOT/not-object.json" <<'EOF'
{"status":"starting","version":"0.6.2","api_version":"v1","schema_version":6,"commit":"f54f036","upgrade":"{\"phase\":\"indexing\"}"}
EOF
cat > "$TEST_ROOT/null-upgrade.json" <<'EOF'
{"status":"starting","version":"0.6.2","api_version":"v1","schema_version":6,"commit":"f54f036","upgrade":null}
EOF
sed 's/"f54f036"/"f54f036" BROKEN/' "$TEST_ROOT/indexing.json" > "$TEST_ROOT/invalid.json"
sed 's/"0.6.2"/"0.6.1"/' "$TEST_ROOT/indexing.json" > "$TEST_ROOT/wrong-version.json"
sed 's/"v1"/"v2"/' "$TEST_ROOT/indexing.json" > "$TEST_ROOT/wrong-api.json"
sed 's/"schema_version":6/"schema_version":7/' "$TEST_ROOT/indexing.json" > "$TEST_ROOT/wrong-schema.json"
sed 's/"indexing"/"complete"/' "$TEST_ROOT/indexing.json" > "$TEST_ROOT/unknown-phase.json"
sed 's/,"completed":5,"total":10//' "$TEST_ROOT/indexing.json" > "$TEST_ROOT/missing-counts.json"
run_launchd_case migration-timeout 1 never 1 0 1 "$TEST_ROOT/indexing.json" 1
run_launchd_case migration-quiesced 0 never 1 0 1 "$TEST_ROOT/preparing.json" 1
run_launchd_case migration-python-fallback 1 never 1 0 1 "$TEST_ROOT/indexing.json" 1 python
run_launchd_case migration-nested-status 1 never 1 0 1 "$TEST_ROOT/shadow-starting.json" 1
for fixture in starting shadow-error wrong-commit not-object null-upgrade invalid wrong-version wrong-api wrong-schema unknown-phase missing-counts unreachable; do
    run_launchd_case "timeout-$fixture" 1 never 1 0 2 "$TEST_ROOT/$fixture.json"
done
run_launchd_case timeout-unbound-commit 1 never 1 0 2 "$TEST_ROOT/indexing.json" 0 plutil ''
run_launchd_case timeout-lost-health 1 never 1 0 2 "$TEST_ROOT/indexing.json" 0 plutil f54f036 2 1
printf '%s\n' 'Beta deployment regressions passed: startup/rollback identities, launchd bootout/wait/bootstrap routing and recovery, short-hash prefix commit match; migration timeout retained only an explicitly identified structured upgrade, exited nonzero and never restarted twice; plutil route and optional Python fallback, invalid/nested JSON and ordinary failure rollback covered; private health messages and owned launcher/plist/configuration/database preserved.'
