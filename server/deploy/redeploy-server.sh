#!/bin/sh
# deploy/redeploy-server.sh - generic deploy/upgrade script for audiolib-server.
#
# Backs up, stops the service (launchd: bootout; systemd: stop), waits until the old
# process has exited and the port is free, replaces the server binary atomically
# (install + mv), starts the service again (launchd: bootstrap of the existing plist),
# verifies /api/v1/health and rolls back the same way (stop, wait, swap back, start)
# when verification fails, except an explicitly identified candidate still reporting
# database migration at the health timeout.
#
# Usage:  redeploy-server.sh /path/to/new/audiolib-server
# Layout 2 additionally requires explicit NEW_MIGRATOR, full PAIR_SOURCE_COMMIT,
# PAIR_TARGET and trusted EXPECT_SERVER_SHA256 / EXPECT_MIGRATOR_SHA256 identities.
# PAIR_RECEIPT + PAIR_RECEIPT_SHA256 may supply hashes from native package acceptance.
# REQUIRES_PAIR=1 forces layout 2 (beta always does); installed pair state also does.
# DATABASE_PATH must explicitly identify the existing configured library for a pair;
# DATA_DIR/LIBRARY_DB guesses are never accepted as a pair rollback safety proof.
# DEPLOY_PREFLIGHT_ONLY=1 validates and stages without stopping or backing up data.
# DEPLOY_QUIESCE_ONLY=1 stages/preflights then proves graceful stop; never installs.
# ROLLBACK_SCHEMA_VERSION optionally supplies a trusted numeric pre-refresh schema;
# beta derives it from the actual stopped library, never from a guessed path.
#
# Configuration (environment variables, all optional):
#   INSTALL_DIR   directory holding the installed binary      (default /opt/audiolib)
#   BINARY        installed binary name                       (default audiolib-server)
#   SERVICE_NAME  systemd unit or launchd label               (default audiolib-server)
#   DATA_DIR      directory with library.db                   (default /var/lib/audiolib)
#   LIBRARY_DB    library file name inside DATA_DIR           (default library.db)
#   BACKUP_DIR    where backups go                            (default $DATA_DIR/backups)
#   HOST / PORT   address the server answers on locally       (default 127.0.0.1 / 8790)
#   TOKEN         bearer token for /scans/current when auth requires one (optional;
#                 prefer exporting AUDIOLIB_SERVER_TOKEN, which is used as a fallback)
#   EXPECT_API_VERSION     expected api_version               (default v1)
#   EXPECT_SCHEMA_VERSION  expected schema_version            (optional, integer)
#   EXPECT_COMMIT          expected commit (optional). Short or full hash: /health
#                 reports a 7-character short hash, compared by prefix (minimum 7).
#                 Required for the migration-timeout retention exception. It uses
#                 system plutil or optional existing python3 for structured JSON;
#                 without either parser ordinary rollback behavior is retained.
#   SERVICE_MANAGER        systemd | launchd | none           (default: auto-detect)
#   LAUNCHD_PLIST          existing plist used for every launchd bootstrap (default
#                 $HOME/Library/LaunchAgents/$SERVICE_NAME.plist; never modified;
#                 must exist, it is checked before anything is changed)
#   LAUNCHD_DOMAIN         launchd domain                     (default gui/$(id -u))
#   STOP_TIMEOUT           seconds to wait for the old process to exit and the
#                 port to be released after stopping it        (default 30)
#   WAIT_SECS              seconds to wait for health "ok"    (default 60)
#   UPDATE_CHANNEL / UPDATE_VERSION  optional paired update markers (stable|beta,
#                 full SemVer without v). Restored before a rollback restart.
#
# Run it in a maintenance window. It never deletes data files and never uses cp on the
# running binary.
set -eu

INSTALL_DIR="${INSTALL_DIR:-/opt/audiolib}"
BINARY="${BINARY:-audiolib-server}"
SERVICE_NAME="${SERVICE_NAME:-audiolib-server}"
DATA_DIR="${DATA_DIR:-/var/lib/audiolib}"
LIBRARY_DB="${LIBRARY_DB:-library.db}"
BACKUP_DIR="${BACKUP_DIR:-$DATA_DIR/backups}"
HOST="${HOST:-127.0.0.1}"
PORT="${PORT:-8790}"
WAIT_SECS="${WAIT_SECS:-60}"
STOP_TIMEOUT="${STOP_TIMEOUT:-30}"
EXPECT_API_VERSION="${EXPECT_API_VERSION:-v1}"
BASE="http://$HOST:$PORT/api/v1"
TOKEN="${TOKEN:-${AUDIOLIB_SERVER_TOKEN:-}}"

die() { echo "error: $*" >&2; exit 1; }
log() { echo "[redeploy] $*"; }

