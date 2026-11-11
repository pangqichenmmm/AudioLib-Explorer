#!/bin/bash
# tools/test-beta-update-markers.sh
# Execute the real beta/redeploy scripts with fake services and an owned fixture.
set -eu
SOURCE="$(cd "$(dirname "$0")/.." && pwd)"
TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/audiolib-beta-markers.XXXXXX")"
case "$TEST_ROOT" in "${TMPDIR:-/tmp}"/audiolib-beta-markers.*) ;; *) exit 2 ;; esac
cleanup() {
    result=$?
    if [ "$result" = 0 ]; then rm -rf -- "$TEST_ROOT"
    else printf 'FAILED fixture retained: %s\n' "$TEST_ROOT" >&2; fi
    exit "$result"
}
trap cleanup EXIT
mkdir -p "$TEST_ROOT/repo/tools" "$TEST_ROOT/repo/server/deploy" "$TEST_ROOT/mock-bin"
cp "$SOURCE/tools/beta-refresh.sh" "$TEST_ROOT/repo/tools/"
cp "$SOURCE/server/deploy/redeploy-server.sh" "$TEST_ROOT/repo/server/deploy/"
# Existing Python is used only to emulate macOS plutil and exercise the optional
# Linux reader here; no parser is installed or added to application packages.
TEST_PYTHON="${TEST_PYTHON:-$(command -v python || command -v python3)}"
# Optional native CLI exercises WAL backup behavior of the host SQLite shell.
TEST_SQLITE3="${TEST_SQLITE3:-}"
[ -z "$TEST_SQLITE3" ] || [ -x "$TEST_SQLITE3" ] || { echo 'TEST_SQLITE3 must be an executable path' >&2; exit 2; }
TEST_ROOT_NATIVE="$TEST_ROOT"
if command -v cygpath >/dev/null 2>&1; then TEST_ROOT_NATIVE="$(cygpath -w "$TEST_ROOT")"; fi
export TEST_PYTHON TEST_SQLITE3 TEST_ROOT TEST_ROOT_NATIVE
TEST_CASES=0
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
    value = value[int(key)] if type(value) is list else value[key]
expected = {'string': str, 'integer': int, 'dictionary': dict, 'boolean': bool}[args[4]]
assert type(value) is expected
print('\n'.join(sorted(value)) if expected is dict else (str(value).lower() if expected is bool else value))
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
    *" rev-parse "*) printf '%s\n' f54f036aa7c0b9d6e5f4a3b2c1d0e9f8a7b6c5d4 ;;
    *" tag "*) printf '%s\n' "${MOCK_TAG:-}" ;;
    *" worktree add "*)
        while [ "$#" -gt 0 ]; do
            if [ "$1" = --detach ]; then shift; mkdir -p "$1"; break; fi
            shift
        done ;;
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
        [ "${MOCK_BOOTOUT_FAIL:-0}" != 1 ] || exit 7
        if [ "${MOCK_LAUNCHD_STUCK:-0}" = 1 ]; then exit 0; fi
        if [ "${MOCK_DB_MIGRATE_ON_STOP:-0}" = 1 ]; then "$TEST_PYTHON" "$TEST_ROOT/sqlite-cli.py" --migrate "$BETA_DATA_DIR/library.db"; fi
        if [ "${MOCK_SCHEMA_CHANGE_ON_STOP:-0}" = 1 ] && [ -f "$BETA_APP_DIR/start-count" ] && [ "$(cat "$BETA_APP_DIR/start-count")" = 1 ]; then printf '%s\n' 9 > "$BETA_APP_DIR/schema-version"; fi
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
if [ "${MOCK_SCHEMA_CHANGE:-0}" = 1 ] && [ "$number" = 1 ]; then printf '%s\n' 9 > "$BETA_APP_DIR/schema-version"; fi
pair=absent
if [ -f "$BETA_APP_DIR/bin/audiolib-migrate" ]; then
    pair=old
    if cmp -s "$BETA_APP_DIR/bin/audiolib-migrate" "$TEST_ROOT/migrator-binary"; then pair=new; fi
fi
printf '%s|%s\n' "$number" "$pair" >> "$BETA_APP_DIR/pair-start-events"
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
cat > "$TEST_ROOT/migrator-binary" <<'EOF'
#!/bin/sh
# new production-migrator fixture; deployment must never execute this binary.
printf '%s\n' unexpected >> "$BETA_APP_DIR/migrator-executions"
exit 97
EOF
cat > "$TEST_ROOT/mock-bin/sqlite3" <<'EOF'
#!/bin/sh
if [ "${MOCK_REAL_DB:-0}" = 1 ]; then exec "$TEST_PYTHON" "$TEST_ROOT/sqlite-cli.py" "$@"; fi
[ "${MOCK_SCHEMA_READ_FAIL:-0}" != 1 ] || exit 1
case " $* " in
    *'.backup '*) exec "$TEST_PYTHON" "$TEST_ROOT/sqlite-cli.py" --copy-placeholder "$@" ;;
    *'PRAGMA journal_mode=DELETE;'*) [ "$#" = 2 ] && printf '%s\n' delete ;;
    *'PRAGMA quick_check;'*) printf '%s\n' ok ;;
    *'PRAGMA user_version;'*)
        if [ -f "$BETA_APP_DIR/schema-version" ]; then cat "$BETA_APP_DIR/schema-version"; else printf '%s\n' 8; fi ;;
    *) exit 1 ;;
