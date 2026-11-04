// app/audiolib-desktop/src/list_waveforms_benchmark.rs
//! Opt-in production-worker benchmark with owned, deterministic PCM fixtures.
//! `cold` means an empty thumbnail cache; the OS file cache is not flushed.
use super::*;
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use std::{fs, path::Path, thread};

struct BenchmarkHost {
    provider: Arc<SqliteProvider>,
}

impl ListWaveformHost for BenchmarkHost {
    fn list_waveform_provider(&self) -> Option<Arc<dyn DataProvider>> {
        Some(self.provider.clone())
    }

    fn list_waveform_scanning(&self) -> bool {
        false
    }

    fn deliver_list_waveforms(&self, _: u64, _: Vec<(i64, ListWaveformCell)>) {}

    fn list_waveform_source_changed(&self, _: ListWaveformSource) {}
}

fn write_fixture(path: &Path, index: usize) -> (ListWaveform, u64, u64) {
    // Cover short effects, medium recordings and 16-second multichannel material.
    let frames = [12_000_u32, 48_000, 192_000, 768_000][index % 4];
    let channels = [1_u16, 2, 4][index % 3];
    let bits = [16_u16, 24][index % 2];
    let bytes_per_frame = channels * (bits / 8);
    let pcm_bytes = frames * u32::from(bytes_per_frame);
    let mut bytes = Vec::with_capacity(pcm_bytes as usize + 44);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(pcm_bytes + 36).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.extend_from_slice(&(48_000 * u32::from(bytes_per_frame)).to_le_bytes());
    bytes.extend_from_slice(&bytes_per_frame.to_le_bytes());
    bytes.extend_from_slice(&bits.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&pcm_bytes.to_le_bytes());
    let samples_per_pixel = frames.div_ceil(LIST_WAVEFORM_COLUMNS);
    let columns = frames.div_ceil(samples_per_pixel) as usize;
    let mut lows = vec![f32::INFINITY; columns];
    let mut highs = vec![f32::NEG_INFINITY; columns];
    for frame in 0..frames {
        let column = (frame / samples_per_pixel) as usize;
        for channel in 0..channels {
            let sample = ((u64::from(frame) * 37 + u64::from(channel) * 143 + index as u64 * 997)
                % 60_001) as i32
                - 30_000;
            let sample = sample << (bits - 16);
            bytes.extend_from_slice(&sample.to_le_bytes()[..usize::from(bits / 8)]);
            let value = sample as f32 / (1_u32 << (bits - 1)) as f32;
            lows[column] = lows[column].min(value);
            highs[column] = highs[column].max(value);
        }
    }
    fs::write(path, bytes).unwrap();
    // Independent fixture oracle: union every source sample into its output column.
    let values = lows
        .into_iter()
        .zip(highs)
        .flat_map(|(low, high)| {
            [
                (low.clamp(-1.0, 1.0) * 32767.0).round() as i16,
                (high.clamp(-1.0, 1.0) * 32767.0).round() as i16,
            ]
        })
        .collect();
    (
        ListWaveform::new(samples_per_pixel, values).unwrap(),
        u64::from(pcm_bytes + 44),
        u64::from(frames),
    )
}