[ $# -eq 1 ] || die "usage: $0 /path/to/new/$BINARY"
NEW="$1"
[ -f "$NEW" ] && [ -x "$NEW" ] || die "$NEW is not an executable file"
CUR="$INSTALL_DIR/$BINARY"
MIGRATOR="${MIGRATOR:-audiolib-migrate}"
CUR_MIGRATOR="$INSTALL_DIR/$MIGRATOR"
PAIR_RECORD="$INSTALL_DIR/server-deployment-pair.json"
DB="$DATA_DIR/$LIBRARY_DB"
PAIR_MODE=0; OLD_MIGRATOR_PRESENT=0; OLD_PAIR_RECORD_PRESENT=0
STAGED_SERVER=""; STAGED_MIGRATOR=""
for name in "$BINARY" "$MIGRATOR"; do
    case "$name" in ''|.|..|*/*) die "binary names must be distinct basenames" ;; esac
done
[ "$BINARY" != "$MIGRATOR" ] || die "server and migrator names must differ"
[ -f "$CUR" ] || die "$CUR not found (set INSTALL_DIR/BINARY)"
command -v curl >/dev/null 2>&1 || die "curl is required"
UPDATE_METADATA=0
if [ -n "${UPDATE_CHANNEL:-}${UPDATE_VERSION:-}" ]; then
    case "${UPDATE_CHANNEL:-}" in stable|beta) ;; *) die "UPDATE_CHANNEL must be stable or beta" ;; esac
    printf '%s\n' "${UPDATE_VERSION:-}" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-(beta|rc)\.(0|[1-9][0-9]*)|-0)?$' || die "invalid UPDATE_VERSION"
    case "$UPDATE_CHANNEL:$UPDATE_VERSION" in stable:*-*) die "stable channel cannot install a prerelease marker" ;; esac
    new_version="$("$NEW" --version)"
    [ "$new_version" = "audiolib-server ${UPDATE_VERSION%%-*}" ] || die "update marker differs from the new binary version"
    for marker in update-channel.txt update-version.txt; do
        [ ! -L "$INSTALL_DIR/$marker" ] || die "linked update marker is not supported"
        [ ! -e "$INSTALL_DIR/$marker" ] || [ -f "$INSTALL_DIR/$marker" ] || die "update marker is not a regular file"
    done
    UPDATE_METADATA=1
elif [ -e "$INSTALL_DIR/update-version.txt" ] || [ -L "$INSTALL_DIR/update-version.txt" ]; then
    [ ! -L "$INSTALL_DIR/update-version.txt" ] && [ -f "$INSTALL_DIR/update-version.txt" ] || die "update version marker is not a regular file"
    old_version="$(cat "$INSTALL_DIR/update-version.txt")"
    new_version="$("$NEW" --version)"
    [ "$new_version" = "audiolib-server ${old_version%%-*}" ] || die "existing update marker would become stale; set both UPDATE_CHANNEL and UPDATE_VERSION for this deployment"
fi

if [ -z "${SERVICE_MANAGER:-}" ]; then
    if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
        SERVICE_MANAGER=systemd
    elif command -v launchctl >/dev/null 2>&1; then
        SERVICE_MANAGER=launchd
    else
        SERVICE_MANAGER=none
    fi
fi

LAUNCHD_DOMAIN="${LAUNCHD_DOMAIN:-gui/$(id -u)}"
LAUNCHD_PLIST="${LAUNCHD_PLIST:-$HOME/Library/LaunchAgents/$SERVICE_NAME.plist}"

# Refuse before any backup or swap when the service could not be started again.
# A launchd job is always restarted with bootout + bootstrap, so the plist that
# defines it (and whose launcher holds the macOS file-access grants) must exist.
preflight_service() {
    case "$SERVICE_MANAGER" in
    launchd)
        launchctl print "$LAUNCHD_DOMAIN/$SERVICE_NAME" >/dev/null 2>&1 || true
        [ -f "$LAUNCHD_PLIST" ] || die "existing plist was not found: $LAUNCHD_PLIST (set LAUNCHD_PLIST)"
        ;;
    esac
}

launchd_loaded() { launchctl print "$LAUNCHD_DOMAIN/$SERVICE_NAME" >/dev/null 2>&1; }
launchd_pid() {
    launchctl print "$LAUNCHD_DOMAIN/$SERVICE_NAME" 2>/dev/null |
        sed -n 's/^[[:space:]]*pid = \([0-9][0-9]*\).*/\1/p' | head -n 1
}

# True while something still listens on PORT (lsof when present, else /health answers).
port_busy() {
    if command -v lsof >/dev/null 2>&1; then
        lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t >/dev/null 2>&1
    else
        curl -s -o /dev/null --max-time 2 "$BASE/health" 2>/dev/null
    fi
}