esac
EOF
cat > "$TEST_ROOT/sqlite-cli.py" <<'EOF'
import os, pathlib, shlex, sqlite3, subprocess, sys

def native(value):
    root=pathlib.Path(os.environ['TEST_ROOT_NATIVE'])
    marker='/'+root.name+'/'
    # Git Bash may convert plain native argv/env paths but not the filename
    # embedded in a sqlite dot-command. Bind both spellings to this owned root.
    if marker in value:
        return str(root/value.split(marker,1)[1])
    return value
args=sys.argv[1:]
if args[0]=='--copy-placeholder':
    import shutil
    assert args[1]=='-readonly'
    shutil.copyfile(native(args[2]),native(shlex.split(args[3])[1]))
    sys.exit(0)
if args[0]=='--migrate':
    with sqlite3.connect(native(args[1])) as db:
        db.execute('PRAGMA user_version=9')
        db.execute("INSERT INTO preserved VALUES ('during-drain')")
    sys.exit(0)
readonly=len(args)==3 and args[0]=='-readonly'
assert readonly or (len(args)==2 and args[1]=='PRAGMA journal_mode=DELETE;')
source=pathlib.Path(native(args[1] if readonly else args[0])).resolve()
sql=args[2] if readonly else args[1]
assert source.is_file()
root=pathlib.Path(os.environ['TEST_ROOT_NATIVE']).resolve()
assert source.is_relative_to(root)
targets=root/'owned-backup-targets.txt'
target=None
if sql.startswith('.backup '):
    assert readonly, 'original backup sources must remain read-only'
    if os.environ.get('MOCK_GENERIC_BACKUP_FAIL')=='1': raise RuntimeError('owned injected generic backup failure')
    if os.environ.get('MOCK_OLD_BACKUP_FAIL')=='1' and 'library-before-refresh.' in args[2]: raise RuntimeError('owned injected backup failure')
    target=pathlib.Path(native(shlex.split(args[2])[1])).resolve()
    assert target.is_relative_to(root) and not target.exists()
    assert target.parent.is_dir(), repr((args[2],str(target)))
elif not readonly:
    assert targets.exists() and str(source) in targets.read_text().splitlines(), 'only new owned backups may be finalized'
    failure=os.environ.get('MOCK_FINALIZE_FAIL')
    if failure=='exit': raise RuntimeError('owned injected backup finalization failure')
    if failure=='mode': print('wal'); sys.exit(0)
native_cli=os.environ.get('TEST_SQLITE3')
if native_cli:
    result=subprocess.run([native_cli,*args]).returncode
    if result: sys.exit(result)
else:
    mode='ro' if readonly else 'rw'
    with sqlite3.connect(source.as_uri()+'?mode='+mode,uri=True) as db:
        if target is not None:
            with sqlite3.connect(target) as backup: db.backup(backup)
        else:
            for row in db.execute(sql): print('|'.join(str(x) for x in row))
if target is not None:
    with targets.open('a') as registered: registered.write(str(target)+'\n')
EOF
cat > "$TEST_ROOT/mock-bin/cargo" <<'EOF'
#!/bin/sh
[ "$*" = 'build --release --locked --no-default-features --target aarch64-apple-darwin -p audiolib-server -p audiolib-db --bin audiolib-server --bin audiolib-migrate' ] || exit 67
printf '%s\n' "$*" >> "$WORK_DIR/cargo-events"
mkdir -p "$CARGO_TARGET_DIR/aarch64-apple-darwin/release"
cp "$TEST_ROOT/server-binary" "$CARGO_TARGET_DIR/aarch64-apple-darwin/release/audiolib-server"
cp "$TEST_ROOT/migrator-binary" "$CARGO_TARGET_DIR/aarch64-apple-darwin/release/audiolib-migrate"
EOF
REAL_INSTALL="$(command -v install)"; REAL_MV="$(command -v mv)"
export REAL_INSTALL REAL_MV
cat > "$TEST_ROOT/mock-bin/install" <<'EOF'
#!/bin/sh
case " $* " in *'.audiolib-migrate.stage.'*) [ "${MOCK_FAIL_MIGRATOR_STAGE:-0}" != 1 ] || exit 65 ;; esac
exec "$REAL_INSTALL" "$@"
EOF
cat > "$TEST_ROOT/mock-bin/mv" <<'EOF'
#!/bin/sh
case " $* " in *'.audiolib-migrate.stage.'*)
    [ "${MOCK_FAIL_MIGRATOR_SWAP:-0}" != 1 ] || exit 65
    if [ "${MOCK_CORRUPT_MIGRATOR_SWAP:-0}" = 1 ]; then
        "$REAL_MV" "$@"
        printf '%s\n' corrupt >> "$BETA_APP_DIR/bin/audiolib-migrate"
        exit 0
    fi ;;
