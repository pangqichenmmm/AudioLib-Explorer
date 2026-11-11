# Deploying audiolib-server

Generic deployment, upgrade and rollback guide. All paths, addresses and names below are
documentation placeholders: `192.0.2.10` (RFC 5737), `audiolib.example.com`,
`/srv/audiolib/music` (audio), `/var/lib/audiolib` (library and data), `/opt/audiolib`
(binary), `/etc/audiolib` (config, token file), `/usr/local/var/audiolib` (macOS).

| Path | Purpose |
|---|---|
| `systemd/audiolib-server.service` | Linux unit (dedicated user, `EnvironmentFile`, sandboxing) |
| `launchd/io.github.audiolib.server.plist.example` | macOS LaunchAgent example |
| `docker/Dockerfile`, `docker/docker-compose.yml`, `docker/config.docker.toml` | container image and compose example |
| `redeploy-server.sh` | generic deploy/upgrade script with backup, health check and rollback |
| `../audiolib-server/config.example.toml` | annotated configuration |
| `../launcher/` | optional, macOS only: stable launcher that avoids repeated TCC prompts |

## Security first

- `auth_mode = "external"` (default): requests coming **directly from the LAN need no
  token**; requests through a reverse proxy (forwarding headers) or from a non-private
  peer need `Authorization: Bearer <token>`. Use it only on a LAN you trust.
- For anything reachable from the internet: run the server behind a **reverse proxy with
  TLS**, keep `/admin` unreachable from outside (the server refuses non-LAN `Host`s
  anyway), and make sure tokens are required (`external` through the proxy, or `all`).
  Never publish the plain HTTP port. Do not use `auth_mode = "off"` outside a closed test network.
- The token is read from the environment variable named by `token_env`
  (`AUDIOLIB_SERVER_TOKEN`): at least 32 random visible ASCII characters. Keep it in a root-only
  file (`/etc/audiolib/audiolib.env`, mode 600) or the container environment, never in
  `config.toml` or in a world-readable plist. Tokens generated on the admin page are stored
  hashed in `tokens.json` next to the library (mode 0600).
- `manage_mode` (`lan` default, `all`, `off`) limits who may start scans or remove records
  and folders. Removal never touches audio files. Use `lan` or `off` for public deployments.
- Mount the music directory read-only where possible; the server only reads audio files.
- When NAT, Docker Desktop, rootless containers or a proxy can hide the real client IP,
  set `trust_lan_peers = false` and `auth_mode = "all"`. The passwordless `/admin` page
  is then disabled; edit the TOML file for administration. Private socket-peer addresses
  alone cannot establish that the original caller is trusted. The Docker example uses
  these settings and maps `127.0.0.1:8790:8790`; publish through a TLS proxy.

## Install

