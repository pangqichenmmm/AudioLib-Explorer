#!/bin/bash
# tools/beta-refresh.sh - refresh the beta audiolib-server instance (macOS, launchd).
#
# 1. builds the current origin/beta head (detached worktree under $WORK_DIR)
# 2. optional (--with-db): copies the production library with SQLite's online backup
#    (read-only for production), resets library_id (drops <db>.server-state.json)
# 3. swaps the server binary and update markers via redeploy-server.sh; the fixed
#    launcher (which holds the macOS file-access grants) is never touched
# 4. checks /health is ok and serves the beta head commit
# 5. root access probe: reads 4 KiB of one file under every configured root; a timeout
#    usually means the launcher lacks the macOS network/removable volume permission
#
# Usage: tools/beta-refresh.sh [--with-db]
#   [--no-build SERVER --migrator MIGRATOR --accepted-receipt JSON --receipt-sha256 SHA256]
# A prebuilt pair requires matching native validate-server-package acceptance,
# bound to the fetched full beta commit and caller-trusted receipt SHA256.
# Environment (required unless noted):
#   BETA_APP_DIR   beta app dir: bin/audiolib-server, bin/audiolib-launcher, config.toml, token
#   BETA_DATA_DIR  beta runtime/data dir
#   BETA_DATABASE_PATH actual absolute library_db from active config (required)
#   PROD_DB        production library.db (only with --with-db; opened read-only)
#   SERVICE_NAME   launchd label of the beta instance
#   PORT           beta port                                      (default 8791)
#   WORK_DIR       scratch dir for worktree, target, backups, logs (not the system disk)
#   ROOT_PROBE_SECS  per-root read timeout                        (default 15)
#   BETA_WAIT_SECS  startup/health wait, including library migration (default 1800)
# Never point this at the production instance. Log: $WORK_DIR/beta-refresh-<time>.log
set -eu
: "${BETA_APP_DIR:?}" "${BETA_DATA_DIR:?}" "${BETA_DATABASE_PATH:?Set the actual active config library_db path}" "${SERVICE_NAME:?}" "${WORK_DIR:?}"
PORT="${PORT:-8791}"; PROBE="${ROOT_PROBE_SECS:-15}"
BETA_WAIT_SECS="${BETA_WAIT_SECS-1800}"
[[ "$BETA_WAIT_SECS" =~ ^[1-9][0-9]*$ ]] || {
    echo '[beta-refresh] FAIL: BETA_WAIT_SECS must be a positive integer (seconds)' >&2
    exit 2
}
WITH_DB=0; PREBUILT=""; PREBUILT_MIGRATOR=""; ACCEPTED_RECEIPT=""; ACCEPTED_RECEIPT_SHA256=""
while [ $# -gt 0 ]; do case "$1" in
    --with-db) WITH_DB=1 ;;
    --no-build|--migrator|--accepted-receipt|--receipt-sha256)
        [ $# -ge 2 ] || { echo "missing value for $1" >&2; exit 2; }
        case "$1" in
            --no-build) PREBUILT="$2" ;;
            --migrator) PREBUILT_MIGRATOR="$2" ;;
            --accepted-receipt) ACCEPTED_RECEIPT="$2" ;;
            --receipt-sha256) ACCEPTED_RECEIPT_SHA256="$2" ;;
        esac
        shift ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
esac; shift; done
REPO="$(cd "$(dirname "$0")/.." && pwd)"
DB="$BETA_DATABASE_PATH"; DB_DIRECTORY="$(dirname "$DB")"; DB_NAME="$(basename "$DB")"; TOKEN_FILE="$BETA_APP_DIR/token"
UIDN="$(id -u)"; PLIST="$HOME/Library/LaunchAgents/$SERVICE_NAME.plist"
mkdir -p "$WORK_DIR"; LOG="$WORK_DIR/beta-refresh-$(date +%Y%m%d-%H%M%S).log"
exec > >(tee -a "$LOG") 2>&1
log() { echo "[beta-refresh] $*"; }
die() { echo "[beta-refresh] FAIL: $*"; echo "log: $LOG"; exit 1; }
grep -q "$PORT" "$BETA_APP_DIR/config.toml" || die "config.toml does not bind port $PORT"

git -C "$REPO" fetch -q origin +refs/heads/beta:refs/remotes/origin/beta
FULL_SHA="$(git -C "$REPO" rev-parse origin/beta)"
[[ "$FULL_SHA" =~ ^[0-9a-f]{40}$ ]] || die "beta full source commit is unavailable"
SHA="${FULL_SHA:0:7}"; log "beta head $SHA"

