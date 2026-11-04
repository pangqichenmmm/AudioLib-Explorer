# 0.8 server playback investigation — 2026-10-27

This records the investigation and observed recovery before the later authorized
production deployment. The investigation itself queried production read-only;
it did not restart, upgrade, reconfigure, scan or write the service. See the
[latest validation report](../doc/Rust_Validation.md) for the subsequent Mac
deployment and real Windows artwork/preview acceptance.

## Confirmed observations

- The existing server reports `status=ok`, version `0.7.8`, schema `6`, commit
  `5256fa4`. Its library declares the supported
  `ale-s6-cld1-t3-m2-p1` snapshot format.
- The real cached schema-6 snapshot has the required current search markers and
  indexes. The Rust `existing_v6_mirror_opens_in_place_and_reads_audio` probe
  confirms that `has_current_search_state` is true, `open_read_only` retains the
  original cache path, and the selected record and its derived album are readable.
  Source length and modification time remain unchanged. The suspected full-cache
  copy/schema upgrade is therefore excluded for this cache.
- `GET /audio/{id}` returns present audio Metadata promptly for the sampled records.
  A direct `GET /audio/{id}/stream` with `Range: bytes=0-4095` did not return response
  headers within a bounded 20-second observation for the earlier selected record.
  Its root reports `waking`.
- Two records from other roots were then sampled directly, independently of the
  desktop. Both original `/stream` and `/preview?codec=pcm&sr=44100&bits=16`
  requests did not return response headers within each 12-second observation.
  One source root had reported `online` before the requests and subsequently
  appeared as `waking`. The stream concurrency limit was not exhausted.
- Existing logs already show repeated server Waveform timeouts for the earlier
  selected record while running 0.7.8. They contain no evidence that 0.8 itself
  caused that source-file access failure.

The short HTTP observations end before the client's normal first-byte timeout.
They show waiting independently of the 0.8 desktop, but cannot by themselves
distinguish an ordinary NAS wake-up from a playback failure.

## Full first-byte budget check

On 2026-10-27 (Asia/Shanghai), exactly one additional request was made for the
sample whose root had initially reported `online`: the original `/stream`, with
`Range: bytes=0-4095` and a **110-second response-header budget**. The request
ended with a local transport error after **110.036 seconds**. **No response
headers or body bytes were received**, so there is no HTTP status, audio magic,
or decoder result for this request. It was not retried.

This observation exceeds the desktop client's normal 100-second first-byte wait
and confirms that this sample did not respond within that playback budget even
without the desktop. It does not identify the cause: source availability,
server-side file access, request/response routing, or macOS volume permissions
needed further evidence at that stage. An `online` root directory alone does not
prove the server can open and read a child WAV. The later SSH investigation below
supersedes any claim that the production request is still continuously blocked.

## Mac investigation and observed recovery

After the user supplied an existing SSH login, read-only inspection reached the
same running production service. No service, launch agent, mount, configuration,
database or source WAV was changed by this investigation.

- Production logs show the sampled audio IDs inside `Stream file open was slow`
  events. Many queued file opens completed together at approximately **19:05:37–47
  Asia/Shanghai**, with `ok=true`; recorded waits ranged from thousands to about
  32,000 seconds. This establishes a real server-side source-open stall, rather
  than a schema-copy delay inside the desktop. The process IDs did not change.
- The source SMB mount is currently mounted. Both previously failing source
  samples can now be opened and read directly over SSH: 4 KiB RIFF headers in
  **0.048 s** and **0.303 s**, respectively. These are warm observations, not
  physical audio-output latency measurements.
- On the Mac, the first sample now returns `/stream` Range `0-4095` as **206**,
  `audio/wav`, 4096 RIFF bytes in **0.280 s**, and PCM `/preview` as **206** with
  RIFF bytes in **0.019 s**. A sample under the other previously failing root
  returns its stream as **206** in **0.014 s**. All 38 roots currently report online.
- Windows rechecked the original server address at **19:12:58 Asia/Shanghai**:
  the same original stream returned **206**, `audio/wav`, 4096 RIFF bytes, first
  headers **53 ms**, total **67 ms**, and a valid `Content-Range`. PCM preview
  returned **206** with RIFF bytes, first headers **2 ms**, total **3 ms**.
  Each route was requested once with a 15-second budget.

macOS TCC logs attribute the source-volume requests to the production launcher
and server. Observed responses include `authValue=2`, `authReason=2`, no error.
The user subsequently confirmed that they **allowed file or network-volume
access at approximately 19:05 Asia/Shanghai**. This trusted user evidence aligns
with the allowed TCC responses, the queued source opens completing together and
the subsequent successful Mac/Windows requests. The incident is therefore
attributed to waiting for source-volume/file-access authorization, rather than
the suspected client snapshot upgrade. This does not mean that client code
permanently repaired macOS permission handling.

Keep the historical 110.036-second failure, the confirmed authorization action
and the successful probes together. Playback access has recovered for these
samples without restarting or redeploying production. Future server deployment
must preserve the existing launcher that holds the macOS access authorization;
replacing it or changing its access context may require renewed permission.

## Regression coverage

The existing isolated desktop server-mode regression now also sends the virtual
HTTP stream registration through the actual audio engine `Load` command, verifies
`Ready` and the record's layout/duration, seeks, then stops. No output device or
audible playback is needed for this boundary check.

```powershell
cargo test -p audiolib-desktop --locked server_mode_writes_annotations_to_the_server_and_switches_back_to_local
```

The regression is synthetic and cannot replace production source-volume access
or physical output acceptance. Do not claim production playback is fixed solely
because it passes. No production address, token, file path, filename or image is
included in this report.