esac
exec "$REAL_MV" "$@"
EOF
chmod +x "$TEST_ROOT/mock-bin/"* "$TEST_ROOT/server-binary" "$TEST_ROOT/migrator-binary"
test_sha() {
    if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print tolower($1)}'
    else sha256sum "$1" | awk '{print tolower($1)}'; fi
}
make_receipt() {
    "$TEST_PYTHON" - "$1" "$(test_sha "$TEST_ROOT/server-binary")" "$(test_sha "$TEST_ROOT/migrator-binary")" <<'EOF'
import json,sys
with open(sys.argv[1], 'w') as output:
    json.dump(dict(success=True,version='0.6.2',tag='v0.6.2-beta.12',target='aarch64-apple-darwin',
        commit='f54f036aa7c0b9d6e5f4a3b2c1d0e9f8a7b6c5d4',binary_sha256=sys.argv[2],migrator_sha256=sys.argv[3],layout_revision=2,
        checks=['production migrator excludes test-hooks','owned migrator status/start/status/rollback/status 8/9/8']),output)
EOF
}
export PATH="$TEST_ROOT/mock-bin:$PATH"
run_case() {
    TEST_CASES=$((TEST_CASES + 1))
    name="$1"; tag="$2"; expected="$3"; fail="$4"; old_markers="$5"
    loaded="${6:-1}"; old_migrator="${7:-1}"; native_build="${8:-0}"
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
    printf '%s\n' 'owned existing synthetic library' > "$data/library.db"
    if [ "$old_migrator" = 1 ]; then
        printf '%s\n' '#!/bin/sh' '# old migrator fixture' 'exit 96' > "$app/bin/audiolib-migrate"
        chmod +x "$app/bin/audiolib-migrate"; cp "$app/bin/audiolib-migrate" "$work/migrator.expected"
        printf '%s\n' '{"layout_revision":2,"prior":"fixture"}' > "$app/bin/server-deployment-pair.json"
        cp "$app/bin/server-deployment-pair.json" "$work/pair-record.expected"
    fi
    make_receipt "$work/accepted.json"
    if [ "$old_markers" = 1 ]; then
        printf '%s\n' stable > "$app/bin/update-channel.txt"
        printf '%s\n' 0.6.1 > "$app/bin/update-version.txt"
    fi
    beta_args=(--no-build "$TEST_ROOT/server-binary" --migrator "$TEST_ROOT/migrator-binary" --accepted-receipt "$work/accepted.json" --receipt-sha256 "$(test_sha "$work/accepted.json")")
    if [ "$native_build" = 1 ]; then beta_args=(); fi
    status=0
    BETA_APP_DIR="$app" BETA_DATA_DIR="$data" BETA_DATABASE_PATH="$data/library.db" SERVICE_NAME=synthetic.beta WORK_DIR="$work" \
        MOCK_TAG="$tag" MOCK_REDEPLOY_FAIL="$fail" MOCK_LAUNCHD_LOADED="$loaded" LAUNCHD_PLIST="$plist" \
        bash "$TEST_ROOT/repo/tools/beta-refresh.sh" "${beta_args[@]}" >/dev/null 2>&1 || status=$?
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
    if [ "$fail" = 1 ]; then
        if [ "$old_migrator" = 1 ]; then
            cmp -s "$app/bin/audiolib-migrate" "$work/migrator.expected" && cmp -s "$app/bin/server-deployment-pair.json" "$work/pair-record.expected" || { echo 'Pair rollback lost previous migrator/receipt' >&2; exit 1; }
            grep -Fx '2|old' "$app/pair-start-events" >/dev/null || exit 1
        else
            [ ! -e "$app/bin/audiolib-migrate" ] && [ ! -e "$app/bin/server-deployment-pair.json" ] || { echo 'Initial-pair rollback invented prior state' >&2; exit 1; }
            grep -Fx '2|absent' "$app/pair-start-events" >/dev/null || exit 1
        fi
    else
        cmp -s "$app/bin/audiolib-migrate" "$TEST_ROOT/migrator-binary" && [ -f "$app/bin/server-deployment-pair.json" ] || exit 1
        grep -Fx '1|new' "$app/pair-start-events" >/dev/null || exit 1
    fi
    [ ! -e "$app/migrator-executions" ] || { echo 'Deployment incorrectly invoked migrator --version or a library command' >&2; exit 1; }
    if [ "$native_build" = 1 ]; then [ "$(wc -l < "$work/cargo-events" | tr -d ' ')" = 1 ] || exit 1; fi
    if [ "$loaded" = 0 ]; then
        grep -Fx "bootstrap|gui/$(id -u)|$plist|" "$app/launchctl-events" >/dev/null && cmp -s "$plist" "$plist.expected" || {
            echo 'Beta refresh did not reuse the quiesced service plist' >&2; exit 1;
        }
    fi
    if [ "${MOCK_REPEAT_DEPLOY:-0}" = 1 ]; then
        TEST_CASES=$((TEST_CASES + 2))
        BETA_APP_DIR="$app" BETA_DATA_DIR="$data" BETA_DATABASE_PATH="$data/library.db" SERVICE_NAME=synthetic.beta WORK_DIR="$work" LAUNCHD_PLIST="$plist" MOCK_TAG="$tag" \
            bash "$TEST_ROOT/repo/tools/beta-refresh.sh" --no-build "$TEST_ROOT/server-binary" --migrator "$TEST_ROOT/migrator-binary" --accepted-receipt "$work/accepted.json" --receipt-sha256 "$(test_sha "$work/accepted.json")" > "$work/repeat.log" 2>&1 || { cat "$work/repeat.log" >&2; exit 1; }
        [ "$(cat "$app/start-count")" = 2 ] && grep -Fx '2|new' "$app/pair-start-events" >/dev/null && cmp -s "$app/bin/audiolib-migrate" "$TEST_ROOT/migrator-binary" || exit 1
        status=0
        BETA_APP_DIR="$app" INSTALL_DIR="$app/bin" DATA_DIR="$data" SERVICE_MANAGER=launchd SERVICE_NAME=synthetic.beta LAUNCHD_PLIST="$plist" \
            sh "$TEST_ROOT/repo/server/deploy/redeploy-server.sh" "$TEST_ROOT/server-binary" > "$work/unpaired.log" 2>&1 || status=$?
        [ "$status" -ne 0 ] && [ "$(cat "$app/start-count")" = 2 ] && grep -F 'explicit executable NEW_MIGRATOR' "$work/unpaired.log" >/dev/null || { echo 'Installed pair silently downgraded to binary-only deployment' >&2; exit 1; }
    fi
}
# Select just the affected backup paths for native SQLite acceptance.
if [ "${TEST_OWNED_BACKUPS_ONLY:-0}" != 1 ]; then
run_case development '' 0.6.2-0 0 1
run_case candidate v0.6.2-beta.12 0.6.2-beta.12 0 1
run_case stable v0.6.2 0.6.2 0 1
run_case wrong-version v0.6.3-beta.1 0.6.2-0 0 1
run_case rollback v0.6.2-beta.12 0.6.2-beta.12 1 1
run_case unmarked-rollback v0.6.2-beta.12 0.6.2-beta.12 1 0
run_case quiesced-beta v0.6.2-beta.12 0.6.2-beta.12 0 1 0
run_case initial-pair v0.6.2-beta.12 0.6.2-beta.12 0 1 1 0
run_case initial-pair-rollback v0.6.2-beta.12 0.6.2-beta.12 1 1 1 0
MOCK_REPEAT_DEPLOY=1 run_case repeated-pair v0.6.2-beta.12 0.6.2-beta.12 0 1 1 0
run_case native-pair-build '' 0.6.2-0 0 1 1 0 1
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
    TEST_CASES=$((TEST_CASES + 1))
    name="$1"; loaded="$2"; bootstrap_fail="$3"; has_plist="$4"; succeeds="$5"; starts="$6"
    health_file="${7:-}"; retained="${8:-0}"; reader="${9:-plutil}"; expected_commit="${10-f54f036}"
    wait_secs="${11:-1}"; drop_after_first="${12:-0}"; pair_mode="${13:-0}"; old_migrator="${14:-1}"
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
    pair_input=""
    if [ "$pair_mode" = 1 ]; then
        pair_input="$TEST_ROOT/migrator-binary"
        if [ "$old_migrator" = 1 ]; then
            printf '%s\n' '#!/bin/sh' '# old migrator fixture' 'exit 96' > "$app/bin/audiolib-migrate"; chmod +x "$app/bin/audiolib-migrate"
            cp "$app/bin/audiolib-migrate" "$work/migrator.expected"
        fi
    fi
    status=0
    BETA_APP_DIR="$app" INSTALL_DIR="$app/bin" DATA_DIR="$data" DATABASE_PATH="$data/library.db" BACKUP_DIR="$work/backups" \
        REQUIRES_PAIR="$pair_mode" NEW_MIGRATOR="$pair_input" PAIR_SOURCE_COMMIT=f54f036aa7c0b9d6e5f4a3b2c1d0e9f8a7b6c5d4 PAIR_TARGET=aarch64-apple-darwin \
        EXPECT_SERVER_SHA256="$(test_sha "$TEST_ROOT/server-binary")" EXPECT_MIGRATOR_SHA256="$(test_sha "$TEST_ROOT/migrator-binary")" \
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
    if [ "$pair_mode" = 1 ]; then
        if [ "$succeeds" = 1 ] || [ "$retained" = 1 ]; then
            cmp -s "$app/bin/audiolib-migrate" "$TEST_ROOT/migrator-binary" && [ -f "$app/bin/server-deployment-pair.json" ] || exit 1
            grep -Fx '1|new' "$app/pair-start-events" >/dev/null || exit 1
        elif [ "$old_migrator" = 1 ]; then cmp -s "$app/bin/audiolib-migrate" "$work/migrator.expected" || exit 1
        else [ ! -e "$app/bin/audiolib-migrate" ] || exit 1; fi
        [ ! -e "$app/migrator-executions" ] || exit 1
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
run_launchd_case paired-bootstrap-rollback 0 first 1 0 2 '' 0 plutil f54f036 1 0 1 1
run_launchd_case paired-initial-bootstrap-rollback 0 first 1 0 2 '' 0 plutil f54f036 1 0 1 0
run_launchd_case paired-migration-timeout 1 never 1 0 1 "$TEST_ROOT/indexing.json" 1 plutil f54f036 1 0 1 1
run_launchd_case paired-initial-migration-timeout 0 never 1 0 1 "$TEST_ROOT/preparing.json" 1 plutil f54f036 1 0 1 0