Official [GitHub Releases](https://github.com/Freedragonlancer/AudioLib-Explorer/releases) provide Windows and Mac clients, macOS arm64 and Linux x64 native servers, plus Docker amd64/arm64 offline images from 0.8.4. Each archive has a `.sha256`; verify it before extraction or `docker load`. Native archives contain the executable at their root and public examples under `server/`. From 0.8.5 (first included version) a prebuilt Windows x64 server ZIP is provided; earlier versions and source builds remain available. Standalone Linux arm64 server packages are not published. Keep existing libraries, roots, configuration and credentials. See [Linux/Docker delivery](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Linux_Docker_Release.md) for versioned images, Compose, architecture and acceptance details.

Build the binary first: see [Quick start](../README.md#quick-start-build-from-source)
(Linux: `RUSTUP_TOOLCHAIN=1.97.1` and an explicit `--target`). After installing, continue
with [First use](../README.md#first-use-admin-page) (token, library roots, scan) and
[Connecting the Windows client](../README.md#connecting-the-windows-client).

### Linux (systemd)

```sh
useradd --system --home /var/lib/audiolib --shell /usr/sbin/nologin audiolib
install -d -o audiolib -g audiolib /var/lib/audiolib
install -d /etc/audiolib /opt/audiolib
install -m 755 target/<target>/release/audiolib-server /opt/audiolib/audiolib-server
install -m 644 server/audiolib-server/config.example.toml /etc/audiolib/config.toml   # then edit
install -m 600 /dev/null /etc/audiolib/audiolib.env
echo 'AUDIOLIB_SERVER_TOKEN=<long random secret>' >> /etc/audiolib/audiolib.env
install -m 644 server/deploy/systemd/audiolib-server.service /etc/systemd/system/
systemctl daemon-reload && systemctl enable --now audiolib-server
curl -s http://127.0.0.1:8790/api/v1/health
```

The library (`library_db`) must already exist and be writable by the `audiolib` user.

### macOS (launchd)

Install the binary to `/opt/audiolib/`, data to `/usr/local/var/audiolib/`, then copy
`launchd/io.github.audiolib.server.plist.example` to `~/Library/LaunchAgents/`, adjust the
paths and `launchctl bootstrap "gui/$(id -u)" <plist>`. To restart after a binary swap,
`launchctl bootout "gui/$(id -u)/io.github.audiolib.server"`, wait until the process has
exited and the port is free, then `launchctl bootstrap "gui/$(id -u)" <plist>` again
(`redeploy-server.sh` does exactly this, with a timeout). If macOS repeatedly asks
for removable/network volume access after each binary swap, use the optional
[launcher](../launcher/README.md).

The example sets `EnvironmentVariables` → `AUDIOLIB_LAUNCHD_LABEL` to the plist `Label`.
Keep the two identical: self-update exits with code 75 (so `KeepAlive` relaunches via the
launcher chain) only on macOS when `AUDIOLIB_LAUNCHD_LABEL` is non-empty and exactly equals
launchd's `XPC_SERVICE_NAME`; otherwise it `exec`s in place.

### Docker

For published images, use the offline archive and `docker-compose.release.yml` described in [Linux/Docker delivery](https://github.com/Freedragonlancer/AudioLib-Explorer/blob/main/doc/Rust_Linux_Docker_Release.md). Official runtime images consume only accepted Linux packages, carry their license closure, and disable in-container updates. The following local source-build example uses the public source context selected by `.dockerignore`; Git metadata is excluded, so its health commit is `unknown`:

```sh
docker build -f server/deploy/docker/Dockerfile -t audiolib-server .
# slim image without ffmpeg (codec=flac previews then answer 503):
docker build -f server/deploy/docker/Dockerfile --build-arg WITH_FFMPEG=false -t audiolib-server:slim .
AUDIOLIB_SERVER_TOKEN='<long random secret>' docker compose -f server/deploy/docker/docker-compose.yml up -d
```

> **Verified** on arm64 (OrbStack) from commit d379894 with the stock Dockerfile: image builds (~511 MB), container starts and serves on port 8790.
>
> **已验证**：arm64（OrbStack），提交 d379894，原版 Dockerfile：镜像可构建（约 511 MB），容器可启动并在 8790 端口提供服务。

Notes / 说明:

- You must mount a volume that contains `/data/library.db`, or create the library with the client first; otherwise `/api/v1/health` keeps returning `status:"error"` (The library could not be opened; the server keeps retrying).
- The image's HEALTHCHECK requires HTTP success and `status:"ok"`; startup and failed library opens remain unhealthy.
- 必须挂载一个包含 `/data/library.db` 的卷，或者先用客户端建好库；否则 `/api/v1/health` 会一直返回 `status:"error"`（The library could not be opened; the server keeps retrying）。
- Docker HEALTHCHECK 同时要求 HTTP 成功和 `status:"ok"`；启动中或开库失败均为 unhealthy。
- Only `/data` is a volume (library.db, snapshots, caches, tokens.json). `/config` is not a volume: the image ships `/config/config.toml`, and you may optionally bind-mount your own file read-only over it (`-v ./my-config.toml:/config/config.toml:ro`).
- The bundled Docker config uses `auth_mode = "all"`. With a plain `docker run` you must pass `-e AUDIOLIB_SERVER_TOKEN=...`; otherwise the server starts but every request answers 401. The compose file enforces it with `${AUDIOLIB_SERVER_TOKEN:?}`.
- 只有 `/data` 是卷（library.db、快照、缓存、tokens.json）。`/config` 不是卷：镜像自带 `/config/config.toml`，可选地用只读 bind mount 覆盖（`-v ./my-config.toml:/config/config.toml:ro`）。
- 镜像内置配置使用 `auth_mode = "all"`。直接 `docker run` 时必须传 `-e AUDIOLIB_SERVER_TOKEN=...`，否则服务能启动，但所有请求都会返回 401；compose 文件用 `${AUDIOLIB_SERVER_TOKEN:?}` 强制要求填写。

The headless source and runtime images do not install ALSA libraries; the server builds `audiolib-audio` without `playback`. Historical stock-Dockerfile measurements above retain their original commit scope and do not establish current release acceptance.

Build dependencies in the builder stage: `build-essential`, `pkg-config`, `clang` +
`libclang-dev`. Put an existing `library.db` in the `audiolib-data` volume
(`/data`); the compose file mounts the music directory read-only at `/music`.

## Versions and what to check

Program version = workspace `Cargo.toml`, API = `/api/v1/`, library schema = `SCHEMA_VERSION`.
`GET /api/v1/health` returns `status`, `version`, `api_version`, `schema_version` and
`commit`; check all of them after every deployment.

## Automatic release updates

The next 0.9.0 server payload uses layout revision 2: both `audiolib-server`
and the production `audiolib-migrate` executable are delivered (with `.exe` on
Windows). The frozen 0.7.8 updater policy refuses this new layout. Bootstrap it
manually after validating the archive and checksum; do not use an older
server's automatic install action for this transition. Package reports record
`automatic_upgrade_compatible=false`, `manual_bootstrap_required=true` and
`minimum_updater_version=0.9.0`. The candidate program version is frozen at 0.9.0
but remains unpublished. See the [release runbook](../../doc/Rust_Release_0.9.0.md) for client-first deployment,
deferred server migration and backup/rollback pairing. Keep configuration,
credentials and the existing library outside the payload. With the service
stopped, the delivered tool supports `audiolib-migrate status|start|rollback
--db <owned-library-path>` and emits JSON; it has no `--version` command and
does not include test-only fault-injection hooks.

### Paired beta and manual deployment

`tools/beta-refresh.sh` builds `audiolib-server` and `audiolib-migrate` together in
one native `cargo build --release --locked --no-default-features` invocation,
selecting only those two bins. Beta always requires the complete layout-2 pair,
including for the version-frozen 0.9.0 candidate. Set required
`BETA_DATABASE_PATH` to the actual absolute `library_db` from the active beta
configuration; `BETA_DATA_DIR/library.db` is never inferred as that proof. It stages and verifies
both files before any service stop (including `--with-db`), and does not replace
the existing launcher, plist, configuration, credentials or TCC grants.

A prebuilt beta pair must be explicit. Supply the JSON emitted by the maintained
native `validate-server-package.ps1`, with its caller-trusted SHA256:

```sh
tools/beta-refresh.sh --no-build <accepted-server> --migrator <accepted-migrator> \
  --accepted-receipt <native-acceptance.json> --receipt-sha256 <trusted-report-sha256>
```

The report must record success, layout 2, the fetched full beta commit, native
`aarch64-apple-darwin` target, matching version, both executable hashes, production
no-test-hooks validation and the owned 8→9→8 migrator lifecycle. A package creation
report alone is insufficient. The migrator has no `--version`; no adjacent binary
is guessed, and no library command is executed as a deployment identity probe.
No jq or application Python dependency is added: macOS uses system `plutil`, with
an already available `python3` as the existing optional structured-JSON fallback.

For a paired manual redeploy, provide the actual configured database explicitly;
`DATA_DIR/library.db` is not accepted as an inferred safety proof:

```sh
INSTALL_DIR=<existing-bin-dir> DATA_DIR=<data-dir> DATABASE_PATH=<configured-existing-db> \
  SERVICE_NAME=<existing-service> REQUIRES_PAIR=1 NEW_MIGRATOR=<new-migrator> \
  PAIR_SOURCE_COMMIT=<full-source-commit> PAIR_TARGET=<native-target> \
  PAIR_RECEIPT=<native-acceptance.json> PAIR_RECEIPT_SHA256=<trusted-report-sha256> \
  UPDATE_CHANNEL=beta UPDATE_VERSION=<actual-version-or-prerelease> \
  sh server/deploy/redeploy-server.sh <new-server>
```

A trusted local native build may instead supply both `EXPECT_SERVER_SHA256` and
`EXPECT_MIGRATOR_SHA256` plus the same full source/native target identity; this is
build identity, not a native package-acceptance claim. The script-owned
`server-deployment-pair.json` records the last external script-installed pair.
Its historical hashes do not override a later Rust updater receipt: that updater
independently verifies the official archive identity. Its presence, an installed
migrator, or a 0.9+ server prevents a later binary-only deployment. Explicit legacy
layout-1 generic deployments remain supported; historical archives need manual
bootstrap and cannot satisfy the current pair requirement.

Both candidates are staged and hash-checked before stopping. Generic deployment
creates and quick-checks a read-only SQLite logical backup only after proven drain;
it never treats live, individually copied DB/WAL/SHM files as a matching snapshot.
The actual entry schema must still match after drain before either binary can be
replaced. Backup/schema failure leaves files unchanged and the service stopped; a
failed stop never bootstraps a second old instance while its PID may still drain.
After graceful stop,
both executables and update markers are installed and verified before restart.
Rollback restores the previous migrator and pair record, including their prior
absence, and restores both update markers before restarting the old server. It
never rewrites the launcher/plist or restores a library automatically. An explicit
existing database and `sqlite3 -readonly` schema proof are required for paired
replacement; a missing database must be initialized or recovered separately.
`--with-db` may prepare its existing supported read-only clone before deployment,
but preflight alone never creates a library. If the candidate changed schema, or
the schema cannot be verified, binary-only rollback is refused and the candidate
pair/backups are retained for manual matching-library recovery. Structured ongoing
migration timeout likewise retains the entire candidate pair and markers, exits
nonzero and does not stop/restart it again.

`DEPLOY_PREFLIGHT_ONLY=1` performs identity/staging validation without a service
stop or installation. `DEPLOY_QUIESCE_ONLY=1` performs the same pair preflight and
then the existing bounded launchd/systemd stop, PID/job/port drain, with no binary,
marker or database replacement. A failed stop never kills a process or replaces
the library/sidecars. Both modes are preparation steps, not deployment acceptance.
Use the normal mode to install the complete pair after successful preparation.

With `beta-refresh.sh --with-db`, both the production source and the existing beta
library must be explicit, distinct existing files. The source is opened with
`sqlite3 -readonly` (literal filenames including spaces and `#`, not an ambiguous
URI) and copied to a new owned staging database. Pair preflight happens before
any stop. The script then proves the old beta service has drained, makes and
validates a consistent logical backup of the stopped beta library including its
committed WAL image, and records its actual post-drain schema. It retains the old
raw database, WAL/SHM and server-state file in an owned directory instead of deleting
them. Only then is the staged source clone installed. The old binary is never
restarted between library replacement and candidate-pair deployment.

`refresh-state.txt` in the old logical-backup directory records the stopped schema,
logical backup, retained raw-image location and candidate location. Keep that record
and the matching binary backups until acceptance. A backup/move/install failure
leaves the service quiesced with actionable paths; there is no automatic overwrite
of a library that the candidate might already have migrated. Beta passes the
actually queried stopped schema as `ROLLBACK_SCHEMA_VERSION`. Generic use of this
optional numeric value is a caller-trusted expected baseline, not a way to infer or
downgrade a library: rollback requires matching schema before **and after** graceful
stop. A change during drain refuses old-binary startup and retains the candidate
pair stopped for manual matching-library recovery. Stop all other beta writers
within the maintenance window as well; stopping launchd is not a cross-process
administration lease for arbitrary independent CLI writers.

The owned synthetic script regressions exercise initial/repeated pair installation,
receipt/hash/source rejection, staging/partial replacement failure, old-migrator
absence restoration, marker ordering and migration retention. Owned SQLite/WAL
fixtures additionally cover failed stop, stop timeout, backup failure, source
filename quoting and migration completed during drain; the no-build acceptance
fixtures themselves remain synthetic identity reports. Native macOS
launchd/TCC, actual production migrator and real beta-library acceptance remain
required and **unverified by synthetic fixtures**. Run the native gate on the Mac;
Windows Git Bash syntax/mocks do not provide that evidence.

For native installations with automatic updates enabled, every server start checks
the official GitHub repository in the background. The listener
and library opening do not wait for GitHub. Open the LAN-only `/admin` page to see the
result, release notes, or retry a failed/offline check. Confirm the displayed release tag
to download a layout-compatible update, validate its archive and checksum, drain requests and library work, install
the package-owned files, and restart with the same arguments, environment and working
directory. Database, audio files, configuration, tokens and generated runtime data stay
outside the update payload. A running scan must finish or be cancelled before confirming.
If graceful library shutdown fails, deployment is cancelled and installed files stay as
they were; check the log and start the preserved server again.

`update-channel.txt` beside the executable selects `stable` or `beta` (missing means
stable). `update-version.txt` records the installed release tag, including the beta/rc
sequence, so restarting the same beta does not offer it again. Stable ignores pre-releases;
beta includes compatible beta/rc releases and newer stable releases. Only a matching
platform's server archive is used. This requires a published package for that platform;
source-only commits cannot be downloaded as program updates.

For a manual `redeploy-server.sh` upgrade of an installation with these markers, pass
both `UPDATE_CHANNEL` and the exact `UPDATE_VERSION` (without the `v` prefix). The script
installs them before starting the candidate and restores the previous values before a
rollback restart. It refuses a base-version change that would leave the old version
marker beside a new binary. For example, replace the tag below with the actual release:

```sh
UPDATE_CHANNEL=stable UPDATE_VERSION=0.6.2 \
  INSTALL_DIR=/opt/audiolib DATA_DIR=/var/lib/audiolib \
  sh server/deploy/redeploy-server.sh new/audiolib-server
```

Use `UPDATE_CHANNEL=beta` and the complete `X.Y.Z-beta.N` or `X.Y.Z-rc.N` marker for a
published prerelease. An untagged beta development snapshot uses `X.Y.Z-0`; do not assign
a published prerelease sequence to an arbitrary same-version build. Explicit historical layout-1 installations retain their previous binary-only
deployment behavior; an installed layout-2 pair never silently falls back to it.

The install directory must be writable by the server account. The hardened systemd
example deliberately keeps `/opt/audiolib` read-only; an administrator who opts into
self-updates must grant ownership of that application directory to the service user and
add it to `ReadWritePaths` in a systemd override. Do not grant write access to unrelated
paths or run the server as root just for updates. Permission/download/verification failure
leaves the current process running. Container deployments should update and recreate their
image instead of modifying container binaries; the startup check remains informative.
Set `AUDIOLIB_UPDATE_DISABLE=1` to suppress checks and deployment in isolated tests or
installations managed entirely by an external deployment tool.

On Unix, deployment runs after the server has drained and its async runtime has stopped,
then atomically replaces files and `exec`s the server in the same process. This retains
systemd/launchd supervision and the macOS launcher's TCC responsible-process relationship;
the stable launcher itself is never replaced. On Windows, a copied update helper waits
for the old process to exit before replacing files and restarting. Schema upgrades can
still require the maintenance planning and library backups described below; automatic
binary deployment does not make an older client compatible with a newer library.

Admin integration (same LAN/Host/origin and `X-AudioLib-Admin: 1` guard as other changes):

| Route | Behavior |
|---|---|
| `GET /admin/api/update` | Owned status: state, running version, available tag and release page |
| `POST /admin/api/update/check` | Start one background check; concurrent checks/downloads return 409 |
| `POST /admin/api/update/install` | JSON `{"tag":"vX.Y.Z-beta.N"}` must match the offered tag; starts download and graceful restart |

These endpoints never accept an arbitrary download URL, installation path or command.

## Upgrade runbook (binary and/or schema)

Rules of thumb: deploy inside a maintenance window, never delete data files, never `cp`
over a running binary.

### 0. Before the window

1. **Schema upgrades need the client released first.** If `SCHEMA_VERSION` changed, the
   server opens and migrates the library, but a client only reads snapshots in
   `MIN_SNAPSHOT_SCHEMA_VERSION..=SCHEMA_VERSION`. Publish the client that understands the
   new schema before deploying the server, otherwise existing clients cannot open the new
   snapshots.
2. **No scan running.** `curl -s http://127.0.0.1:8790/api/v1/scans/current` must return
   `{"scan":null}` (add `Authorization: Bearer <token>` if the mode needs one; the admin page
   shows the same). Wait or cancel; also make sure no `db rebuild-search` or other CLI
   maintenance runs on the library (`pgrep -fl audiolib`).
3. **Check free space, then back up the library and the old binary.** The backup needs the
   library size plus its WAL:

   ```sh
   DATA=/var/lib/audiolib                         # macOS: /usr/local/var/audiolib
   ls -l "$DATA"/library.db*                      # library + -wal size
   df -h "$DATA"                                  # free space must exceed that, with headroom
   TS=$(date +%Y%m%d-%H%M%S); mkdir -p "$DATA/backups/$TS"
   cp -p /opt/audiolib/audiolib-server "$DATA/backups/$TS/audiolib-server.bin"
   ```

   - **APFS clone (macOS, preferred):** instant and costs no space until the files diverge;
     same volume only. Take it while the server is stopped or the library is quiescent and
     copy the `-wal`/`-shm` files together with the database:

     ```sh
     cp -c "$DATA/library.db" "$DATA/backups/$TS/library.db"
     [ -f "$DATA/library.db-wal" ] && cp -c "$DATA/library.db-wal" "$DATA/backups/$TS/"
     [ -f "$DATA/library.db-shm" ] && cp -c "$DATA/library.db-shm" "$DATA/backups/$TS/"
     ```

     `cp -c` fails (rather than silently copying) when the volume is not APFS or the target is
     on another volume.
   - **Anywhere else:** use SQLite's consistent online copy, which needs the full free space:
     `sqlite3 "$DATA/library.db" ".backup $DATA/backups/$TS/library.db"`.

   Open the backup read-only and check `PRAGMA quick_check;` and the row count.
4. Record the current state to compare later: `/api/v1/library` (`audio_files` count and
   `revision`), `public_url` and the version fields from `/api/v1/health`.
5. Build the new binary and announce the **maintenance window**: the server is down from
   the restart until `/health` is `ok` (seconds for a binary swap; much longer when a schema
   migration plus tag backfill runs on open — see timings below). The FTS rebuild does
   **not** run automatically; schedule `AudioLibCli db rebuild-search` separately if needed.

### 1. Replace the binary (install + mv, not cp)

Either use the script:

```sh
: "${EXPECTED_SERVER_COMMIT:?Set the new release's expected short commit first}"
INSTALL_DIR=/opt/audiolib DATA_DIR=/var/lib/audiolib SERVICE_NAME=audiolib-server \
  EXPECT_SCHEMA_VERSION=8 EXPECT_COMMIT="$EXPECTED_SERVER_COMMIT" \
  ./server/deploy/redeploy-server.sh target/<target>/release/audiolib-server
# macOS: SERVICE_NAME=io.github.audiolib.server INSTALL_DIR=/opt/audiolib DATA_DIR=/usr/local/var/audiolib
```

It checks that no scan is running, backs up the library and the old binary to
`$DATA_DIR/backups/<timestamp>/` (plus `<binary>.prev` next to the binary), installs the new
binary under a temporary name and renames it into place, restarts the service
(systemd or launchd), waits for `/health` to report `ok`, verifies `version`, `api_version`,
`schema_version` and `commit` (not `unknown`; optionally against `EXPECT_*`), and normally
rolls back automatically if verification fails. If the latest health response still
reports `starting` with a recognized database `upgrade.phase`, an explicit matching
`EXPECT_COMMIT`, the new binary's version and matching API/schema conditions, timeout
instead exits nonzero and retains the new binary, markers and backups without another
restart. This is failed acceptance, not successful deployment. Check health/logs and use
the matching CLI's migration inspect/resume/rollback workflow; never interrupt that job
by swapping an older binary. The exception requires structured JSON extraction by system
`plutil` or optional existing `python3`; without either reader, or without explicit commit
identity, ordinary rollback behavior remains. No parser is bundled or installed. Missing
responses, ordinary startup without upgrade progress, wrong identity and `error` do not
qualify. Rust in-app auto-update has no post-restart health watchdog; this timeout belongs
to the external deployment script. Parameters: `INSTALL_DIR`, `BINARY`, `SERVICE_NAME`,
`DATA_DIR`, `LIBRARY_DB`, `BACKUP_DIR`, `HOST`, `PORT`, `TOKEN`, `SERVICE_MANAGER`, `WAIT_SECS`,
`LAUNCHD_PLIST`, `LAUNCHD_DOMAIN`, `STOP_TIMEOUT`, `EXPECT_API_VERSION`, `EXPECT_SCHEMA_VERSION`, `EXPECT_COMMIT` (documented at the top of the script).

Sequence: back up the library and binary; stop the service (launchd `bootout`, systemd
`stop`); poll until the old process has exited and `PORT` is released (fails after
`STOP_TIMEOUT` seconds without swapping); `install` + `mv` the new binary; start it again
(launchd `bootstrap` of the existing `LAUNCHD_PLIST`, which must exist and is checked before
anything changes; systemd `start`); verify `/api/v1/health`. A failed verification rolls back
along the same path (stop, wait, swap the previous binary back, start). `EXPECT_COMMIT` may be
a short or full hash: `/health` reports a 7-character short hash, compared by prefix.

Or by hand:

```sh
install -m 755 new/audiolib-server /opt/audiolib/audiolib-server.new
systemctl stop audiolib-server           # macOS: launchctl bootout "gui/$(id -u)/io.github.audiolib.server"
# wait until the old process has exited and the port is free
mv -f /opt/audiolib/audiolib-server.new /opt/audiolib/audiolib-server
systemctl start audiolib-server          # macOS: launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/io.github.audiolib.server.plist
```

Do not `cp` onto the installed binary: writing into a file a running process maps can corrupt
it, and on macOS an in-place rewrite invalidates the code signature (the process is killed on
next launch). `install` to a new name plus `mv` is an atomic rename; the old process keeps its
old inode until restarted.

### 2. Verify after the deploy

```sh
curl -s http://127.0.0.1:8790/api/v1/health
```

Check `status: "ok"`, `version`, `api_version` (`v1`), `schema_version` (the integer expected
for this release) and `commit` (the short hash of the deployed commit, not `unknown`). Compare
`/api/v1/library` `audio_files` and `revision` with the notes from step 0 (a pure binary swap
must not change them) and confirm `public_url` in `/health` is still the expected external
address (for example `https://audiolib.example.com:8443`); fetch `<public_url>/api/v1/health`
from outside the LAN as well. Watch the service log for a few minutes
(`journalctl -u audiolib-server -f`, or the launchd error log).

### 3. Rollback

- **Binary only** (library untouched, schema unchanged): reinstall the previous binary
  (`<binary>.prev` or `backups/<timestamp>/audiolib-server.bin`) with `install` + `mv`,
  restart, and repeat the verification; `version`/`commit` must be the old values.
- **Library too** (a schema migration or search rebuild ran and went wrong): stop the service
  and wait for the port to free, move the damaged `library.db`, `-wal` and `-shm` aside (do not
  delete them), restore the backup files into the data directory, restore the old binary,
  start the service and verify. **An older binary refuses a library with a newer
  `user_version`**, so the library restore and the binary rollback must go together.
- Never discard a backup until the new version has run correctly for several days.

## Search index: backfill and FTS rebuild (`db rebuild-search`)

Opening a v3/v4 library with a v5 binary migrates the schema and backfills `source_tags_text`
on open; that alone can keep `/health` at `starting` for minutes on a large library. The
full-text index is **not** rebuilt automatically. When a release changes the search text (or
the index must be repaired), stop the server or use a maintenance window with **no scan
running**, then run the CLI:

```sh
AudioLibCli db rebuild-search               # backfill + FTS rebuild (repairs even if current)
AudioLibCli db rebuild-search --if-needed   # skip an up-to-date library
# Optional: AudioLibCli --database /path/to/library.db db rebuild-search
```

Point the CLI at the right library (`--database <path>` or the configured default; see
`AudioLibCli --help`) and check `db status` first so you do not rebuild a different library.
Stage 1 backfills `source_tags_text` in resumable batches; stage 2 rebuilds the FTS index
under an exclusive lease, with an integrity check.

**Anonymized measured timings (2026-10-11, large library copy):** upgrade plus extracted-tag
backfill about 198 s; FTS rebuild about 513 s more (only via
`AudioLibCli db rebuild-search`, not on open). A smaller library upgraded in about 7 s.
Record counts and database capacity are omitted from the public report.
Scale roughly with row count and disk speed. Do not interrupt it: the backfill is resumable
and the FTS stage is protected by the rollback copy.

**Disk space:** during backfill/rebuild the FTS stage first writes a rollback copy
`audiolib-rollback-*.db` next to the library, about as large as the library itself. Reserve
that much free space before the upgrade, plus room for derived-column and index growth. Check `df -h`
before starting. Keep the copy until the new index is verified, then remove it by hand
(nothing deletes it automatically). It counts together with the backup from step 0.

## Reverse proxy checklist

The full requirements and nginx/Caddy examples are in the API document, section 2.4.
In short: forward everything (or a path prefix, stripped) to `http://192.0.2.10:8790` (or
`127.0.0.1:8790` on the same host), the proxy MUST set forwarding headers (`X-Forwarded-For`,
`X-Forwarded-Proto`, ...) so the server treats the request as external and requires a token; otherwise a same-host or LAN proxy is mistaken for a LAN-direct client,
terminate TLS at the proxy, **never expose `/admin` to the internet** (forward only
`/api/v1/`, or block `/admin` with e.g. `location /admin { return 403; }`; the server's own
LAN-only guard is a second line of defence, not a replacement), and do not buffer or time out large
streams.

## Backup and health check

- Back up `library.db` (+ `-wal`/`-shm`), `admin-settings.json`, `tokens.json` and
  `<library_db>.server-state.json` from the data directory (see step 3 of the runbook for a
  consistent copy). Snapshots and the waveform cache can be regenerated.
- Health check: `curl -s http://127.0.0.1:8790/api/v1/health` → `status` (`starting`, `ok`,
  `error`), `version`, `api_version` (`v1`), `schema_version` (5), `commit`, `public_url`.
  `/health` never needs a token.