if [ -n "$PREBUILT" ]; then
    [ -n "$PREBUILT_MIGRATOR" ] && [ -n "$ACCEPTED_RECEIPT" ] && [ -n "$ACCEPTED_RECEIPT_SHA256" ] || die "--no-build requires explicit --migrator, --accepted-receipt and --receipt-sha256"
    NEW="$PREBUILT"; NEW_MIGRATOR="$PREBUILT_MIGRATOR"
else
    [ -z "$PREBUILT_MIGRATOR$ACCEPTED_RECEIPT$ACCEPTED_RECEIPT_SHA256" ] || die "prebuilt identity arguments require --no-build"
    WT="$WORK_DIR/wt-beta-refresh"
    [ -d "$WT" ] || git -C "$REPO" worktree add -q --detach "$WT" "$FULL_SHA"
    git -C "$WT" checkout -q --detach "$FULL_SHA"
    [ -z "$(git -C "$WT" status --porcelain)" ] || die "beta build worktree has local changes; preserve it and resolve them first"
    log "building (log $WORK_DIR/beta-build.log)"
    (cd "$WT" && CARGO_TARGET_DIR="$WORK_DIR/target" cargo build --release --locked --no-default-features \
        --target aarch64-apple-darwin -p audiolib-server -p audiolib-db --bin audiolib-server --bin audiolib-migrate) >"$WORK_DIR/beta-build.log" 2>&1 \
        || die "build failed, see $WORK_DIR/beta-build.log"
    NEW="$WORK_DIR/target/aarch64-apple-darwin/release/audiolib-server"
    NEW_MIGRATOR="$WORK_DIR/target/aarch64-apple-darwin/release/audiolib-migrate"
fi
[ -f "$NEW" ] && [ -x "$NEW" ] && [ -f "$NEW_MIGRATOR" ] && [ -x "$NEW_MIGRATOR" ] || die "both native pair executables are required"
PAIR_STAGE="$(mktemp -d "$WORK_DIR/beta-pair.XXXXXX")" || die "cannot reserve pair staging"
cp "$NEW" "$PAIR_STAGE/audiolib-server"; cp "$NEW_MIGRATOR" "$PAIR_STAGE/audiolib-migrate"
chmod 755 "$PAIR_STAGE/audiolib-server" "$PAIR_STAGE/audiolib-migrate"
NEW="$PAIR_STAGE/audiolib-server"; NEW_MIGRATOR="$PAIR_STAGE/audiolib-migrate"
sha256_file() {
    if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print tolower($1)}'
    elif command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print tolower($1)}'
    else die "shasum or sha256sum is required"; fi
}
SERVER_SHA256="$(sha256_file "$NEW")"; MIGRATOR_SHA256="$(sha256_file "$NEW_MIGRATOR")"
VERSION="$("$NEW" --version)"; VERSION="${VERSION#audiolib-server }"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "unexpected built server version"
# The binary Cargo version omits the release suffix. Preserve an exact candidate
# tag, or identify this source snapshot as a development baseline before beta.1.
UPDATE_VERSION="$VERSION-0"
for tag in $(git -C "$REPO" tag --points-at origin/beta --sort=-version:refname); do
    if [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-(beta|rc)\.(0|[1-9][0-9]*))?$ ]] &&
        [ "${tag%%-*}" = "v$VERSION" ]; then UPDATE_VERSION="${tag#v}"; break; fi
done


# Preflight the complete staged pair before --with-db can stop any service.
# A matching accepted native receipt (including production no-test-hooks proof)
# is mandatory for --no-build; the migrator has no --version command.
INSTALL_DIR="$BETA_APP_DIR/bin" SERVICE_NAME="$SERVICE_NAME" SERVICE_MANAGER=launchd \
DATA_DIR="$BETA_DATA_DIR" DATABASE_PATH="$DB" PORT="$PORT" REQUIRES_PAIR=1 \
NEW_MIGRATOR="$NEW_MIGRATOR" PAIR_SOURCE_COMMIT="$FULL_SHA" PAIR_TARGET=aarch64-apple-darwin \
EXPECT_SERVER_SHA256="$SERVER_SHA256" EXPECT_MIGRATOR_SHA256="$MIGRATOR_SHA256" \
PAIR_RECEIPT="$ACCEPTED_RECEIPT" PAIR_RECEIPT_SHA256="$ACCEPTED_RECEIPT_SHA256" DEPLOY_PREFLIGHT_ONLY=1 \
UPDATE_CHANNEL=beta UPDATE_VERSION="$UPDATE_VERSION" \
    sh "$REPO/server/deploy/redeploy-server.sh" "$NEW" || die "pair preflight failed before any service/data change"