# These cases exercise the real paired beta/deploy path. Mocks never run a real
# service or open the fixture as SQLite; native macOS service/CLI gates remain.
run_pair_fault() {
    TEST_CASES=$((TEST_CASES + 1))
    name="$1"; fault="$2"; old_migrator="${3:-1}"
    app="$TEST_ROOT/fault-$name/app"; data="$TEST_ROOT/fault-$name/data"; work="$TEST_ROOT/fault-$name/work"
    mkdir -p "$app/bin" "$data" "$work"
    plist="$app/original.plist"; printf '%s\n' 'fixed plist' > "$plist"
    printf '%s\n' 'fixed launcher' > "$app/bin/audiolib-launcher"
    printf '%s\n' 'bind = "127.0.0.1:8791"' > "$app/config.toml"
    printf '%s\n' token > "$app/token"; printf '%s\n' 'owned DB fixture' > "$data/library.db"
    printf '%s\n' '#!/bin/sh' 'printf "%s\n" "audiolib-server 0.6.1"' > "$app/bin/audiolib-server"; chmod +x "$app/bin/audiolib-server"
    printf '%s\n' stable > "$app/bin/update-channel.txt"; printf '%s\n' 0.6.1 > "$app/bin/update-version.txt"
    if [ "$old_migrator" = 1 ]; then
        printf '%s\n' '#!/bin/sh' '# old fixture' 'exit 96' > "$app/bin/audiolib-migrate"; chmod +x "$app/bin/audiolib-migrate"
        cp "$app/bin/audiolib-migrate" "$work/old-migrator"
        printf '%s\n' '{"layout_revision":2,"prior":"original"}' > "$app/bin/server-deployment-pair.json"
        cp "$app/bin/server-deployment-pair.json" "$work/old-pair"
    fi
    for file in "$app/bin/audiolib-launcher" "$plist" "$app/config.toml" "$data/library.db"; do cp "$file" "$file.expected"; done
    make_receipt "$work/accepted.json"; receipt_hash="$(test_sha "$work/accepted.json")"
    args=(--no-build "$TEST_ROOT/server-binary" --migrator "$TEST_ROOT/migrator-binary" --accepted-receipt "$work/accepted.json" --receipt-sha256 "$receipt_hash")
    stage_fail=0; swap_fail=0; corrupt_swap=0; schema_change=0; schema_stop=0; health_fail=0
    case "$fault" in
        missing-migrator) args=(--no-build "$TEST_ROOT/server-binary" --accepted-receipt "$work/accepted.json" --receipt-sha256 "$receipt_hash") ;;
        missing-receipt) args=(--no-build "$TEST_ROOT/server-binary" --migrator "$TEST_ROOT/migrator-binary") ;;
        receipt-tamper) printf '\n' >> "$work/accepted.json" ;;
        wrong-migrator) args=(--no-build "$TEST_ROOT/server-binary" --migrator "$app/bin/audiolib-server" --accepted-receipt "$work/accepted.json" --receipt-sha256 "$receipt_hash") ;;
        wrong-source|no-production-proof)
            "$TEST_PYTHON" - "$work/accepted.json" "$fault" <<'EOF'
