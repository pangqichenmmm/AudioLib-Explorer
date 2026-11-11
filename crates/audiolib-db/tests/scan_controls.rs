// crates/audiolib-db/tests/scan_controls.rs
//! Cooperative controls keep the same session and lease while releasing database writers.
use audiolib_core::{DataProvider, Error, MetadataPatch, ScanProgress, ScanState, SearchRequest};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use std::{
    fs,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

fn wait_paused(receiver: &mpsc::Receiver<ScanProgress>, after: u64) -> ScanProgress {
    loop {
        let progress = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("worker progress");
        if progress.paused && progress.processed >= after {
            return progress;
        }
    }
}

#[test]
fn pause_resume_and_cancel_preserve_session_lease_annotations_and_sources() {
    for cancel_while_paused in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("audio");
        fs::create_dir(&root).unwrap();
        // Tiny valid WAV files keep the regression independent of real assets.
        let bytes = [
            b"RIFF".as_slice(),
            &38u32.to_le_bytes(),
            b"WAVEfmt ",
            &16u32.to_le_bytes(),
            &1u16.to_le_bytes(),
            &1u16.to_le_bytes(),
            &48000u32.to_le_bytes(),
            &96000u32.to_le_bytes(),
            &2u16.to_le_bytes(),
            &16u16.to_le_bytes(),
            b"data",
            &2u32.to_le_bytes(),
            &[0, 0],
        ]
        .concat();
        for index in 0..24 {
            fs::write(root.join(format!("{index}.wav")), &bytes).unwrap();
        }
        let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
        let control = ScanCancellation::default();
        let worker_control = control.clone();
        let worker_provider = provider.clone();
        let worker_root = root.clone();
        let (updates, receiver) = mpsc::channel();
        let (done, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut pauses = 0;
            let result = ScanService::new(worker_provider).scan(
                &worker_root,
                &ScanOptions {
                    incremental: false,
                    batch_size: 1,
                    ..Default::default()
                },
                &worker_control,
                |progress| {
                    if !progress.paused
                        && !progress.finalizing
                        && progress.processed >= (pauses + 1) * 2
                        && pauses < 2
                    {
                        assert!(worker_control.pause());
                        pauses += 1;
                    }
                    updates.send(progress).unwrap();
                },
            );
            done.send(result).unwrap();
        });
        let first = wait_paused(&receiver, 2);
        assert_eq!(first.processed, 2);
        let second = wait_paused(&receiver, 2);
        assert_eq!(
            first.processed, second.processed,
            "paused work must stop advancing"
        );
        assert!(completion.try_recv().is_err());
        assert_eq!(provider.stats().unwrap().audio_files, 2);
        // A paused scan retains process exclusion but permits ordinary annotations.
        assert!(matches!(
            ScanService::new(provider.clone()).scan(
                &root,
                &ScanOptions::default(),
                &ScanCancellation::default(),
                |_| {}
            ),
            Err(Error::Busy(_))
        ));
        let id = provider.search(&SearchRequest::default()).unwrap().results[0].id;
        provider
            .update_metadata(
                id,
                &MetadataPatch {
                    notes: Some("kept during pause".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        control.resume();
        let next = wait_paused(&receiver, 4);
        assert_eq!(next.session_id, first.session_id);
        assert_eq!(next.processed, 4);
        if cancel_while_paused {
            control.cancel();
        } else {
            control.resume();
        }
        let result = completion
            .recv_timeout(Duration::from_secs(10))
            .expect("paused worker must drain")
            .unwrap();
        worker.join().unwrap();
        assert_eq!(result.session_id, first.session_id);
        assert_eq!(
            result.state,
            if cancel_while_paused {
                ScanState::Cancelled
            } else {
                ScanState::Completed
            }
        );
        assert_eq!(result.processed, if cancel_while_paused { 4 } else { 24 });
        assert_eq!(provider.get_audio(id).unwrap().notes, "kept during pause");
        assert_eq!(provider.scan_history(10, 0).unwrap().len(), 1);
        assert_eq!(result.removed, 0);
        assert!(!control.pause(), "a terminal scan cannot be paused again");
        for index in 0..24 {
            assert_eq!(fs::read(root.join(format!("{index}.wav"))).unwrap(), bytes);
        }
        // Cancellation must release the lease so a subsequent scan can complete.
        assert_eq!(
            ScanService::new(provider.clone())
                .scan(
                    &root,
                    &ScanOptions::default(),
                    &ScanCancellation::default(),
                    |_| {}
                )
                .unwrap()
                .state,
            ScanState::Completed
        );
    }
}

#[test]
fn prepaused_scan_can_be_cancelled_without_resume_or_missing_reconciliation() {
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    let root = temp.path().join("empty");
    fs::create_dir(&root).unwrap();
    let control = ScanCancellation::default();
    assert!(control.pause());
    let worker_control = control.clone();
    let (updates, receiver) = mpsc::channel();
    let (done, completion) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        done.send(ScanService::new(provider).scan(
            &root,
            &ScanOptions {
                incremental: false,
                ..Default::default()
            },
            &worker_control,
            |progress| {
                updates.send(progress).unwrap();
            },
        ))
        .unwrap();
    });
    assert_eq!(wait_paused(&receiver, 0).processed, 0);
    control.cancel();
    control.resume(); // Continue cannot undo cancellation.
    let result = completion
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    worker.join().unwrap();
    assert_eq!(result.state, ScanState::Cancelled);
    assert_eq!(result.removed, 0);
}

