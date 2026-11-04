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
# Usage: tools/beta-refresh.sh [--with-db] [--no-build BINARY]
# Environment (required unless noted):
#   BETA_APP_DIR   beta app dir: bin/audiolib-server, bin/audiolib-launcher, config.toml, token
#   BETA_DATA_DIR  beta data dir holding library.db
#   PROD_DB        production library.db (only with --with-db; opened read-only)
#   SERVICE_NAME   launchd label of the beta instance
#   PORT           beta port                                      (default 8791)
#   WORK_DIR       scratch dir for worktree, target, backups, logs (not the system disk)
#   ROOT_PROBE_SECS  per-root read timeout                        (default 15)
#   BETA_WAIT_SECS  startup/health wait, including library migration (default 1800)
# Never point this at the production instance. Log: $WORK_DIR/beta-refresh-<time>.log
set -eu
: "${BETA_APP_DIR:?}" "${BETA_DATA_DIR:?}" "${SERVICE_NAME:?}" "${WORK_DIR:?}"
PORT="${PORT:-8791}"; PROBE="${ROOT_PROBE_SECS:-15}"
BETA_WAIT_SECS="${BETA_WAIT_SECS-1800}"
[[ "$BETA_WAIT_SECS" =~ ^[1-9][0-9]*$ ]] || {
    echo '[beta-refresh] FAIL: BETA_WAIT_SECS must be a positive integer (seconds)' >&2
    exit 2
}
WITH_DB=0; PREBUILT=""
while [ $# -gt 0 ]; do case "$1" in
    --with-db) WITH_DB=1 ;;
    --no-build) PREBUILT="$2"; shift ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
esac; shift; done
REPO="$(cd "$(dirname "$0")/.." && pwd)"
DB="$BETA_DATA_DIR/library.db"; TOKEN_FILE="$BETA_APP_DIR/token"
UIDN="$(id -u)"; PLIST="$HOME/Library/LaunchAgents/$SERVICE_NAME.plist"
mkdir -p "$WORK_DIR"; LOG="$WORK_DIR/beta-refresh-$(date +%Y%m%d-%H%M%S).log"
exec > >(tee -a "$LOG") 2>&1
log() { echo "[beta-refresh] $*"; }
die() { echo "[beta-refresh] FAIL: $*"; echo "log: $LOG"; exit 1; }
grep -q "$PORT" "$BETA_APP_DIR/config.toml" || die "config.toml does not bind port $PORT"

git -C "$REPO" fetch -q origin +refs/heads/beta:refs/remotes/origin/beta
SHA="$(git -C "$REPO" rev-parse --short=7 origin/beta)"; log "beta head $SHA"

if [ -n "$PREBUILT" ]; then NEW="$PREBUILT"; else
    WT="$WORK_DIR/wt-beta-refresh"
    [ -d "$WT" ] || git -C "$REPO" worktree add -q --detach "$WT" origin/beta
    git -C "$WT" checkout -q --detach origin/beta
    log "building (log $WORK_DIR/beta-build.log)"
    (cd "$WT/server" && CARGO_TARGET_DIR="$WORK_DIR/target" cargo build --release \
        --target aarch64-apple-darwin -p audiolib-server) >"$WORK_DIR/beta-build.log" 2>&1 \
        || die "build failed, see $WORK_DIR/beta-build.log"
    NEW="$WORK_DIR/target/aarch64-apple-darwin/release/audiolib-server"
fi
[ "$NEW" -ef "$BETA_APP_DIR/bin/audiolib-server.$SHA" ] || cp "$NEW" "$BETA_APP_DIR/bin/audiolib-server.$SHA"
VERSION="$("$NEW" --version)"; VERSION="${VERSION#audiolib-server }"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "unexpected built server version"
# The binary Cargo version omits the release suffix. Preserve an exact candidate
# tag, or identify this source snapshot as a development baseline before beta.1.
UPDATE_VERSION="$VERSION-0"
for tag in $(git -C "$REPO" tag --points-at origin/beta --sort=-version:refname); do
    if [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-(beta|rc)\.(0|[1-9][0-9]*))?$ ]] &&
        [ "${tag%%-*}" = "v$VERSION" ]; then UPDATE_VERSION="${tag#v}"; break; fi
done

if [ "$WITH_DB" = 1 ]; then
    : "${PROD_DB:?PROD_DB required with --with-db}"
    [ "$PROD_DB" != "$DB" ] || die "PROD_DB equals beta db"
    log "copying production db (sqlite .backup, read-only on source)"
    rm -f "$DB.new"
    sqlite3 "file:$PROD_DB?mode=ro" ".backup '$DB.new'" || die "sqlite backup failed"
    [ "$(sqlite3 "$DB.new" 'pragma quick_check;')" = ok ] || die "quick_check failed on copy"
    launchctl bootout "gui/$UIDN/$SERVICE_NAME" 2>/dev/null || true
    mv "$DB.new" "$DB"; rm -f "$DB-wal" "$DB-shm" "$DB.server-state.json"
    launchctl bootstrap "gui/$UIDN" "$PLIST"
    log "db replaced, library_id reset"
fi

log "swapping binary (launcher untouched; startup wait ${BETA_WAIT_SECS}s)"
# Keep binary and marker rollback in the same transaction. In particular, the
# previous markers must be restored before the previous binary is restarted.
INSTALL_DIR="$BETA_APP_DIR/bin" SERVICE_NAME="$SERVICE_NAME" SERVICE_MANAGER=launchd \
DATA_DIR="$BETA_DATA_DIR" BACKUP_DIR="$WORK_DIR/beta-backups" PORT="$PORT" \
UPDATE_CHANNEL=beta UPDATE_VERSION="$UPDATE_VERSION" \
TOKEN="$(cat "$TOKEN_FILE")" EXPECT_COMMIT="$SHA" WAIT_SECS="$BETA_WAIT_SECS" \
    sh "$REPO/server/deploy/redeploy-server.sh" "$BETA_APP_DIR/bin/audiolib-server.$SHA" \
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
    id="$(sqlite3 "file:$DB?mode=ro" "select id from audio_files where missing=0 and file_path like '$root/%' limit 1;" 2>/dev/null || true)"
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