import json,sys
p=sys.argv[1]
with open(p) as f: value=json.load(f)
if sys.argv[2]=='wrong-source': value['commit']='f'*40
else: value['checks'].remove('production migrator excludes test-hooks')
with open(p,'w') as f:json.dump(value,f)
EOF
            args=(--no-build "$TEST_ROOT/server-binary" --migrator "$TEST_ROOT/migrator-binary" --accepted-receipt "$work/accepted.json" --receipt-sha256 "$(test_sha "$work/accepted.json")") ;;
        missing-database) rm -f "$data/library.db" ;;
        stage-failure) stage_fail=1 ;;
        partial-swap) swap_fail=1 ;;
        corrupt-installed-pair) corrupt_swap=1 ;;
        changed-schema) schema_change=1; health_fail=1 ;;
        schema-changed-during-drain) schema_stop=1; health_fail=1 ;;
        *) exit 2 ;;
    esac
    status=0
    BETA_APP_DIR="$app" BETA_DATA_DIR="$data" BETA_DATABASE_PATH="$data/library.db" SERVICE_NAME=synthetic.beta WORK_DIR="$work" LAUNCHD_PLIST="$plist" BETA_WAIT_SECS=1 \
        MOCK_FAIL_MIGRATOR_STAGE="$stage_fail" MOCK_FAIL_MIGRATOR_SWAP="$swap_fail" MOCK_CORRUPT_MIGRATOR_SWAP="$corrupt_swap" \
        MOCK_SCHEMA_CHANGE="$schema_change" MOCK_SCHEMA_CHANGE_ON_STOP="$schema_stop" MOCK_REDEPLOY_FAIL="$health_fail" \
        bash "$TEST_ROOT/repo/tools/beta-refresh.sh" "${args[@]}" > "$work/output.log" 2>&1 || status=$?
    [ "$status" -ne 0 ] || { echo "Fault was accepted: $name" >&2; exit 1; }
    starts=0; [ ! -f "$app/start-count" ] || starts="$(cat "$app/start-count")"
    case "$fault" in
        partial-swap|corrupt-installed-pair) [ "$starts" = 1 ] && grep -Fx '1|0.6.1|stable|0.6.1' "$app/start-events" >/dev/null || { cat "$work/output.log" >&2; exit 1; } ;;
        changed-schema|schema-changed-during-drain)
            [ "$starts" = 1 ] && grep -Fx '1|new' "$app/pair-start-events" >/dev/null && cmp -s "$app/bin/audiolib-migrate" "$TEST_ROOT/migrator-binary" || exit 1
            [ "$("$app/bin/audiolib-server" --version)" = 'audiolib-server 0.6.2' ] && [ "$(cat "$app/bin/update-version.txt")" = 0.6.2-0 ] || exit 1
            grep -F 'No binary-only downgrade performed' "$work"/beta-refresh-*.log >/dev/null || exit 1 ;;
        *) [ "$starts" = 0 ] && ! grep -E '^(bootout|bootstrap)\|' "$app/launchctl-events" >/dev/null 2>&1 || { cat "$work/output.log" >&2; exit 1; } ;;
    esac
    if [ "$fault" != changed-schema ] && [ "$fault" != schema-changed-during-drain ]; then
        [ "$("$app/bin/audiolib-server" --version)" = 'audiolib-server 0.6.1' ] && [ "$(cat "$app/bin/update-version.txt")" = 0.6.1 ] && [ "$(cat "$app/bin/update-channel.txt")" = stable ] || exit 1
        if [ "$old_migrator" = 1 ]; then cmp -s "$app/bin/audiolib-migrate" "$work/old-migrator" && cmp -s "$app/bin/server-deployment-pair.json" "$work/old-pair" || exit 1
        else [ ! -e "$app/bin/audiolib-migrate" ] && [ ! -e "$app/bin/server-deployment-pair.json" ] || exit 1; fi
    fi
    for file in "$app/bin/audiolib-launcher" "$plist" "$app/config.toml"; do cmp -s "$file" "$file.expected" || exit 1; done
    if [ "$fault" = missing-database ]; then [ ! -e "$data/library.db" ] || exit 1
    else cmp -s "$data/library.db" "$data/library.db.expected" || exit 1; fi
    [ ! -e "$app/migrator-executions" ] || exit 1
}
for fault in missing-migrator missing-receipt receipt-tamper wrong-migrator wrong-source no-production-proof missing-database stage-failure partial-swap corrupt-installed-pair changed-schema schema-changed-during-drain; do run_pair_fault "$fault" "$fault"; done
run_pair_fault initial-partial-swap partial-swap 0
run_pair_fault initial-corrupt-installed-pair corrupt-installed-pair 0
fi