fn tiny_wav() -> Vec<u8> {
    [
        b"RIFF".as_slice(),
        &38u32.to_le_bytes(),
        b"WAVEfmt ",
        &16u32.to_le_bytes(),
        &1u16.to_le_bytes(),
        &1u16.to_le_bytes(),
        &48000u32.to_le_bytes(),
        &96000u32.to_le_bytes(),
        &2u16.to_le_bytes(),
        &16u16.to_le_bytes(),
        b"data",
        &2u32.to_le_bytes(),
        &[0, 0],
    ]
    .concat()
}

fn one_sound_library() -> (tempfile::TempDir, std::path::PathBuf, Arc<SqliteProvider>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("one.wav"), tiny_wav()).unwrap();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    (temp, root, provider)
}

fn folder_total(provider: &SqliteProvider) -> u64 {
    provider
        .folders(None)
        .unwrap()
        .iter()
        .map(|folder| folder.file_count)
        .sum()
}

fn wait_for_cancelled_rollup(provider: &SqliteProvider) -> audiolib_core::ScanSession {
    let started = Instant::now();
    loop {
        let history = provider.scan_history(1, 0).unwrap();
        if let Some(session) = history.first()
            && session.state == ScanState::Cancelled
        {
            return session.clone();
        }
        if started.elapsed() > Duration::from_secs(10) {
            panic!("finish interrupt did not cancel the in-flight rollup: {history:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Close sets a finish interrupt before Metadata drain. When that drain fails, the
/// interrupt is cleared and the same scan must still publish folder statistics.
#[test]
fn retracted_finish_interrupt_still_rolls_up_folder_statistics() {
    let (_temp, root, provider) = one_sound_library();
    let control = ScanCancellation::default();
    // Set before enumeration ends, the same way a close request does.
    control.interrupt_finish();
    let worker_control = control.clone();
    let worker_provider = provider.clone();
    let worker_root = root.clone();
    let (done, completion) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        done.send(ScanService::new(worker_provider).scan(
            &worker_root,
            &ScanOptions {
                incremental: false,
                ..Default::default()
            },
            &worker_control,
            |_| {},
        ))
        .unwrap();
    });
    let cancelled = wait_for_cancelled_rollup(&provider);
    assert_eq!(
        folder_total(&provider),
        0,
        "an interrupted rollup must not publish folder statistics"
    );
    assert_eq!(provider.stats().unwrap().audio_files, 1);
    // Failed close retracts the finish interrupt and leaves the scan running.
    control.clear_finish_interrupt();
    let progress = completion
        .recv_timeout(Duration::from_secs(10))
        .expect("retracted finish interrupt must not stick")
        .expect("rollup after a retracted finish interrupt");
    worker.join().unwrap();
    assert!(!control.is_interrupted());
    assert!(!control.is_cancelled());
    assert_eq!(progress.session_id, cancelled.id);
    assert_eq!(progress.state, ScanState::Completed);
    assert_eq!(folder_total(&provider), 1);
    assert_eq!(
        provider.scan_history(1, 0).unwrap()[0].state,
        ScanState::Completed
    );
}

/// Quit upgrades the finish interrupt to a full interrupt. That must stay cancelled
/// and must not rerun the folder-statistics statement.
#[test]
fn committed_finish_interrupt_does_not_retry_rollup() {
    let (_temp, root, provider) = one_sound_library();
    let control = ScanCancellation::default();
    control.interrupt_finish();
    let worker_control = control.clone();
    let worker_provider = provider.clone();
    let worker_root = root.clone();
    let (done, completion) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        done.send(ScanService::new(worker_provider).scan(
            &worker_root,
            &ScanOptions {
                incremental: false,
                ..Default::default()
            },
            &worker_control,
            |_| {},
        ))
        .unwrap();
    });
    let cancelled = wait_for_cancelled_rollup(&provider);
    let started = Instant::now();
    control.interrupt();
    let error = completion
        .recv_timeout(Duration::from_secs(10))
        .expect("a committed finish interrupt must stop the scan")
        .expect_err("quit must not complete the interrupted rollup");
    worker.join().unwrap();
    assert!(matches!(error, Error::Cancelled), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(control.is_cancelled() && control.is_interrupted());
    assert_eq!(folder_total(&provider), 0);
    let session = &provider.scan_history(1, 0).unwrap()[0];
    assert_eq!(session.id, cancelled.id);
    assert_eq!(session.state, ScanState::Cancelled);
}
