// crates/audiolib-audio/src/playback_range_tests.rs
//! Selected-interval playback through the actual worker, decoder and bounded DSP.
use super::*;

fn range() -> AudioFrameRange {
    AudioFrameRange {
        start_frame: 200,
        end_frame: 600,
        sample_rate: 1000,
    }
}

fn loaded_worker(path: &std::path::Path) -> Worker {
    let output = tests::prepared_output(path);
    let mut worker = tests::worker(PlaybackStatus::default());
    worker.output = Some(output);
    worker
        .execute_command(AudioCommand::Load(path.to_owned()))
        .unwrap();
    worker
}

#[test]
fn range_transport_clamps_seeks_restarts_and_clears_without_starting() {
    let root = tempfile::tempdir().unwrap();
    let mut worker = loaded_worker(&root.path().join("range.wav"));
    let generation = worker.snapshot().generation;
    worker
        .execute_command(AudioCommand::PlayRange {
            range: range(),
            expected_generation: generation,
        })
        .unwrap();
    let playing = worker.snapshot();
    assert_eq!(playing.state, PlaybackState::Playing);
    assert_eq!(playing.position, 0.2);
    assert_eq!(playing.playback_range, Some(range()));
    assert_eq!(playing.play_session_id, 1);

    worker.execute_command(AudioCommand::Seek(5.0)).unwrap();
    assert_eq!(worker.snapshot().position, 0.6);
    worker.execute_command(AudioCommand::Pause).unwrap();
    assert_eq!(worker.snapshot().state, PlaybackState::Ended);
    worker.execute_command(AudioCommand::Play).unwrap();
    assert_eq!(worker.snapshot().position, 0.2);
    assert_eq!(worker.snapshot().play_session_id, 2);
    worker.execute_command(AudioCommand::Seek(-1.0)).unwrap();
    assert_eq!(worker.snapshot().position, 0.2);
    worker.execute_command(AudioCommand::Stop).unwrap();
    let stopped = worker.snapshot();
    assert_eq!(stopped.position, 0.2);
    assert_eq!(stopped.state, PlaybackState::Stopped);
    worker
        .execute_command(AudioCommand::SetAudition(AuditionSettings {
            pitch_semitones: 7.0,
            rate: 2.0,
            ..Default::default()
        }))
        .unwrap();
    assert_eq!(worker.snapshot().playback_range, Some(range()));
    worker.execute_command(AudioCommand::ClearRange).unwrap();
    let cleared = worker.snapshot();
    assert_eq!(cleared.position, 0.2);
    assert_eq!(cleared.state, PlaybackState::Stopped);
    assert_eq!(cleared.play_session_id, stopped.play_session_id);
    assert_eq!(cleared.playback_range, None);
}

#[test]
fn stale_or_unrepresentable_range_does_not_replace_current_transport() {
    let root = tempfile::tempdir().unwrap();
    let mut worker = loaded_worker(&root.path().join("guard.wav"));
    worker.execute_command(AudioCommand::Play).unwrap();
    let before = worker.snapshot();
    for (range, expected_generation) in [
        (range(), before.generation.wrapping_add(1)),
        (
            AudioFrameRange {
                start_frame: 200,
                end_frame: 2000,
                sample_rate: 1000,
            },
            before.generation,
        ),
        (
            AudioFrameRange {
                start_frame: 1,
                end_frame: 2,
                sample_rate: 192_000,
            },
            before.generation,
        ),
    ] {
        assert!(matches!(
            worker.execute_command(AudioCommand::PlayRange {
                range,
                expected_generation
            }),
            Err(Error::Validation(_))
        ));
        let after = worker.snapshot();
        assert_eq!(after.state, before.state);
        assert_eq!(after.position, before.position);
        assert_eq!(after.playback_range, None);
        assert_eq!(after.transport_revision, before.transport_revision);
    }
}

#[test]
fn shortened_source_cannot_silently_truncate_a_new_range() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("shortened.wav");
    let mut worker = loaded_worker(&path);
    let before = worker.snapshot();
    assert_eq!(before.duration, 1.0);
    // The loaded snapshot is still one second, but the actual material changed
    // before PlayRange reopens it. A partial range is never an accepted substitute.
    crate::tests::write_wav(&path, 1000, 1, 16, 1, &vec![0; 1000], false, false);
    assert!(matches!(
        worker.execute_command(AudioCommand::PlayRange {
            range: range(),
            expected_generation: before.generation,
        }),
        Err(Error::Validation(_))
    ));
    let after = worker.snapshot();
    assert_eq!(after.state, before.state);
    assert_eq!(after.position, before.position);
    assert_eq!(after.playback_range, None);
    assert_eq!(after.transport_revision, before.transport_revision);
    assert_eq!(after.play_session_id, before.play_session_id);
    assert!(worker.output.is_some());
}

