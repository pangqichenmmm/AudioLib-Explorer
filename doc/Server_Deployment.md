# Server deployment

How to install `audiolib-server` from a published release, configure it, run it on Windows, Linux, macOS or Docker, upgrade it, and connect a desktop client. Library roots, scanning and sidecar Metadata are in [Managing sound libraries on the server](Server_Library_Guide.md).

This describes the v0.9.3 release and the server on branch `beta`. The program version is the workspace `Cargo.toml` version, shared by the desktop client, `AudioLibCli` and the server. Paths, hosts and ports below are placeholders.

## Release assets

[v0.9.3](https://github.com/Freedragonlancer/AudioLib-Explorer/releases/tag/v0.9.3) publishes seven archives and a `.sha256` file for each (fourteen assets). The server archives are:

| Platform | Asset |
| --- | --- |
| macOS arm64 | `AudioLibServer-<tag>-macos-arm64.tar.gz` |
| Linux x64 | `AudioLibServer-<tag>-linux-x64.tar.gz` |
| Windows x64 | `AudioLibServer-<tag>-win-x64.zip` |
| Docker linux/amd64 | `AudioLibServer-<tag>-docker-linux-amd64.tar.gz` |
| Docker linux/arm64 | `AudioLibServer-<tag>-docker-linux-arm64.tar.gz` |

`<tag>` is the release tag, for example `v0.9.3`. Standalone Linux arm64 server archives are not published; that build is an input to the arm64 Docker image. Verify the `.sha256` before extracting a native archive or running `docker load`.

A native archive keeps `audiolib-server` and `audiolib-migrate` at its root (`.exe` on Windows), plus `update-channel.txt`, `update-version.txt`, `licenses/` and the public examples under `server/audiolib-server/`. Docker images keep both binaries under `/opt/audiolibserver`.

## Configuration

The process requires an explicit configuration:

```sh
audiolib-server --config /path/to/config.toml
```

`--config=<path>` is equivalent. With no `--config`, the process reads `AUDIOLIB_SERVER_CONFIG`. There is no port flag and no default config path. `-h` / `--help` and `-V` / `--version` are the other flags. `--version` prints `audiolib-server X.Y.Z`.

Copy [`server/audiolib-server/config.example.toml`](../server/audiolib-server/config.example.toml) outside the repository and edit it. Relative paths resolve against the directory that contains the config file. The example comments document every field. The settings that a first install has to set are:

| Field | Role |
| --- | --- |
| `bind` | Listen address, including the port. |
| `library_db` | Existing SQLite library. The server opens this file; it does not create a library. |
| `[[roots]]` | Named audio directories. `name` is the client-visible root name. `server_path` is an absolute directory on the server. `webdav_url` is optional. |
| `auth_mode` | `external` (default), `off`, or `all`. Legacy `auth = true` means `all`. |
| `manage_mode` | Who may scan and remove records: `lan` (default), `all`, or `off`. |
| `token_env` | Environment variable for the bearer token. Default `AUDIOLIB_SERVER_TOKEN`. |
| `public_url` | Base URL reported by `/api/v1/health` for clients outside the LAN. Optional. |
| `ffmpeg_path` | Absolute path used only by `GET /audio/{id}/preview?codec=flac`. |

Create the database before the first start, for example `AudioLibCli db create /path/to/library.db`. On Windows, put paths in TOML single quotes so backslashes stay literal.

`auth_mode = "external"` accepts LAN-direct requests with no token. Requests that arrive through a reverse proxy, or from a non-private peer, need `Authorization: Bearer <token>`. `all` requires a token on every route except `/api/v1/health`. `off` requires none. The environment token is at least 32 random visible ASCII characters and is never written to the config file or to logs. The LAN admin page can also generate `al_` tokens; only their SHA-256 is stored in `tokens.json` beside the library.

`manage_mode` does not apply to reads or to Metadata writes. It gates scans, record removal and folder-record removal. `lan` allows those operations from a LAN-direct client. An external token receives 403 `MANAGE_FORBIDDEN`. Behind Docker port forwarding, NAT or a proxy that hides the client address, set `trust_lan_peers = false` and `auth_mode = "all"`. That also disables the passwordless `/admin` page; edit the TOML file for that deployment.

After the admin page saves roots, `admin-settings.json` beside the library replaces `[[roots]]` in the TOML file. Later edits belong in that file or in the admin/client root UI, which write the same store.

## Windows

Verify `AudioLibServer-<tag>-win-x64.zip` against its `.sha256` and extract it to a writable program directory. The ZIP does not register a Windows service. Start the process yourself:

```powershell
.\audiolib-server.exe --config $serverConfig
```

The recommended config and data directory is `%ProgramData%\AudioLibServer`, separate from the program directory. Create it explicitly and, on first setup, copy `server\audiolib-server\config.example.toml` there as `config.toml`. Keep that path and the existing `library_db` when upgrading. Admin-page updates need the program directory to be writable by the account that runs the server. The desktop updater does not update the server.

Install ffmpeg separately when FLAC previews are required, and set `ffmpeg_path` to the absolute path of `ffmpeg.exe`. The bundled default path is the macOS Homebrew location. Without ffmpeg, `preview?codec=flac` returns 503 `TRANSCODER_UNAVAILABLE`. Direct streams and in-process WAV PCM previews keep working.

## Linux (systemd)

Verify and extract `AudioLibServer-<tag>-linux-x64.tar.gz`. The example unit is [`server/deploy/systemd/audiolib-server.service`](../server/deploy/systemd/audiolib-server.service). It runs as the `audiolib` system user, loads `AUDIOLIB_SERVER_TOKEN` from `/etc/audiolib/audiolib.env`, and executes:

```text
/opt/audiolib/audiolib-server --config /etc/audiolib/config.toml
```

The unit allows writes under `/var/lib/audiolib` and read-only access to `/srv/audiolib/music`. A library or audio directory on another path needs a systemd override that adds that path to `ReadWritePaths` or `ReadOnlyPaths`. `library_db` must already exist and be writable by `audiolib`. Enable the unit with `systemctl enable --now audiolib-server`, then run the health check below. `Restart=on-failure` and `TimeoutStopSec=30` are part of the example unit.

Linux packages are built against Debian 12 as the GNU libc baseline.

## macOS (launchd)

Verify and extract `AudioLibServer-<tag>-macos-arm64.tar.gz`. The server listens on `bind` from the config file. Register a LaunchAgent from [`server/deploy/launchd/io.github.audiolib.server.plist.example`](../server/deploy/launchd/io.github.audiolib.server.plist.example):

- `EnvironmentVariables` sets `AUDIOLIB_LAUNCHD_LABEL` to the same string as `Label`.
- `KeepAlive` stays enabled so launchd starts the process again after an admin-page update. The updater exits with code 75 only when that label matches launchd's service name.
- The example argument list starts with `audiolib-launcher` and `/bin/sh run.sh`. `run.sh` is yours to create: it reads the token from a mode-600 file, exports `AUDIOLIB_SERVER_TOKEN`, and `exec`s `audiolib-server --config /path/to/config.toml`. Omit the launcher argument when you have not built one.

The release archive does not include the optional launcher. Building [`server/launcher`](../server/launcher/README.md) and granting removable-volume and network-volume access to that stable binary avoids repeating the permission prompt after each binary swap. Otherwise grant the same access to `audiolib-server` in System Settings → Privacy & Security.

Further install notes: [server/README.md](../server/README.md) and [server/deploy/README.md](../server/deploy/README.md).

## Docker

Load the verified offline image (`docker load`) and recreate the container. [`server/deploy/docker/docker-compose.release.yml`](../server/deploy/docker/docker-compose.release.yml) expects `AUDIOLIB_SERVER_TOKEN` and an image name in `AUDIOLIB_SERVER_IMAGE`. The data volume must already contain `/data/library.db`. The image ships `/config/config.toml` with:

- `auth_mode = "all"` and `trust_lan_peers = false`
- `manage_mode = "off"`
- `library_db = "/data/library.db"`
- one root named `Music` at `/music`
- `ffmpeg_path = "/usr/bin/ffmpeg"` when the image was built with ffmpeg

`manage_mode = "off"` refuses client scans and record removal. `trust_lan_peers = false` disables `/admin`, including on loopback. Mount your own config read-only over `/config/config.toml` when this deployment should use LAN management or different roots. Only `/data` is a volume (the library, snapshots, caches and `tokens.json`).

The compose file sets `AUDIOLIB_UPDATE_DISABLE=1`. Upgrade by loading a new image and recreating the container; keep the data volume. The image health check requests `/api/v1/health` and requires `"status":"ok"`. A library that is still opening, or that failed to open, stays unhealthy. Publish the HTTP port on the host loopback address and terminate TLS on a reverse proxy for internet clients. Do not publish `/admin`.

## Upgrade, backup and schema migration

Drain scans and requested Metadata writes, then stop the service (systemd stop, launchd `bootout`, or a normal Ctrl+C / SIGTERM). The server gives in-flight non-scan work a short drain and asks a running scan to record a cancelled session. Keep the config, `library_db` and its SQLite sidecars, `tokens.json`, `admin-settings.json`, snapshot and waveform directories, and the source audio.

Native Windows x64, macOS arm64 and Linux x64 packages can install a newer release from the admin page after the 0.9 layout is already in place. That layout places `audiolib-migrate` next to `audiolib-server`. A pre-0.9 updater rejects that extra binary, so the first move onto a 0.9 package is a manual extract while the old process is stopped. Docker upgrades replace the image.

v0.9.3 does not add a schema migration. Local writers stay on schema 8 (`ale-s8-cld1-t3-m2-p1-a1`). Schema 9 (`ale-s9-cld1-t3-m2-p1-a1-sd1`) is published only by an explicit server migration. Update clients that can read schemas 8 and 9, install the server, then migrate the real database:

```sh
audiolib-migrate status --db /path/to/library.db
audiolib-migrate start --db /path/to/library.db
```

On Windows the tool is `audiolib-migrate.exe`. `pause`, `resume`, `cancel` and `rollback` take the same `--db`. `startup` matches `start`, and with `AUDIOLIB_DEFER_SCHEMA9=1` it records schema 8 as deferred instead of migrating. The admin HTTP API accepts `start`, `pause`, `resume`, `cancel` and `rollback`; it rejects `startup`. Routes are `GET /admin/api/migration/status` and `POST /admin/api/migration/<action>`. They use the LAN admin guard and `manage_mode`.

Before writing, the migration checks that free space is at least 1.3 times the library size. It stores an online backup named `audiolib-rollback-mig9-*.db` beside the library, validates it, and leaves it in place. `rollback` restores derived schema-9 objects from that backup and keeps annotations written after the backup. Swapping in an older binary does not downgrade a schema 9 file; use the matching backup. The migration contract is [Schema 9 server migration](Rust_Schema9_Migration.md).

`server/deploy/redeploy-server.sh` is the generic binary swap with a backup, a health check and a rollback of the binaries when the library schema still allows it.

## Health check

Library routes live under `/api/v1/`. `GET /api/v1/health` needs no token and returns HTTP 200 while the process is serving. A request to bare `/health` is 404.

```sh
curl -fsS "http://<server-host>:<port>/api/v1/health"
```

`<server-host>` and `<port>` are the address in `bind`, or the reverse-proxy URL clients actually use.

| `status` | Meaning |
| --- | --- |
| `ok` | The library is open. `schema_version` is that library's schema. |
| `starting` | The port is up and the library is still opening. Authenticated routes return 503 `UNAVAILABLE` with `Retry-After: 5`. |
| `error` | Open failed. `message` is `The library could not be opened; the server keeps retrying`. The process retries in the background. |

The body also includes `version`, `api_version` (`v1`), `commit` (short git id, or `unknown`), `public_url` (`null` when unset) and `compatibility_catalog_revision`. An optional `upgrade` object appears during startup maintenance. While the library is not open yet, `schema_version` falls back to the build's ordinary schema (8) unless a migration report supplies another version.

## Connecting a client

In the desktop app open **Manage sound library** (Ctrl+D) and add a server profile.

- **Address:** `http://<server-host>:<port>` on a trusted LAN, or `https://audiolib.example.com` behind TLS. A path prefix is allowed. The client appends `/api/v1`.
- **Access token:** optional for a LAN-direct client when `auth_mode` is `external`. Required for internet access and for `auth_mode = "all"`. The Windows client stores it with DPAPI, per profile.
- Choose **Test connection**, save, then sync. Search runs on the downloaded snapshot. The client's local working library is a different database and is left unchanged.
- **Transcode auditions** uses `/audio/{id}/preview`. Leave it off on a LAN. FLAC previews need ffmpeg on the server.

Put internet deployments behind a TLS reverse proxy, require tokens, and do not publish `/admin`. Forwarding headers make those requests count as external. The proxy target is the server's `bind` address on the same machine, for example `http://127.0.0.1:<port>`.

## Automatic updates

The Windows desktop client and the native server check GitHub Releases in the background after a normal start. They are separate products: confirming an update in the desktop app does not replace the server, and the server admin page does not replace the desktop app. Stable and beta installs keep separate channels (`update-channel.txt` beside the program). Libraries, config and tokens stay outside the payload.

The macOS desktop client is installed and updated by hand. Native servers (Windows x64, macOS arm64, Linux x64) confirm the download on the admin page; the process drains work, then the helper replaces the binaries. Docker images set `AUDIOLIB_UPDATE_DISABLE=1` and are replaced as images. An already-installed build of the same version does not prompt again. `AUDIOLIB_UPDATE_DISABLE=1` turns the check off.

See [automatic updates](Rust_Auto_Update.md) and [Linux and Docker delivery](Rust_Linux_Docker_Release.md).