run_with_db_case() {
    TEST_CASES=$((TEST_CASES + 1))
    name="$1"; app="$TEST_ROOT/db-$name/app"; data="$TEST_ROOT/db-$name/data space#"; work="$TEST_ROOT/db-$name/work"
    mkdir -p "$app/bin" "$data" "$work"
    plist="$app/original.plist"; printf '%s\n' 'fixed plist' > "$plist"
    printf '%s\n' 'fixed launcher' > "$app/bin/audiolib-launcher"
    printf '%s\n' 'bind = "127.0.0.1:8791"' > "$app/config.toml"; printf '%s\n' token > "$app/token"
    printf '%s\n' '#!/bin/sh' 'printf "%s\n" "audiolib-server 0.6.1"' > "$app/bin/audiolib-server"; chmod +x "$app/bin/audiolib-server"
    printf '%s\n' '#!/bin/sh' '# old migrator fixture' 'exit 96' > "$app/bin/audiolib-migrate"; chmod +x "$app/bin/audiolib-migrate"
    printf '%s\n' stable > "$app/bin/update-channel.txt"; printf '%s\n' 0.6.1 > "$app/bin/update-version.txt"
    printf '%s\n' '{"retained":"old library identity"}' > "$data/library.db.server-state.json"
    "$TEST_PYTHON" - "$TEST_ROOT_NATIVE" "${data#$TEST_ROOT/}" <<'EOF'
import os,pathlib,sqlite3,sys
path=pathlib.Path(sys.argv[1])/sys.argv[2]
with sqlite3.connect(path/'production # source.db') as db:
    db.execute('PRAGMA user_version=8');db.execute('CREATE TABLE preserved(value TEXT)');db.execute("INSERT INTO preserved VALUES ('production')")
db=sqlite3.connect(path/'library.db')
db.execute('PRAGMA journal_mode=WAL');db.execute('PRAGMA wal_autocheckpoint=0');db.execute('PRAGMA user_version=8')
db.execute('CREATE TABLE preserved(value TEXT)');db.execute("INSERT INTO preserved VALUES ('baseline-WAL')");db.commit()
db.execute('CREATE TABLE vendor_metadata(note TEXT)');db.execute("INSERT INTO vendor_metadata VALUES ('retain extension Metadata')");db.commit()
os._exit(0) # Owned fixture generator intentionally leaves committed WAL; no user process is stopped.
EOF
    for file in "$data/library.db" "$data/library.db-wal" "$data/library.db-shm" "$data/library.db.server-state.json" "$data/production # source.db" "$app/bin/audiolib-server" "$app/bin/audiolib-migrate" "$plist" "$app/bin/audiolib-launcher"; do cp "$file" "$file.expected"; done
    make_receipt "$work/accepted.json"
    bootout_fail=0; stuck=0; drain_migration=0; health_fail=0; backup_fail=0; generic_backup_fail=0; finalize_fail=0
    with_db_option=--with-db
    case "$name" in
        stop-failure) bootout_fail=1 ;;
        stop-timeout) stuck=1 ;;
        during-drain) drain_migration=1; health_fail=1 ;;
        backup-failure) backup_fail=1 ;;
        candidate-finalize-failure) finalize_fail=mode ;;
        generic-finalize-failure) with_db_option=; finalize_fail=exit ;;
        generic-backup-failure) with_db_option=; generic_backup_fail=1 ;;
        generic-drain-migration) with_db_option=; drain_migration=1 ;;
        generic-success) with_db_option= ;;
        success) ;;
        *) exit 2 ;;
    esac
    status=0
    BETA_APP_DIR="$app" BETA_DATA_DIR="$data" BETA_DATABASE_PATH="$data/library.db" PROD_DB="$data/production # source.db" SERVICE_NAME=synthetic.beta WORK_DIR="$work" LAUNCHD_PLIST="$plist" \
        STOP_TIMEOUT=0 BETA_WAIT_SECS=1 MOCK_REAL_DB=1 MOCK_BOOTOUT_FAIL="$bootout_fail" MOCK_LAUNCHD_STUCK="$stuck" \
        MOCK_DB_MIGRATE_ON_STOP="$drain_migration" MOCK_REDEPLOY_FAIL="$health_fail" MOCK_OLD_BACKUP_FAIL="$backup_fail" MOCK_GENERIC_BACKUP_FAIL="$generic_backup_fail" MOCK_FINALIZE_FAIL="$finalize_fail" \
        bash "$TEST_ROOT/repo/tools/beta-refresh.sh" ${with_db_option:+"$with_db_option"} --no-build "$TEST_ROOT/server-binary" --migrator "$TEST_ROOT/migrator-binary" --accepted-receipt "$work/accepted.json" --receipt-sha256 "$(test_sha "$work/accepted.json")" > "$work/output.log" 2>&1 || status=$?
    starts=0; [ ! -f "$app/start-count" ] || starts="$(cat "$app/start-count")"
    if [ "$name" = success ] || [ "$name" = generic-success ]; then [ "$status" = 0 ] && [ "$starts" = 1 ] || { cat "$work/output.log" >&2; exit 1; }
    else [ "$status" -ne 0 ] || exit 1; fi
    case "$name" in
        stop-failure|stop-timeout)
            [ "$starts" = 0 ] || exit 1
            for file in "$data/library.db" "$data/library.db-wal" "$data/library.db-shm" "$data/library.db.server-state.json" "$app/bin/audiolib-server" "$app/bin/audiolib-migrate"; do cmp -s "$file" "$file.expected" || { echo "Failed stop changed $file" >&2; exit 1; }; done ;;
        backup-failure|generic-backup-failure|candidate-finalize-failure|generic-finalize-failure)
            [ "$starts" = 0 ] && cmp -s "$data/library.db" "$data/library.db.expected" && cmp -s "$app/bin/audiolib-server" "$app/bin/audiolib-server.expected" && cmp -s "$app/bin/audiolib-migrate" "$app/bin/audiolib-migrate.expected" || exit 1
            if [ "$name" = candidate-finalize-failure ]; then ! grep -E '^(bootout|bootstrap)\|' "$app/launchctl-events" >/dev/null 2>&1 || exit 1; fi
            if [ "$name" = generic-finalize-failure ]; then grep -F 'backup finalization failed; no binary replaced and service remains stopped' "$work"/beta-refresh-*.log >/dev/null || exit 1; fi ;;
        generic-success)
            stopped_backup="$(find "$work/beta-backups" -name library.db -type f)"
            [ -n "$stopped_backup" ] && grep -Fx '1|new' "$app/pair-start-events" >/dev/null || exit 1
            "$TEST_PYTHON" - "$TEST_ROOT_NATIVE" "${stopped_backup#$TEST_ROOT/}" <<'EOF'
