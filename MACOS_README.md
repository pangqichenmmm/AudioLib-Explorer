# AudioLib Explorer for macOS

This desktop package requires Apple Silicon (arm64) and macOS 11 or newer.
Verify the release ZIP's SHA256 before extracting it, then move
`AudioLib Explorer.app` into Applications. The command-line tool is
`AudioLib Explorer.app/Contents/MacOS/AudioLibCli`.

Playback uses CoreAudio. ASIO is unavailable on macOS; imported ASIO preferences
fall back to the default CoreAudio output. No microphone permission is needed.

The initial macOS client is ad-hoc signed unless its release notes explicitly
state a Developer ID identity. It is not notarized. Follow macOS's own first-open
confirmation for software you trust. This package installs no privileged helper.
Desktop updates are manual: quit the application, verify the new ZIP, and replace
the application bundle. The client does not install desktop updates automatically.

Libraries, settings, logs and caches are separate from the application, under
`~/Library/Application Support/AudioLibExplorer/v1`. An explicit
`AUDIOLIB_DATA_DIR` and configured library paths take precedence. Replacing the
application does not replace that data. Never place your library inside the app
bundle. For an earlier unpublished development build that used a directory beside
its executable, retain that directory using `AUDIOLIB_DATA_DIR` or use the supported
backup/import workflow. There is no automatic migration or removal of that data.

A beta or release-candidate bundle uses the separate sibling directory
`~/Library/Application Support/AudioLibExplorer/beta`, based on the validated
version/channel markers in `AudioLib Explorer.app/Contents/Resources`.
`AUDIOLIB_BETA_DATA_DIR` overrides
that beta location; `AUDIOLIB_DATA_DIR` takes priority over both. Stable bundles
ignore the beta override. Invalid or incomplete package markers stop startup
before selecting a data root. These markers are sealed by the application code
signature; keep the complete application bundle together.

Server mode uses a validated local snapshot as described in
[Rust_Remote_Mode.md](doc/Rust_Remote_Mode.md). Use the application's folder picker
and macOS privacy settings when granting access to removable or network volumes.
No working libraries, connection addresses, credentials or audio files are shipped.

The native release validator checks the package boundary, code signatures, binary
identity, isolated first launch, a synthetic WAV import, native application Quit
with a pending Metadata draft, Chinese UI startup and default data placement with
a temporary home and read-only app/current directory. The Quit regression sends
the application's own AppKit termination request, then verifies the saved note
through the packaged CLI and checks that the synthetic source WAV is unchanged.
These checks do not establish acoustic performance or real-server beta acceptance.

## Native release verification

Maintainers build and package from a clean checkout at the final release commit,
using native arm64 PowerShell 7.3+, the pinned Rust toolchain and the installed
Xcode command-line tools. No ASIO SDK or Python runtime is used.

```powershell
$taskReleaseCommit = (git rev-parse HEAD).Trim()
$taskBuildProof = Join-Path (Get-Location).Path ('.rust-validation/macos-client-build/' + [guid]::NewGuid().ToString('N') + '/build.json')
pwsh ./tools/build-macos-client.ps1 -ExpectedCommit $taskReleaseCommit -ReportPath $taskBuildProof
pwsh ./tools/package-macos-client.ps1 -Tag v0.8.5 -ExpectedCommit $taskReleaseCommit -BuildReport $taskBuildProof -ValidateRuntime
```

The packager prints the retained application bundle path and produces
`dist/latest/AudioLibExplorer-v0.8.5-macos-arm64.zip`, its `.sha256`, and a local
`AudioLibExplorer-v0.8.5-macos-arm64-report.json`. Substitute the actual tag for a
later release or beta candidate. With `-ValidateRuntime`, packaging runs structural
and native acceptance together once, before publishing the output files. Keep its
private `validation.json` evidence; the package inventory report itself still
records `runtime_validation: not-run`. Do not repeat acceptance for unchanged
artifacts. If no logged-in desktop session is available, omit `-ValidateRuntime`
and complete the deferred native acceptance later with those paths:

```powershell
pwsh ./tools/validate-macos-client.ps1 -Tag v0.8.5 -ExpectedCommit $taskReleaseCommit -BundlePath '<retained app path>' -ArchivePath ./dist/latest/AudioLibExplorer-v0.8.5-macos-arm64.zip -PackageReport ./dist/latest/AudioLibExplorer-v0.8.5-macos-arm64-report.json -BuildReport $taskBuildProof
```

The build proof contains local paths and stays private. Upload only the ZIP and
its checksum for this client. The local package report also remains outside the
archive; the validator uses it to check every signed file and archive entry.
`-StructureOnly` explicitly records runtime validation as `not-run` and cannot
replace full acceptance. `-TestBoundaries` runs synthetic ZIP boundary regressions
on Windows or Mac without executing native application binaries.

Full acceptance requires a logged-in Mac desktop session for UI startup. It keeps
the owned temporary home, synthetic audio, screenshots and diagnostics as proof,
prints their path, and never terminates an unrelated process. If a smoke process
times out, its owned PID is recorded and the run fails without terminating it.
The native Quit fixture uses `--smoke-ui --smoke-native-quit`, an explicit owned
data directory and a validated `.native-quit-fixture.json` marker beside its
single-record `library.db`. It cannot target a normal working library.
The default signature is ad-hoc; `-SigningIdentity '<identity>'` selects an
existing signing identity when packaging. Signing alone does not notarize the app.