#[test]
fn range_replacement_while_playing_resumes_new_decoder_and_load_clears_it() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("replace.wav");
    let mut worker = loaded_worker(&path);
    worker.execute_command(AudioCommand::Play).unwrap();
    let before = worker.snapshot();
    worker
        .execute_command(AudioCommand::PlayRange {
            range: range(),
            expected_generation: before.generation,
        })
        .unwrap();
    assert_eq!(worker.snapshot().state, PlaybackState::Playing);
    assert!(
        worker
            .output
            .as_ref()
            .unwrap()
            .realtime
            .running
            .load(Ordering::Acquire)
    );
    assert_eq!(worker.snapshot().play_session_id, before.play_session_id);
    worker.execute_command(AudioCommand::Load(path)).unwrap();
    assert_eq!(worker.snapshot().playback_range, None);
    assert_eq!(worker.snapshot().position, 0.0);
}

#[test]
fn representation_change_remaps_original_frames_without_changing_selection_or_session() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("original.wav");
    let mut worker = loaded_worker(&path);
    worker
        .execute_command(AudioCommand::PlayRange {
            range: range(),
            expected_generation: worker.snapshot().generation,
        })
        .unwrap();
    worker.execute_command(AudioCommand::Pause).unwrap();
    let before = worker.snapshot();
    let preview = root.path().join("preview.wav");
    crate::tests::write_wav(&preview, 500, 1, 16, 1, &vec![0; 1000], false, false);
    worker
        .execute_command(AudioCommand::ConfigureSource {
            expected_generation: before.generation,
            expected_path: path,
            path: preview.clone(),
            config: worker.config.clone(),
        })
        .unwrap();
    let after = worker.snapshot();
    assert_eq!(after.sample_rate, 500);
    assert_eq!(after.playback_range, Some(range()));
    assert_eq!(after.state, PlaybackState::Paused);
    assert_eq!(after.position, before.position);
    assert_eq!(after.generation, before.generation);
    assert_eq!(after.play_session_id, before.play_session_id);
    let mut decoder = prepare_audition_decoder(
        Decoder::open(&preview).unwrap(),
        1000,
        0.2,
        0,
        AuditionSettings::default(),
        after.playback_range,
    )
    .unwrap();
    let mut frames = 0;
    while !decoder.is_finished() {
        frames += decoder.next().unwrap().len() / 2;
    }
    assert_eq!(frames, 400);
}

#[test]
fn selected_silence_never_reads_outside_pulses_into_resampling_or_soundtouch() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("outside-pulses.wav");
    let mut bytes = Vec::new();
    for frame in 0..6000 {
        let value = if (1000..4000).contains(&frame) {
            0.0_f32
        } else {
            0.95_f32
        };
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&(-value).to_le_bytes());
    }
    crate::tests::write_wav(&path, 48_000, 2, 32, 3, &bytes, false, false);
    for output_rate in [44_100, 48_000, 96_000] {
        for (rate, pitch) in [(1.0, 0.0), (0.25, -12.0), (4.0, 12.0)] {
            let mut decoder = AuditionDecoder::new_bounded(
                Decoder::open(&path).unwrap(),
                output_rate,
                1000,
                4000,
                0,
                AuditionSettings {
                    rate,
                    pitch_semitones: pitch,
                    ..Default::default()
                },
            )
            .unwrap();
            let mut frames = 0;
            while !decoder.is_finished() {
                let output = decoder.next().unwrap();
                assert!(output.len() <= 4096);
                assert!(
                    output.iter().all(|value| value.abs() < 1e-7),
                    "outside audio at output {output_rate}, rate {rate}, pitch {pitch}"
                );
                frames += output.len() / 2;
            }
            assert_eq!(
                frames,
                (3000.0 * f64::from(output_rate) / 48_000.0 / f64::from(rate)).round() as usize
            );
        }
    }
}

#[test]
fn automatic_range_loop_retains_history_session_and_restarts_at_range_start() {
    let root = tempfile::tempdir().unwrap();
    let mut worker = loaded_worker(&root.path().join("loop.wav"));
    worker
        .execute_command(AudioCommand::PlayRange {
            range: range(),
            expected_generation: worker.snapshot().generation,
        })
        .unwrap();
    worker.config.audition.loop_enabled = true;
    let before = worker.snapshot();
    let output = worker.output.as_mut().unwrap();
    output.eof = true;
    output.realtime.ring.clear_suspended();
    output.start_position = 0.6;
    output.drain_deadline = Some(std::time::Instant::now());
    let status = worker.status.clone();
    let (commands, receiver) = mpsc::sync_channel(1);
    let thread = thread::spawn(move || worker.run(receiver));
    for _ in 0..2 {
        let (acknowledgment, received) = mpsc::sync_channel(1);
        commands
            .send(CommandRequest {
                command: AudioCommand::Synchronize,
                acknowledgment: Some(acknowledgment),
            })
            .unwrap();
        received
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
    }
    let after = status.lock().unwrap().clone();
    drop(commands);
    thread.join().unwrap();
    assert_eq!(after.state, PlaybackState::Playing);
    assert_eq!(after.position, 0.2);
    assert_eq!(after.playback_range, Some(range()));
    assert_eq!(after.play_session_id, before.play_session_id);
    assert_eq!(after.generation, before.generation);
}