import os,pathlib,shutil,sqlite3,subprocess,sys
path=pathlib.Path(sys.argv[1])/sys.argv[2]
assert path.read_bytes()[18:20]==b'\x01\x01', 'owned snapshot must have a standalone rollback-journal header'
standalone=path.parent/'standalone # snapshot.db'
shutil.copyfile(path,standalone)
with sqlite3.connect(standalone.as_uri()+'?mode=ro',uri=True) as db:
    assert db.execute('PRAGMA quick_check').fetchone()==('ok',)
    assert db.execute('PRAGMA user_version').fetchone()==(8,)
    assert db.execute('SELECT value FROM preserved').fetchall()==[('baseline-WAL',)]
    assert db.execute('SELECT note FROM vendor_metadata').fetchall()==[('retain extension Metadata',)]
if os.environ.get('TEST_SQLITE3'):
    result=subprocess.run([os.environ['TEST_SQLITE3'],'-readonly',str(standalone),'PRAGMA quick_check; PRAGMA user_version; SELECT value FROM preserved; SELECT note FROM vendor_metadata;'],check=True,capture_output=True,text=True)
    assert result.stdout.splitlines()==['ok','8','baseline-WAL','retain extension Metadata'], result.stdout
assert not pathlib.Path(str(standalone)+'-wal').exists() and not pathlib.Path(str(standalone)+'-shm').exists()
EOF
            cmp -s "$data/library.db" "$data/library.db.expected" && cmp -s "$data/library.db-wal" "$data/library.db-wal.expected" || exit 1 ;;
        generic-drain-migration)
            [ "$starts" = 0 ] && cmp -s "$app/bin/audiolib-server" "$app/bin/audiolib-server.expected" && cmp -s "$app/bin/audiolib-migrate" "$app/bin/audiolib-migrate.expected" || exit 1
            stopped_backup="$(find "$work/beta-backups" -name library.db -type f)"
            [ -n "$stopped_backup" ] && grep -F 'No binary-only downgrade performed' "$work"/beta-refresh-*.log >/dev/null || exit 1
            "$TEST_PYTHON" - "$TEST_ROOT_NATIVE" "${stopped_backup#$TEST_ROOT/}" <<'EOF'