# Poll until the stopped job is gone from launchd, its old pid has exited and the
# port is free. Fails (never kills anything) after STOP_TIMEOUT seconds.
wait_stopped() {
    old_pid="$1"; i=0
    while :; do
        busy=""
        if [ "$SERVICE_MANAGER" = launchd ] && launchd_loaded; then busy="launchd job still loaded"; fi
        if [ -z "$busy" ] && [ -n "$old_pid" ] && kill -0 "$old_pid" 2>/dev/null; then busy="pid $old_pid still running"; fi
        if [ -z "$busy" ] && port_busy; then busy="port $PORT still in use"; fi
        [ -n "$busy" ] || return 0
        if [ "$i" -ge "$STOP_TIMEOUT" ]; then
            log "timed out after ${STOP_TIMEOUT}s waiting for the old server to stop: $busy"
            return 1
        fi
        i=$((i + 1))
        sleep 1
    done
}

stop_service() {
    case "$SERVICE_MANAGER" in
    systemd) systemctl stop "$SERVICE_NAME" || return 1; wait_stopped "" ;;
    launchd)
        old_pid=""
        if launchd_loaded; then
            old_pid="$(launchd_pid)"
            log "launchctl bootout $LAUNCHD_DOMAIN/$SERVICE_NAME"
            if ! launchctl bootout "$LAUNCHD_DOMAIN/$SERVICE_NAME"; then
                launchd_loaded && { log "launchctl bootout failed"; return 1; }
            fi
        else
            log "launchd job is not loaded; nothing to boot out"
        fi
        wait_stopped "$old_pid"
        ;;
    *) log "SERVICE_MANAGER=none: stop the service yourself now, then press Enter"; read -r _; wait_stopped "" ;;
    esac
}

start_service() {
    case "$SERVICE_MANAGER" in
    systemd) systemctl start "$SERVICE_NAME" ;;
    launchd)
        # Reuse the same plist and launcher so their macOS file-access grants are retained.
        [ -f "$LAUNCHD_PLIST" ] || { log "existing plist was not found: $LAUNCHD_PLIST"; return 1; }
        log "launchctl bootstrap $LAUNCHD_DOMAIN $LAUNCHD_PLIST"
        launchctl bootstrap "$LAUNCHD_DOMAIN" "$LAUNCHD_PLIST"
        ;;
    *) log "SERVICE_MANAGER=none: start the service yourself now, then press Enter"; read -r _ ;;
    esac
}

# /health reports a short hash; EXPECT_COMMIT may be short or full. Match by prefix.
commit_matches() {
    actual="$(printf '%s' "$1" | tr 'A-F' 'a-f')"; expected="$(printf '%s' "$2" | tr 'A-F' 'a-f')"
    [ "${#actual}" -ge 7 ] && [ "${#expected}" -ge 7 ] || [ "$actual" = "$expected" ] || return 1
    case "$expected" in "$actual"*) return 0 ;; esac
    case "$actual" in "$expected"*) return 0 ;; esac
    return 1
}

curl_auth() {
    if [ -n "$TOKEN" ]; then
        curl -fsS --max-time 10 -H "Authorization: Bearer $TOKEN" "$@"
    else
        curl -fsS --max-time 10 "$@"
    fi
}

# Extract a JSON field without jq: string or number value of "key".
json_field() {
    printf '%s' "$1" | sed -n "s/.*\"$2\"[[:space:]]*:[[:space:]]*\"\{0,1\}\([^\",}]*\)\"\{0,1\}.*/\1/p" | head -n 1
}

wait_health() {
    i=0
    LAST_HEALTH=""
    while [ "$i" -lt "$WAIT_SECS" ]; do
        body="$(curl -fsS --max-time 5 "$BASE/health" 2>/dev/null || true)"
        # Clear stale progress when a later request fails or no longer reports it.
        LAST_HEALTH="$body"
        if command -v plutil >/dev/null 2>&1 || command -v python3 >/dev/null 2>&1; then
            health_status="$(health_field "$body" status string)" || health_status=""
        else
            health_status="$(json_field "$body" status)"
        fi
        if [ -n "$body" ] && [ "$health_status" = "ok" ]; then
            HEALTH="$body"
            return 0
        fi
        i=$((i + 1))
        sleep 1
    done
    return 1
}

verify_health() {
    v="$(json_field "$HEALTH" version)"
    api="$(json_field "$HEALTH" api_version)"
    schema="$(json_field "$HEALTH" schema_version)"
    commit="$(json_field "$HEALTH" commit)"
    log "health: version=$v api_version=$api schema_version=$schema commit=$commit"
    [ -n "$v" ] || { log "version missing"; return 1; }
    [ "$api" = "$EXPECT_API_VERSION" ] || { log "api_version is not $EXPECT_API_VERSION"; return 1; }
    [ -n "$schema" ] || { log "schema_version missing"; return 1; }
    [ -n "$commit" ] && [ "$commit" != "unknown" ] || { log "commit missing or unknown"; return 1; }
    if [ -n "${EXPECT_SCHEMA_VERSION:-}" ] && [ "$schema" != "$EXPECT_SCHEMA_VERSION" ]; then
        log "schema_version is not $EXPECT_SCHEMA_VERSION"; return 1
    fi
    if [ -n "${EXPECT_COMMIT:-}" ] && ! commit_matches "$commit" "$EXPECT_COMMIT"; then
        log "commit is not $EXPECT_COMMIT"; return 1
    fi
    return 0
}