#[test]
fn very_short_ranges_finish_with_exact_frames_without_outside_audio() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("very-short.wav");
    for length in [1_u64, 2, 7, 31, 127, 1023] {
        let mut bytes = Vec::new();
        for frame in 0..4096 {
            let value = if (128..128 + length).contains(&frame) {
                0.0_f32
            } else {
                0.95_f32
            };
            bytes.extend_from_slice(&value.to_le_bytes());
            bytes.extend_from_slice(&(-value).to_le_bytes());
        }
        crate::tests::write_wav(&path, 48_000, 2, 32, 3, &bytes, false, false);
        for output_rate in [44_100, 48_000, 96_000] {
            for (rate, pitch_semitones) in [(1.0, 0.0), (0.25, -24.0), (4.0, 24.0)] {
                let mut decoder = AuditionDecoder::new_bounded(
                    Decoder::open(&path).unwrap(),
                    output_rate,
                    128,
                    128 + length,
                    0,
                    AuditionSettings {
                        rate,
                        pitch_semitones,
                        ..Default::default()
                    },
                )
                .unwrap();
                let expected = (length as f64 * f64::from(output_rate) / 48_000.0 / f64::from(rate))
                    .round() as u64;
                let mut frames = 0;
                let mut calls = 0;
                while !decoder.is_finished() {
                    calls += 1;
                    assert!(calls <= expected.div_ceil(2048) + 2);
                    let block = decoder.next().unwrap();
                    assert!(!block.is_empty());
                    assert!(block.len().is_multiple_of(2) && block.len() <= 4096);
                    assert!(
                        block
                            .iter()
                            .all(|sample| sample.is_finite() && sample.abs() < 1e-7),
                        "Outside pulse in {length}-frame range at output {output_rate}, rate {rate}, pitch {pitch_semitones}"
                    );
                    frames += (block.len() / 2) as u64;
                }
                assert_eq!(frames, expected);
                assert!(decoder.next().unwrap().is_empty());
            }
        }
    }
}

fn rapid_range_worker(path: &std::path::Path) -> Worker {
    let mut output = tests::prepared_output(path);
    let mut bytes = Vec::new();
    for frame in 0..12_000 {
        let value = (std::f32::consts::TAU * 220.0 * frame as f32 / 48_000.0).sin() * 0.2;
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&(-value).to_le_bytes());
    }
    crate::tests::write_wav(path, 48_000, 2, 32, 3, &bytes, false, false);
    output.sample_rate = 48_000;
    Arc::get_mut(&mut output.realtime).unwrap().ring = Ring::new(4096);
    let mut worker = tests::worker(PlaybackStatus::default());
    worker.output = Some(output);
    worker
        .execute_command(AudioCommand::Load(path.to_owned()))
        .unwrap();
    worker.output.as_mut().unwrap().callback_available = true;
    worker
}