import pathlib,sqlite3,sys
path=pathlib.Path(sys.argv[1])/sys.argv[2]
assert path.read_bytes()[18:20]==b'\x01\x01'
with sqlite3.connect(path.as_uri()+'?mode=ro',uri=True) as db:
    assert db.execute('PRAGMA quick_check').fetchone()==('ok',)
    assert db.execute('PRAGMA user_version').fetchone()==(9,)
    assert db.execute('SELECT value FROM preserved ORDER BY value').fetchall()==[('baseline-WAL',),('during-drain',)]
EOF
            ;;
        success|during-drain)
            [ "$starts" = 1 ] && grep -Fx '1|new' "$app/pair-start-events" >/dev/null || exit 1
            old_backup="$(find "$work/beta-backups" -path '*/library-before-refresh.*/library.db' -type f)"
            raw_state="$(find "$data" -path '*/.library.db.before-refresh.*/library.db.server-state.json' -type f)"
            [ -n "$old_backup" ] && [ -n "$raw_state" ] && cmp -s "$raw_state" "$data/library.db.server-state.json.expected" || exit 1
            "$TEST_PYTHON" - "$TEST_ROOT_NATIVE" "${old_backup#$TEST_ROOT/}" "${data#$TEST_ROOT/}/library.db" "$name" <<'EOF'
import pathlib,sqlite3,sys
base=pathlib.Path(sys.argv[1]);old=base/sys.argv[2];current=base/sys.argv[3]
assert old.read_bytes()[18:20]==b'\x01\x01'
with sqlite3.connect(old.as_uri()+'?mode=ro',uri=True) as db:
    assert db.execute('PRAGMA quick_check').fetchone()==('ok',)
    rows=db.execute('SELECT value FROM preserved ORDER BY value').fetchall()
    assert ('baseline-WAL',) in rows
    schema=db.execute('PRAGMA user_version').fetchone()[0]
    assert schema==(9 if sys.argv[4]=='during-drain' else 8)
    assert (('during-drain',) in rows)==(sys.argv[4]=='during-drain')
with sqlite3.connect(current.as_uri()+'?mode=ro',uri=True) as db:
    assert db.execute('SELECT value FROM preserved').fetchall()==[('production',)]
    assert db.execute('PRAGMA user_version').fetchone()==(8,)
EOF
            if [ "$name" = during-drain ]; then grep -F 'No binary-only downgrade performed' "$work"/beta-refresh-*.log >/dev/null || exit 1; fi ;;
    esac
    for file in "$data/production # source.db" "$plist" "$app/bin/audiolib-launcher"; do cmp -s "$file" "$file.expected" || exit 1; done
}
for db_case in stop-failure stop-timeout backup-failure candidate-finalize-failure generic-finalize-failure success during-drain generic-success generic-backup-failure generic-drain-migration; do run_with_db_case "$db_case"; done
if [ "${TEST_OWNED_BACKUPS_ONLY:-0}" = 1 ]; then
    printf 'Owned backup deployment cases passed: %s (native SQLite: %s)\n' "$TEST_CASES" "${TEST_SQLITE3:-Python adapter}"
else
    printf 'Deployment scenario cases: %s\n' "$((TEST_CASES + 2))"
    printf '%s\n' 'Beta deployment regressions passed: startup/rollback identities, launchd bootout/wait/bootstrap routing and recovery, short-hash prefix commit match; migration timeout retained only an explicitly identified structured upgrade, exited nonzero and never restarted twice; plutil route and optional Python fallback, invalid/nested JSON and ordinary failure rollback covered; private health messages and owned launcher/plist/configuration/database preserved.'
fi