# Extract typed keypaths with a system JSON reader, not the permissive legacy
# display-field helper above. No health body or parser error is written to logs.
health_field() {
    if command -v plutil >/dev/null 2>&1; then
        if value="$(printf '%s' "$1" | plutil -extract "$2" raw -expect "$3" -o - -- - 2>/dev/null)"; then
            printf '%s\n' "$value"
            return 0
        fi
    fi
    command -v python3 >/dev/null 2>&1 || return 1
    printf '%s' "$1" | python3 -c '
import json, sys
def invalid(*_):
    raise ValueError("Invalid health JSON")
def unique(pairs):
    result = {}
    for key, value in pairs:
        if key in result: invalid()
        result[key] = value
    return result
value = json.load(sys.stdin, object_pairs_hook=unique, parse_constant=invalid)
for key in sys.argv[1].split("."):
    if type(value) is list and key.isdigit(): value = value[int(key)]
    elif type(value) is dict: value = value[key]
    else: invalid()
kind = {"string": str, "integer": int, "dictionary": dict, "boolean": bool}[sys.argv[2]]
if type(value) is not kind: invalid()
print("dictionary" if kind is dict else (str(value).lower() if kind is bool else value))
' "$2" "$3" 2>/dev/null
}

candidate_migration() {
    [ -n "${EXPECT_COMMIT:-}" ] && [ -n "$1" ] && [ "${#1}" -le 65536 ] || return 1
    if ! command -v plutil >/dev/null 2>&1 && ! command -v python3 >/dev/null 2>&1; then
        log "migration timeout exception unavailable: no system JSON reader" >&2
        return 1
    fi
    candidate_version="$("$NEW" --version 2>/dev/null)" || return 1
    case "$candidate_version" in 'audiolib-server '*) ;; *) return 1 ;; esac
    candidate_version="${candidate_version#audiolib-server }"
    printf '%s\n' "$candidate_version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || return 1
    [ "$(health_field "$1" status string)" = starting ] &&
        commit_matches "$(health_field "$1" commit string)" "$EXPECT_COMMIT" &&
        [ "$(health_field "$1" version string)" = "$candidate_version" ] &&
        [ "$(health_field "$1" api_version string)" = "$EXPECT_API_VERSION" ] || return 1
    candidate_schema="$(health_field "$1" schema_version integer)" || return 1
    case "$candidate_schema" in ''|*[!0-9]*) return 1 ;; esac
    [ "$candidate_schema" -gt 0 ] || return 1
    [ -z "${EXPECT_SCHEMA_VERSION:-}" ] || [ "$candidate_schema" = "$EXPECT_SCHEMA_VERSION" ] || return 1
    health_field "$1" upgrade dictionary >/dev/null || return 1
    phase="$(health_field "$1" upgrade.phase string)" || return 1
    case "$phase" in
        copying|tokenizing|indexing)
            completed="$(health_field "$1" upgrade.completed integer)" || return 1
            total="$(health_field "$1" upgrade.total integer)" || return 1
            case "$completed:$total" in *[!0-9:]*|:*|*:) return 1 ;; esac
            ;;
        preparing|backing_up|compacting|publishing|building_fts|validating|checkpoint|source_tags) ;;
        *) return 1 ;;
    esac
    printf '%s\n' "$phase"
}