ROLLBACK_SCHEMA_VERSION=""
if [ "$WITH_DB" = 1 ]; then
    : "${PROD_DB:?PROD_DB required with --with-db}"
    [ "$PROD_DB" != "$DB" ] && { [ ! -e "$DB" ] || [ ! "$PROD_DB" -ef "$DB" ]; } || die "PROD_DB equals beta db"
    [ -f "$PROD_DB" ] && [ -f "$DB" ] || die "production source and existing beta library must both exist; initialize a missing beta library separately"
    backup_readonly() {
        local destination journal_mode
        # sqlite3 receives the source as a filename, not a URI (# and spaces are literal).
        # The dot-command target is quoted with its documented backslash escapes.
        destination="${2//\\/\\\\}"; destination="${destination//\"/\\\"}"
        sqlite3 -readonly "$1" ".backup \"$destination\"" || return 1
        # Only the newly created owned destination is writable. Normalize its
        # WAL header before later read-only validation or a main-file move.
        [ -f "$2" ] && [ ! -L "$2" ] &&
            journal_mode="$(sqlite3 "$2" 'PRAGMA journal_mode=DELETE;')" && [ "$journal_mode" = delete ]
    }
    CLONE_DIR="$(mktemp -d "$DB_DIRECTORY/.$DB_NAME.refresh.XXXXXX")" || die "cannot reserve beta clone staging"
    CLONE="$CLONE_DIR/library.db"
    log "copying production db read-only to owned candidate (current beta image untouched)"
    backup_readonly "$PROD_DB" "$CLONE" || die "source backup failed; current beta library untouched"
    [ "$(sqlite3 -readonly "$CLONE" 'PRAGMA quick_check;')" = ok ] || die "candidate quick_check failed; current beta library untouched"
    INSTALL_DIR="$BETA_APP_DIR/bin" SERVICE_NAME="$SERVICE_NAME" SERVICE_MANAGER=launchd \
    DATA_DIR="$BETA_DATA_DIR" DATABASE_PATH="$DB" PORT="$PORT" REQUIRES_PAIR=1 \
    NEW_MIGRATOR="$NEW_MIGRATOR" PAIR_SOURCE_COMMIT="$FULL_SHA" PAIR_TARGET=aarch64-apple-darwin \
    EXPECT_SERVER_SHA256="$SERVER_SHA256" EXPECT_MIGRATOR_SHA256="$MIGRATOR_SHA256" \
    PAIR_RECEIPT="$ACCEPTED_RECEIPT" PAIR_RECEIPT_SHA256="$ACCEPTED_RECEIPT_SHA256" \
    UPDATE_CHANNEL=beta UPDATE_VERSION="$UPDATE_VERSION" DEPLOY_PREFLIGHT_ONLY=0 DEPLOY_QUIESCE_ONLY=1 \
    TOKEN="$(cat "$TOKEN_FILE")" \
        sh "$REPO/server/deploy/redeploy-server.sh" "$NEW" || die "beta did not drain; original library, sidecars and binaries retained"
    ROLLBACK_SCHEMA_VERSION="$(sqlite3 -readonly "$DB" 'PRAGMA user_version;')" || die "stopped beta schema unavailable; no library replaced; service remains quiesced"
    [[ "$ROLLBACK_SCHEMA_VERSION" =~ ^[0-9]+$ ]] || die "invalid stopped beta schema; no library replaced"
    mkdir -p "$WORK_DIR/beta-backups"
    OLD_IMAGE="$(mktemp -d "$WORK_DIR/beta-backups/library-before-refresh.XXXXXX")" || die "cannot reserve old-image backup; service remains quiesced"
    backup_readonly "$DB" "$OLD_IMAGE/library.db" || die "old beta logical backup failed; no library replaced; service remains quiesced"
    [ "$(sqlite3 -readonly "$OLD_IMAGE/library.db" 'PRAGMA quick_check;')" = ok ] &&
        [ "$(sqlite3 -readonly "$OLD_IMAGE/library.db" 'PRAGMA user_version;')" = "$ROLLBACK_SCHEMA_VERSION" ] || die "old beta backup validation failed; no library replaced"
    OLD_RAW="$(mktemp -d "$DB_DIRECTORY/.$DB_NAME.before-refresh.XXXXXX")" || die "cannot reserve old raw image; service remains quiesced"
    printf '%s\n' "schema=$ROLLBACK_SCHEMA_VERSION" "logical_backup=$OLD_IMAGE/library.db" "raw_image=$OLD_RAW" "candidate=$CLONE" 'service=quiesced; restore only matching library+binary pair' > "$OLD_IMAGE/refresh-state.txt"
    log "old stopped library backup retained at $OLD_IMAGE; raw image at $OLD_RAW"
    # Retain rather than delete old sidecars/state. A move/install failure leaves
    # the service stopped and both logical/raw recovery locations in the manifest.
    for file in "$DB" "$DB-wal" "$DB-shm" "$DB.server-state.json"; do
        [ ! -e "$file" ] || mv "$file" "$OLD_RAW/" || die "old image move failed; keep service stopped, recover from $OLD_IMAGE/refresh-state.txt"
    done
    mv "$CLONE" "$DB" || die "candidate install failed; service remains stopped, recover from $OLD_IMAGE/refresh-state.txt"
    log "beta db replaced only after proven drain; previous logical/raw images retained, library_id reset"