struct RangeCallback {
    stop: Arc<AtomicBool>,
    invalid: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RangeCallback {
    fn new(realtime: Arc<Realtime>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let invalid = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_invalid = invalid.clone();
        let thread = thread::spawn(move || {
            let mut callback = RenderCallback::new(realtime, 2, 0, 48_000);
            let mut block = [0.0_f32; 256];
            while !thread_stop.load(Ordering::Acquire) {
                callback.render(&mut block);
                if block
                    .iter()
                    .any(|sample| !sample.is_finite() || sample.abs() > 1.0)
                {
                    thread_invalid.store(true, Ordering::Release);
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            stop,
            invalid,
            thread: Some(thread),
        }
    }

    fn finish(mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
        assert!(!self.invalid.load(Ordering::Acquire));
    }
}

impl Drop for RangeCallback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn rapid_range_stop_seek_clear_and_load_keep_callback_and_transport_valid() {
    let root = tempfile::tempdir().unwrap();
    for (index, audition) in [
        AuditionSettings::default(),
        AuditionSettings {
            pitch_semitones: 7.0,
            rate: 1.5,
            ..Default::default()
        },
    ]
    .into_iter()
    .enumerate()
    {
        let path = root.path().join(format!("rapid-{index}.wav"));
        let mut worker = rapid_range_worker(&path);
        let callback = RangeCallback::new(worker.output.as_ref().unwrap().realtime.clone());
        worker
            .execute_command(AudioCommand::SetAudition(audition))
            .unwrap();
        for iteration in 0..96 {
            let length = [1, 2, 31, 127, 1023][iteration % 5];
            let start_frame = 128 + (iteration as u64 % 4) * 2048;
            let selected = AudioFrameRange {
                start_frame,
                end_frame: start_frame + length,
                sample_rate: 48_000,
            };
            let generation = worker.snapshot().generation;
            worker
                .execute_command(AudioCommand::PlayRange {
                    range: selected,
                    expected_generation: generation,
                })
                .unwrap();
            assert_eq!(worker.snapshot().state, PlaybackState::Playing);
            assert_eq!(worker.snapshot().playback_range, Some(selected));
            if iteration % 3 == 0 {
                worker
                    .execute_command(AudioCommand::Seek(selected.end_seconds()))
                    .unwrap();
                worker.execute_command(AudioCommand::Play).unwrap();
                assert_eq!(worker.snapshot().position, selected.start_seconds());
            }
            if iteration % 4 == 0 {
                worker.execute_command(AudioCommand::ClearRange).unwrap();
                assert_eq!(worker.snapshot().playback_range, None);
                worker
                    .execute_command(AudioCommand::PlayRange {
                        range: selected,
                        expected_generation: generation,
                    })
                    .unwrap();
            }
            worker.execute_command(AudioCommand::Stop).unwrap();
            let stopped = worker.snapshot();
            assert_eq!(stopped.state, PlaybackState::Stopped);
            assert_eq!(stopped.position, selected.start_seconds());
            assert_eq!(stopped.playback_range, Some(selected));
            assert!(
                !worker
                    .output
                    .as_ref()
                    .unwrap()
                    .realtime
                    .running
                    .load(Ordering::Acquire)
            );
            if iteration % 8 == 0 {
                worker
                    .execute_command(AudioCommand::Load(path.clone()))
                    .unwrap();
                assert_eq!(worker.snapshot().generation, generation + 1);
                assert_eq!(worker.snapshot().playback_range, None);
            }
        }
        callback.finish();
    }
}

#[test]
fn tiny_range_repeated_automatic_loops_stop_without_restarting_or_new_sessions() {
    let root = tempfile::tempdir().unwrap();
    for (index, audition) in [
        AuditionSettings {
            loop_enabled: true,
            ..Default::default()
        },
        AuditionSettings {
            loop_enabled: true,
            pitch_semitones: 7.0,
            rate: 1.5,
            ..Default::default()
        },
    ]
    .into_iter()
    .enumerate()
    {
        let mut worker = rapid_range_worker(&root.path().join(format!("tiny-loop-{index}.wav")));
        let callback = RangeCallback::new(worker.output.as_ref().unwrap().realtime.clone());
        worker
            .execute_command(AudioCommand::SetAudition(audition))
            .unwrap();
        let selected = AudioFrameRange {
            start_frame: 256,
            end_frame: 287,
            sample_rate: 48_000,
        };
        worker
            .execute_command(AudioCommand::PlayRange {
                range: selected,
                expected_generation: worker.snapshot().generation,
            })
            .unwrap();
        let before = worker.snapshot();
        let status = worker.status.clone();
        let (commands, receiver) = mpsc::sync_channel(4);
        let worker_thread = thread::spawn(move || worker.run(receiver));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let looped = loop {
            let current = status.lock().unwrap().clone();
            if current.transport_revision >= before.transport_revision + 8
                || current.state != PlaybackState::Playing
                || std::time::Instant::now() >= deadline
            {
                break current;
            }
            thread::sleep(Duration::from_millis(2));
        };
        let (acknowledgment, received) = mpsc::sync_channel(1);
        commands
            .send(CommandRequest {
                command: AudioCommand::Stop,
                acknowledgment: Some(acknowledgment),
            })
            .unwrap();
        let stopped = received
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        thread::sleep(Duration::from_millis(20));
        let settled = status.lock().unwrap().clone();
        drop(commands);
        worker_thread.join().unwrap();
        callback.finish();
        assert_eq!(looped.state, PlaybackState::Playing);
        assert!(looped.transport_revision >= before.transport_revision + 8);
        assert_eq!(looped.generation, before.generation);
        assert_eq!(looped.play_session_id, before.play_session_id);
        assert_eq!(stopped.state, PlaybackState::Stopped);
        assert_eq!(stopped.position, selected.start_seconds());
        assert_eq!(settled.state, PlaybackState::Stopped);
        assert_eq!(settled.transport_revision, stopped.transport_revision);
        assert_eq!(settled.play_session_id, stopped.play_session_id);
    }
}