# Identity and staging never invoke the migrator against a library. Its version
# is deliberately not probed: the production CLI has no --version command.
sha256_file() {
    if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print tolower($1)}'
    elif command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print tolower($1)}'
    else die "shasum or sha256sum is required"; fi
}
sha_matches() { [ "$(sha256_file "$1")" = "$(printf '%s' "$2" | tr 'A-F' 'a-f')" ]; }
receipt_check() {
    n=0
    while [ "$n" -lt 64 ]; do
        check="$(health_field "$1" "checks.$n" string)" || return 1
        [ "$check" != "$2" ] || return 0
        n=$((n + 1))
    done
    return 1
}
preflight_pair() {
    version_line="$("$NEW" --version)" || die "candidate version probe failed"
    candidate_version="${version_line#audiolib-server }"
    printf '%s\n' "$candidate_version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "unexpected candidate server version"
    major="${candidate_version%%.*}"; rest="${candidate_version#*.}"; minor="${rest%%.*}"
    case "${REQUIRES_PAIR:-0}" in 0|1) ;; *) die "REQUIRES_PAIR must be 0 or 1" ;; esac
    if [ "${REQUIRES_PAIR:-0}" = 1 ] || [ -n "${NEW_MIGRATOR:-}" ] ||
        [ -e "$CUR_MIGRATOR" ] || [ -L "$CUR_MIGRATOR" ] || [ -e "$PAIR_RECORD" ] || [ -L "$PAIR_RECORD" ] ||
        [ "$major" -ge 1 ] || [ "$minor" -ge 9 ]; then PAIR_MODE=1; fi
    [ "$PAIR_MODE" = 1 ] || return 0
    [ -n "${NEW_MIGRATOR:-}" ] && [ -f "$NEW_MIGRATOR" ] && [ -x "$NEW_MIGRATOR" ] && [ ! -L "$NEW_MIGRATOR" ] || die "layout 2 requires an explicit executable NEW_MIGRATOR"
    [ ! -L "$NEW" ] && [ ! -L "$CUR" ] || die "linked server binary is not supported"
    for file in "$CUR_MIGRATOR" "$PAIR_RECORD"; do
        [ ! -L "$file" ] && { [ ! -e "$file" ] || [ -f "$file" ]; } || die "installed pair object is not a regular file"
    done
    PAIR_SOURCE_COMMIT="${PAIR_SOURCE_COMMIT:-${EXPECT_COMMIT:-}}"
    printf '%s\n' "$PAIR_SOURCE_COMMIT" | grep -Eq '^[0-9a-f]{40}$' || die "layout 2 requires a full source commit"
    printf '%s\n' "${PAIR_TARGET:-}" | grep -Eq '^[a-z0-9_]+-[a-z0-9_-]+$' || die "layout 2 requires the native PAIR_TARGET"
    [ -n "${DATABASE_PATH:-}" ] || die "layout 2 requires explicit DATABASE_PATH from the active configuration"
    case "$DATABASE_PATH" in /*) ;; *) die "DATABASE_PATH must be absolute" ;; esac
    [ ! -L "$DATABASE_PATH" ] || die "configured DATABASE_PATH is linked"
    if [ "${DEPLOY_PREFLIGHT_ONLY:-0}" != 1 ]; then
        [ -f "$DATABASE_PATH" ] || die "configured DATABASE_PATH is missing; initialize/recover it separately"
    fi
    DB="$DATABASE_PATH"
    [ -z "${EXPECT_COMMIT:-}" ] || commit_matches "$EXPECT_COMMIT" "$PAIR_SOURCE_COMMIT" || die "pair source differs from expected health commit"
    EXPECT_COMMIT="$PAIR_SOURCE_COMMIT"
    if [ -n "${PAIR_RECEIPT:-}" ]; then
        [ -f "$PAIR_RECEIPT" ] && [ ! -L "$PAIR_RECEIPT" ] && [ -n "${PAIR_RECEIPT_SHA256:-}" ] || die "accepted receipt and its trusted SHA256 are required"
        sha_matches "$PAIR_RECEIPT" "$PAIR_RECEIPT_SHA256" || die "accepted receipt SHA256 differs"
        [ "$(wc -c < "$PAIR_RECEIPT" | tr -d ' ')" -le 65536 ] || die "accepted receipt exceeds identity limit"
        receipt="$(cat "$PAIR_RECEIPT")"
        [ "$(health_field "$receipt" success boolean)" = true ] &&
            [ "$(health_field "$receipt" layout_revision integer)" = 2 ] &&
            [ "$(health_field "$receipt" commit string)" = "$PAIR_SOURCE_COMMIT" ] &&
            [ "$(health_field "$receipt" target string)" = "$PAIR_TARGET" ] &&
            [ "$(health_field "$receipt" version string)" = "$candidate_version" ] &&
            receipt_check "$receipt" 'production migrator excludes test-hooks' &&
            receipt_check "$receipt" 'owned migrator status/start/status/rollback/status 8/9/8' || die "receipt is not matching native layout-2 production acceptance"
        server_hash="$(health_field "$receipt" binary_sha256 string)"; migrator_hash="$(health_field "$receipt" migrator_sha256 string)"
        [ -z "${EXPECT_SERVER_SHA256:-}" ] || [ "$(printf '%s' "$EXPECT_SERVER_SHA256" | tr 'A-F' 'a-f')" = "$(printf '%s' "$server_hash" | tr 'A-F' 'a-f')" ] || die "server receipt/hash identities differ"
        [ -z "${EXPECT_MIGRATOR_SHA256:-}" ] || [ "$(printf '%s' "$EXPECT_MIGRATOR_SHA256" | tr 'A-F' 'a-f')" = "$(printf '%s' "$migrator_hash" | tr 'A-F' 'a-f')" ] || die "migrator receipt/hash identities differ"
        EXPECT_SERVER_SHA256="$server_hash"; EXPECT_MIGRATOR_SHA256="$migrator_hash"
    fi
    for hash in "${EXPECT_SERVER_SHA256:-}" "${EXPECT_MIGRATOR_SHA256:-}"; do
        printf '%s\n' "$hash" | grep -Eq '^[0-9a-fA-F]{64}$' || die "both explicit pair SHA256 identities are required"
    done
    sha_matches "$NEW" "$EXPECT_SERVER_SHA256" && sha_matches "$NEW_MIGRATOR" "$EXPECT_MIGRATOR_SHA256" || die "candidate pair SHA256 differs"
}
cleanup_staging() {
    [ -z "$STAGED_SERVER" ] || rm -f -- "$STAGED_SERVER"
    [ -z "$STAGED_MIGRATOR" ] || rm -f -- "$STAGED_MIGRATOR"
}
stage_candidates() {
    STAGED_SERVER="$(mktemp "$INSTALL_DIR/.$BINARY.stage.XXXXXX")" || die "cannot reserve staged server"
    install -m 755 "$NEW" "$STAGED_SERVER" || die "cannot stage server"
    [ "$PAIR_MODE" = 1 ] || return 0
    STAGED_MIGRATOR="$(mktemp "$INSTALL_DIR/.$MIGRATOR.stage.XXXXXX")" || die "cannot reserve staged migrator"
    install -m 755 "$NEW_MIGRATOR" "$STAGED_MIGRATOR" || die "cannot stage migrator"
    sha_matches "$STAGED_SERVER" "$EXPECT_SERVER_SHA256" && sha_matches "$STAGED_MIGRATOR" "$EXPECT_MIGRATOR_SHA256" || die "staged pair SHA256 differs"
}
current_schema() {
    if [ ! -e "$DB" ]; then printf '%s\n' absent; return 0; fi
    command -v sqlite3 >/dev/null 2>&1 || return 1
    schema="$(sqlite3 -readonly "$DB" 'PRAGMA user_version;' 2>/dev/null)" || return 1
    case "$schema" in ''|*[!0-9]*) return 1 ;; esac
    printf '%s\n' "$schema"
}
preflight_service
preflight_pair
trap cleanup_staging EXIT
stage_candidates
if [ "${DEPLOY_PREFLIGHT_ONLY:-0}" = 1 ]; then
    log "preflight passed: staged candidate identities verified; no service/data changes"
    exit 0
fi
if [ "${DEPLOY_QUIESCE_ONLY:-0}" != 1 ]; then
    ENTRY_SCHEMA="$(current_schema)" || die "cannot prove the original library schema read-only; deployment not started"
    ORIGINAL_SCHEMA="$ENTRY_SCHEMA"
    if [ -n "${ROLLBACK_SCHEMA_VERSION:-}" ]; then
        printf '%s\n' "$ROLLBACK_SCHEMA_VERSION" | grep -Eq '^[0-9]+$' || die "ROLLBACK_SCHEMA_VERSION must be an explicitly trusted numeric baseline"
        ORIGINAL_SCHEMA="$ROLLBACK_SCHEMA_VERSION"
    fi
fi

# 1. No scan may be running.
log "checking for a running scan"
if scan="$(curl_auth "$BASE/scans/current" 2>/dev/null)"; then
    compact="$(printf '%s' "$scan" | tr -d ' \t\r\n')"
    case "$compact" in
    '{"scan":null}') ;;
    *'"active":true'*) die "a scan is active; wait for it or cancel it first" ;;
    esac
else
    log "warning: could not query /scans/current (server down, or a token is required: set TOKEN)"
fi
if pgrep -fl "audiolib.*(db rebuild-search|scan)" >/dev/null 2>&1; then
    die "an audiolib maintenance command is running; wait for it to finish"
fi

# Quiesce is a standalone bounded operation for beta's explicit DB refresh.
# Neither binaries/markers nor library/sidecars are replaced by this mode.
if [ "${DEPLOY_QUIESCE_ONLY:-0}" = 1 ]; then
    if ! stop_service; then
        die "old server did not drain; binaries and library/sidecars were not replaced"
    fi
    log "quiesced: service/job/pid/port have stopped; no binaries or library replaced"
    exit 0
fi

# 2. Back up the library and the old binary.
TS="$(date +%Y%m%d-%H%M%S)"
mkdir -p "$BACKUP_DIR"
DEST="$(mktemp -d "$BACKUP_DIR/$TS.XXXXXX")" || die "cannot reserve a unique backup"
cp -p "$CUR" "$DEST/$BINARY.bin"
# A running SQLite DB/WAL/SHM tuple is never copied as a backup. The logical
# backup is created only after proven drain, below, before installing either bin.

cp -p "$CUR" "$CUR.prev"
if [ "$PAIR_MODE" = 1 ]; then
    if [ -f "$CUR_MIGRATOR" ]; then
        OLD_MIGRATOR_PRESENT=1
        cp -p "$CUR_MIGRATOR" "$DEST/$MIGRATOR.bin"
        cp -p "$CUR_MIGRATOR" "$CUR_MIGRATOR.prev"
    fi
    printf '%s\n' "$OLD_MIGRATOR_PRESENT" > "$DEST/migrator-before-present.txt"
    if [ -f "$PAIR_RECORD" ]; then OLD_PAIR_RECORD_PRESENT=1; cp -p "$PAIR_RECORD" "$DEST/server-deployment-pair.json"; fi
fi
if [ "$UPDATE_METADATA" = 1 ]; then
    OLD_CHANNEL_PRESENT=0; OLD_VERSION_PRESENT=0
    [ ! -f "$INSTALL_DIR/update-channel.txt" ] || OLD_CHANNEL_PRESENT=1
    [ ! -f "$INSTALL_DIR/update-version.txt" ] || OLD_VERSION_PRESENT=1
    for marker in update-channel.txt update-version.txt; do
        if [ -f "$INSTALL_DIR/$marker" ]; then cp -p "$INSTALL_DIR/$marker" "$DEST/$marker"; fi
    done
fi

write_marker() {
    marker="$1"; value="$2"
    staged="$(mktemp "$INSTALL_DIR/.$marker.new.XXXXXX")" || return 1
    printf '%s\n' "$value" > "$staged" && chmod 644 "$staged" && mv -f "$staged" "$INSTALL_DIR/$marker" || {
        rm -f "$staged"; return 1;
    }
}
restore_markers() {
    [ "$UPDATE_METADATA" = 1 ] || return 0
    for marker in update-channel.txt update-version.txt; do
        [ ! -L "$INSTALL_DIR/$marker" ] || return 1
        original_present="$OLD_VERSION_PRESENT"
        [ "$marker" != update-channel.txt ] || original_present="$OLD_CHANNEL_PRESENT"
        if [ "$original_present" = 1 ]; then
            staged="$(mktemp "$INSTALL_DIR/.$marker.new.XXXXXX")" || return 1
            cp -p "$DEST/$marker" "$staged" && mv -f "$staged" "$INSTALL_DIR/$marker" || {
                rm -f "$staged"; return 1;
            }
        else
            # Remove only this exact script-owned marker when none existed before.
            rm -f "$INSTALL_DIR/$marker" || return 1
        fi
    done
}

TRANSACTION_STARTED=0; COMMITTED=0; ROLLBACK_DONE=0; RETAIN_MIGRATING=0
rollback() {
    ROLLBACK_DONE=1
    {
        rollback_schema="$(current_schema)" || rollback_schema=unverified
        if [ "$rollback_schema" != "$ORIGINAL_SCHEMA" ]; then
            RETAIN_MIGRATING=1
            log "NOT VERIFIED: library schema changed or cannot be verified; retained candidate pair and backups at $DEST"
            log "No binary-only downgrade performed; use the matching library backup and manual recovery."
            return 1
        fi
    }
    log "ROLLBACK: stopping the candidate, then restoring the previous binary pair"
    stop_service || { log "candidate did not stop; binary not swapped back; backup retained at $DEST"; return 1; }
    # Graceful drain can finish a migration after the pre-stop observation.
    # Recheck after the actual writer has stopped before admitting old binaries.
    {
        rollback_schema="$(current_schema)" || rollback_schema=unverified
        if [ "$rollback_schema" != "$ORIGINAL_SCHEMA" ]; then
            RETAIN_MIGRATING=1
            log "NOT VERIFIED: schema changed during drain; candidate pair retained stopped, backups at $DEST"
            log "No binary-only downgrade performed; use the matching library backup and manual recovery."
            return 1
        fi
    }
    install -m 755 "$CUR.prev" "$CUR.new" && mv -f "$CUR.new" "$CUR" || {
        log "binary rollback failed; backup retained at $DEST"; return 1;
    }
    if [ "$PAIR_MODE" = 1 ]; then
        if [ "$OLD_MIGRATOR_PRESENT" = 1 ]; then
            install -m 755 "$DEST/$MIGRATOR.bin" "$CUR_MIGRATOR.new" && mv -f "$CUR_MIGRATOR.new" "$CUR_MIGRATOR" || { log "migrator rollback failed; service remains stopped"; return 1; }
        else
            rm -f -- "$CUR_MIGRATOR" || return 1
        fi
        if [ "$OLD_PAIR_RECORD_PRESENT" = 1 ]; then
            cp -p "$DEST/server-deployment-pair.json" "$PAIR_RECORD.new" && mv -f "$PAIR_RECORD.new" "$PAIR_RECORD" || return 1
        else rm -f -- "$PAIR_RECORD" || return 1; fi
    fi
    # The old process reads its release identity once during startup, so restore
    # both markers before restarting it, rather than after this script returns.
    restore_markers || { log "marker rollback failed; backup retained at $DEST"; return 1; }
    start_service || true
    if wait_health; then
        log "rolled back; health is ok again (version $(json_field "$HEALTH" version))"
    else
        log "rolled-back server is still not healthy; library backup: $DEST"
    fi
    log "If a schema migration already ran, an older binary refuses the newer library:"
    log "restore the library files from $DEST together with the old binary (see README)."
}
on_exit() {
    result=$?
    if [ "$TRANSACTION_STARTED" = 1 ] && [ "$COMMITTED" = 0 ] && [ "$ROLLBACK_DONE" = 0 ] && [ "$RETAIN_MIGRATING" = 0 ]; then rollback || true; fi
    cleanup_staging
    exit "$result"
}
trap on_exit EXIT

# 3. Stop the old server, wait for it to exit, then replace the binary and its
#    optional release identity before starting it.
if ! stop_service; then
    # A booted-out job can still own a draining PID/port. Never bootstrap a
    # second instance when stopped state could not be proved.
    die "old server did not stop within ${STOP_TIMEOUT}s; binary not replaced"
fi
DRAINED_SCHEMA="$(current_schema)" || DRAINED_SCHEMA=unverified
printf '%s\n' "entry_schema=$ENTRY_SCHEMA" "drained_schema=$DRAINED_SCHEMA" "rollback_baseline=$ORIGINAL_SCHEMA" > "$DEST/schema-state.txt"
if [ -f "$DB" ]; then
    snapshot="$DEST/$(basename "$DB")"
    quoted_snapshot="$(printf '%s' "$snapshot" | sed 's/\\/\\\\/g; s/"/\\"/g')"
    log "backing up the stopped library logical image to $DEST"
    sqlite3 -readonly "$DB" ".backup \"$quoted_snapshot\"" || die "stopped-library backup failed; no binary replaced and service remains stopped"
    # A WAL-mode backup can retain its WAL header without companion files.
    # Finalize only this new owned image so it is independently readable.
    [ -f "$snapshot" ] && [ ! -L "$snapshot" ] &&
        snapshot_mode="$(sqlite3 "$snapshot" 'PRAGMA journal_mode=DELETE;')" && [ "$snapshot_mode" = delete ] || die "stopped-library backup finalization failed; no binary replaced and service remains stopped"
    [ "$(sqlite3 -readonly "$snapshot" 'PRAGMA quick_check;')" = ok ] || die "stopped-library backup validation failed; no binary replaced and service remains stopped"
else
    log "library is absent after drain; no logical backup available"
fi
if [ "$DRAINED_SCHEMA" != "$ENTRY_SCHEMA" ]; then
    log "No binary-only downgrade performed: library schema changed or cannot be verified during drain."
    die "candidate not installed; keep service stopped and inspect matching library/binary backups in $DEST"
fi
log "installing new binary"
TRANSACTION_STARTED=1
mv -f "$STAGED_SERVER" "$CUR"; STAGED_SERVER=""
if [ "$PAIR_MODE" = 1 ]; then
    mv -f "$STAGED_MIGRATOR" "$CUR_MIGRATOR"; STAGED_MIGRATOR=""
    sha_matches "$CUR" "$EXPECT_SERVER_SHA256" && sha_matches "$CUR_MIGRATOR" "$EXPECT_MIGRATOR_SHA256" || die "installed pair SHA256 differs"
    pair_record="$(mktemp "$INSTALL_DIR/.server-deployment-pair.new.XXXXXX")" || die "cannot stage pair identity"
    printf '{"layout_revision":2,"commit":"%s","target":"%s","binary_sha256":"%s","migrator_sha256":"%s"}\n' "$PAIR_SOURCE_COMMIT" "$PAIR_TARGET" "$EXPECT_SERVER_SHA256" "$EXPECT_MIGRATOR_SHA256" > "$pair_record"
    chmod 644 "$pair_record" && mv -f "$pair_record" "$PAIR_RECORD" || die "cannot install pair identity"
fi
if [ "$UPDATE_METADATA" = 1 ]; then
    write_marker update-channel.txt "$UPDATE_CHANNEL"
    write_marker update-version.txt "$UPDATE_VERSION"
    [ "$(cat "$INSTALL_DIR/update-channel.txt")" = "$UPDATE_CHANNEL" ] && [ "$(cat "$INSTALL_DIR/update-version.txt")" = "$UPDATE_VERSION" ] || die "installed update markers differ"
fi
if [ "$PAIR_MODE" = 1 ]; then
    sha_matches "$CUR" "$EXPECT_SERVER_SHA256" && sha_matches "$CUR_MIGRATOR" "$EXPECT_MIGRATOR_SHA256" || die "installed pair changed before startup"
fi

# 4. Start and verify.
start_service || { rollback; die "start failed"; }
if wait_health && verify_health; then
    COMMITTED=1
    log "OK: new binary is running (previous one kept as $CUR.prev, backups in $DEST)"
    exit 0
fi
if migration_phase="$(candidate_migration "$LAST_HEALTH")"; then
    RETAIN_MIGRATING=1
    log "NOT VERIFIED: health wait expired while the identified candidate reports database migration ($migration_phase)"
    log "Retained new binary pair, update markers and backups at $DEST; no rollback or second restart was performed."
    log "Check health and service logs manually; use the matching CLI migration inspect/resume/rollback workflow if recovery is needed."
    die "migration still running; deployment acceptance failed and candidate was retained"
fi
rollback
die "health verification failed; rolled back"