fi

log "swapping binary (launcher untouched; startup wait ${BETA_WAIT_SECS}s)"
# Keep binary and marker rollback in the same transaction. In particular, the
# previous markers must be restored before the previous binary is restarted.
INSTALL_DIR="$BETA_APP_DIR/bin" SERVICE_NAME="$SERVICE_NAME" SERVICE_MANAGER=launchd \
DATA_DIR="$BETA_DATA_DIR" DATABASE_PATH="$DB" BACKUP_DIR="$WORK_DIR/beta-backups" PORT="$PORT" \
DEPLOY_PREFLIGHT_ONLY=0 DEPLOY_QUIESCE_ONLY=0 ROLLBACK_SCHEMA_VERSION="$ROLLBACK_SCHEMA_VERSION" \
REQUIRES_PAIR=1 NEW_MIGRATOR="$NEW_MIGRATOR" PAIR_SOURCE_COMMIT="$FULL_SHA" PAIR_TARGET=aarch64-apple-darwin \
EXPECT_SERVER_SHA256="$SERVER_SHA256" EXPECT_MIGRATOR_SHA256="$MIGRATOR_SHA256" \
PAIR_RECEIPT="$ACCEPTED_RECEIPT" PAIR_RECEIPT_SHA256="$ACCEPTED_RECEIPT_SHA256" \
UPDATE_CHANNEL=beta UPDATE_VERSION="$UPDATE_VERSION" \
TOKEN="$(cat "$TOKEN_FILE")" EXPECT_COMMIT="$FULL_SHA" WAIT_SECS="$BETA_WAIT_SECS" \
    sh "$REPO/server/deploy/redeploy-server.sh" "$NEW" \
    || die "redeploy failed; inspect its rollback or retained-migration result above"
log "update channel beta, installed version $UPDATE_VERSION"

API="http://127.0.0.1:$PORT/api/v1"; AUTH="Authorization: Bearer $(cat "$TOKEN_FILE")"
H="$(curl -s -m 10 "$API/health")"
echo "$H" | grep -q '"status":"ok"' || die "health not ok: $H"
echo "$H" | grep -q "\"commit\":\"$SHA" || die "served commit differs from $SHA: $H"
log "PASS health ok, commit $SHA"

FAILS=0
for root in $(sed -n 's/^server_path *= *"\(.*\)"/\1/p' "$BETA_APP_DIR/config.toml" | tr ' ' '\001'); do
    root="$(printf '%s' "$root" | tr '\001' ' ')"
    id="$(sqlite3 -readonly "$DB" "select id from audio_files where missing=0 and file_path like '$root/%' limit 1;" 2>/dev/null || true)"
    [ -n "$id" ] || { log "SKIP root $root (no records)"; continue; }
    c="$(curl -s -o /dev/null -m "$PROBE" -w '%{http_code}' -H "$AUTH" -r 0-4095 "$API/audio/$id/stream" || true)"
    case "$c" in
        200|206) log "PASS root $root readable (id $id, $c)" ;;
        000) log "FAIL root $root: no answer in ${PROBE}s - grant the launcher network/removable volume access (System Settings > Privacy & Security > Files and Folders) or wake the disk"; FAILS=$((FAILS + 1)) ;;
        *) log "FAIL root $root: HTTP $c (id $id)"; FAILS=$((FAILS + 1)) ;;
    esac
done
[ "$FAILS" -eq 0 ] || die "$FAILS root(s) not readable"
log "PASS all done; log: $LOG"