fn await_pass(service: &ListWaveforms, timeout: Duration) -> ListWaveformProgress {
    let deadline = Instant::now() + timeout;
    loop {
        let progress = service.progress();
        if matches!(
            progress.state,
            ListWaveformProgressState::Complete | ListWaveformProgressState::Error
        ) {
            return progress;
        }
        assert!(
            Instant::now() < deadline,
            "Waveform pass timed out: {progress:?}"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "release-only production-worker benchmark; use tools/benchmark-local-waveforms.ps1"]
#[expect(
    clippy::assertions_on_constants,
    reason = "runtime guard for an opt-in release benchmark"
)]
fn local_waveform_pregeneration_benchmark() {
    // Compile in debug for all-target Clippy, but reject accidental debug timing.
    assert!(!cfg!(debug_assertions), "run this benchmark with --release");
    let count = std::env::var("AUDIOLIB_WAVEFORM_BENCH_FILES")
        .ok()
        .map(|value| value.parse::<usize>().expect("file count is an integer"))
        .unwrap_or(120);
    assert!((12..=1200).contains(&count));
    let label = std::env::var("AUDIOLIB_WAVEFORM_BENCH_LABEL").unwrap_or_else(|_| "current".into());
    let fixture_parent =
        std::env::var_os("AUDIOLIB_WAVEFORM_BENCH_PARENT").filter(|parent| !parent.is_empty());
    let mut builder = tempfile::Builder::new();
    builder.prefix("audiolib-waveform-benchmark-");
    let source_temp = match &fixture_parent {
        Some(parent) => {
            assert!(
                Path::new(parent).is_dir(),
                "fixture parent must already exist"
            );
            builder.tempdir_in(parent).unwrap()
        }
        None => builder.tempdir().unwrap(),
    };
    // A desktop library stays on the local machine even when its WAV roots are on
    // a NAS. Only the owned audio fixture directory follows the optional parent.
    let database_temp = tempfile::Builder::new()
        .prefix("audiolib-waveform-benchmark-db-")
        .tempdir()
        .unwrap();
    let source_parent = source_temp.path().parent().unwrap().canonicalize().unwrap();
    let database_parent = database_temp
        .path()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let root = source_temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let mut fixtures = Vec::with_capacity(count);
    let (mut bytes, mut frames) = (0_u64, 0_u64);
    for index in 0..count {
        let path = root.join(format!("fixture-{index:04}.wav"));
        let (expected, file_bytes, file_frames) = write_fixture(&path, index);
        bytes += file_bytes;
        frames += file_frames;
        fixtures.push((path.canonicalize().unwrap(), expected));
    }
    let provider =
        Arc::new(SqliteProvider::open(&database_temp.path().join("library.db")).unwrap());
    ScanService::new(provider.clone())
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let host: Arc<dyn ListWaveformHost> = Arc::new(BenchmarkHost {
        provider: provider.clone(),
    });
    let service = ListWaveforms::new(true);
    let started = Instant::now();
    service.start(Arc::downgrade(&host));
    let cold = await_pass(&service, Duration::from_secs(300));
    let cold_seconds = started.elapsed().as_secs_f64();
    assert_eq!(cold.state, ListWaveformProgressState::Complete, "{cold:?}");
    assert_eq!(cold.generated, count as u64);
    assert_eq!(cold.processed, count as u64);
    assert_eq!(cold.failed, 0);
    assert!(provider.list_waveform_backlog(0, count).unwrap().is_empty());

    for (path, expected) in &fixtures {
        let id = provider.find_audio_by_path(path).unwrap().unwrap().id;
        let batch = provider
            .list_waveforms(&[id], Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert_eq!(
            batch.items[0].state,
            ListWaveformState::Ready {
                waveform: expected.clone()
            },
            "incorrect peaks for {}",
            path.file_name().unwrap().to_string_lossy()
        );
    }

    let started = Instant::now();
    service.scan_finished();
    let warm = await_pass(&service, Duration::from_secs(30));
    let warm_seconds = started.elapsed().as_secs_f64();
    assert_eq!(warm.state, ListWaveformProgressState::Complete, "{warm:?}");
    assert_eq!(warm.processed, 0, "a warm pass must not decode any WAV");
    service.shutdown();
    assert!(service.wait_idle(Duration::from_secs(10)));
    drop(host);

    let report = serde_json::json!({
        "label": label,
        "workload_version": 1,
        "files": count,
        "source_bytes": bytes,
        "source_frames": frames,
        "sample_rate": 48000,
        "pcm_bits": [16, 24],
        "channels": [1, 2, 4],
        "durations_seconds": [0.25, 1.0, 4.0, 16.0],
        "thumbnail_cache_before": "empty",
        "fixture_location": if fixture_parent.is_some() { "explicit_parent" } else { "system_temp" },
        "source_parent": source_parent,
        "database_parent": database_parent,
        "database_location": "system_temp",
        "os_file_cache": "not_flushed",
        "worker": "production ListWaveforms::start",
        "cold_seconds": cold_seconds,
        "cold_files_per_second": count as f64 / cold_seconds,
        "cold_source_mib_per_second": bytes as f64 / 1048576.0 / cold_seconds,
        "warm_seconds": warm_seconds,
        "warm_files_decoded": warm.processed,
        "generated": cold.generated,
        "failed": cold.failed,
        "waveforms_matching_independent_oracle": count,
        "available_parallelism": thread::available_parallelism().map_or(1, usize::from),
        "architecture": std::env::consts::ARCH,
        "os": std::env::consts::OS,
    });
    println!("WAVEFORM_BENCHMARK {report}");
    if let Some(output) = std::env::var_os("AUDIOLIB_WAVEFORM_BENCH_REPORT") {
        fs::write(output, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}
