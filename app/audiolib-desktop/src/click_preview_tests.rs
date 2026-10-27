// app/audiolib-desktop/src/click_preview_tests.rs
//! Exercise both click-preview modes through the real Controller, event loop and muted output.
use super::*;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::{PointerEventButton, WindowEvent};
use std::cell::RefCell;
use std::rc::Rc;

const ROOT_ENV: &str = "AUDIOLIB_CLICK_PREVIEW_TEST_ROOT";
const WAV_NAME: &str = "Click preview silent.wav";

#[test]
#[ignore = "requires muted Windows output"]
fn real_pointer_preview_modes_play_after_checkbox_changes() {
    let root = std::env::temp_dir().join(format!(
        "audiolib-click-preview-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "controller::click_preview_tests::child_real_pointer_preview_modes",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(ROOT_ENV, &root)
        .env("AUDIOLIB_DATA_DIR", root.join("preferences"))
        .output()
        .unwrap();
    // Only this test's subtree is removed, after the isolated child released every handle.
    assert!(
        owned_test_root(&root),
        "Refusing cleanup outside the owned test root"
    );
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stdout}");
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "Real pointer preview regression failed: {}\n{stdout}\n{stderr}",
        output.status
    );
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Search,
    InitialSecondClick,
    InitialPlaying,
    EnableAfterStop,
    SinglePlaying,
    DisableAfterStop,
    FinalRefresh,
    FinalSecondClick,
    FinalPlaying,
    Complete,
}

struct Progress {
    phase: Phase,
    stage_started: Instant,
    first_release: Option<Instant>,
    first_position: Option<slint::LogicalPosition>,
    first_model: Option<ModelRc<AudioRow>>,
    refresh_generation: Option<u64>,
    refresh_row_id: Option<String>,
    last_observation: String,
    failure: Option<String>,
}

#[test]
#[ignore = "requires muted Windows output; invoked by the isolated parent regression"]
fn child_real_pointer_preview_modes() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("Parent test must supply its root"));
    assert!(
        owned_test_root(&root),
        "Test root must remain inside the owned temporary subtree"
    );
    let sounds = root.join("sounds");
    std::fs::create_dir_all(&sounds).unwrap();
    let source = sounds.join(WAV_NAME);
    write_silent_wav(&source);
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let preferences = root.join("preferences");
    std::fs::create_dir_all(&preferences).unwrap();
    // Construct explicit owned paths instead of invoking legacy-settings discovery.
    let config = AppConfig {
        paths: audiolib_core::config::AppPaths {
            database: preferences.join("library.db"),
            cache: preferences.join("cache"),
            logs: preferences.join("logs"),
            settings: preferences.join("settings.json"),
            legacy_database: None,
            audio_terms: None,
        },
        language: "en".into(),
        playback: serde_json::to_value(&playback).unwrap(),
        audition_play_on_single_click: false,
        play_history_limit: audiolib_core::config::PLAY_HISTORY_LIMIT_PER_LIBRARY,
        pregenerate_list_waveforms: false,
        hold_list_waveform_batches_after_audition: false,
        max_search_tasks: 6,
        ui_scale_percent: 0,
        theme: audiolib_core::config::DEFAULT_THEME.into(),
        ui_font_size: audiolib_core::config::DEFAULT_FONT_SIZE,
        list_font_size: audiolib_core::config::DEFAULT_FONT_SIZE,
        audio_table: Default::default(),
        spectral_peaks: Default::default(),
        window_layout: Default::default(),
        allowed_scan_roots: vec![sounds.canonicalize().unwrap()],
        data_source: Default::default(),
        data_source_fallback: None,
    };
    config.save().unwrap();
    let provider = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
    ScanService::new(provider.clone())
        .scan(
            &sounds,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let records = provider.search(&SearchRequest::default()).unwrap().results;
    assert_eq!(records.len(), 1);
    let target = PathBuf::from(&records[0].file_path);
    assert_eq!(
        target.canonicalize().unwrap(),
        source.canonicalize().unwrap()
    );
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
    let trace = Rc::new(RefCell::new(Vec::<String>::new()));
    let observed = trace.clone();
    let c = controller.clone();
    ui.on_row_clicked(move |index, toggle, range, play| {
        observed.borrow_mut().push(format!(
            "clicked index={index} toggle={toggle} range={range} play={play}"
        ));
        // The spy preserves the production callback, including checked-row publication.
        c.select_row(index, toggle, range, play);
        observed.borrow_mut().push(format!(
            "clicked selection_generation={}",
            c.selection_generation.load(Ordering::Acquire)
        ));
    });
    let observed = trace.clone();
    let c = controller.clone();
    ui.on_audition_requested(move |index| {
        observed
            .borrow_mut()
            .push(format!("audition index={index}"));
        c.select(index, true);
        observed.borrow_mut().push(format!(
            "audition selection_generation={}",
            c.selection_generation.load(Ordering::Acquire)
        ));
    });

    let started = Instant::now();
    let progress = Rc::new(RefCell::new(Progress {
        phase: Phase::Search,
        stage_started: started,
        first_release: None,
        first_position: None,
        first_model: None,
        refresh_generation: None,
        refresh_row_id: None,
        last_observation: String::new(),
        failure: None,
    }));
    let checking = progress.clone();
    let observed = trace.clone();
    let window = ui.as_weak();
    let c = controller.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(5),
        move || {
            let Some(ui) = window.upgrade() else { return };
            let mut state = checking.borrow_mut();
            let status = c.audio.status();
            let observation = format!(
                "phase={:?} selected={} selection_generation={} search_generation={} audio_generation={} session={} state={:?} error={:?} searching={} dirty={} status={}",
                state.phase,
                ui.get_selected_id(),
                c.selection_generation.load(Ordering::Acquire),
                c.search_generation.load(Ordering::Acquire),
                status.generation,
                status.play_session_id,
                status.state,
                status.error,
                ui.get_searching(),
                ui.get_metadata_dirty(),
                ui.get_status()
            );
            if observation != state.last_observation {
                observed.borrow_mut().push(observation.clone());
                state.last_observation = observation.clone();
            }
            if started.elapsed() > Duration::from_secs(20)
                || state.stage_started.elapsed() > Duration::from_secs(5)
            {
                state.failure = Some(format!("Preview stage timed out: {observation}"));
                slint::quit_event_loop().ok();
                return;
            }
            let playing = status.state == PlaybackState::Playing
                && status.file_path.as_ref() == Some(&target)
                && status.volume == 0.0;
            let result = (|| -> Result<(), String> {
                match state.phase {
                    Phase::Search if !ui.get_searching() && ui.get_rows().row_count() == 1 => {
                        if ui.get_audition_play_on_single_click() || ui.get_selected_row() != -1 {
                            return Err("The first double-click must target an unselected row in off mode".into());
                        }
                        state.stage_started = Instant::now();
                        let position = result_position(&ui)?;
                        click_at(&ui, position);
                        state.first_release = Some(Instant::now());
                        state.first_position = Some(position);
                        state.phase = Phase::InitialSecondClick;
                    }
                    Phase::InitialSecondClick | Phase::FinalSecondClick
                        if state.first_release.is_some_and(|at| at.elapsed() >= Duration::from_millis(100)) =>
                    {
                        // Timer ticks let the real workers and UI publications run between releases.
                        if matches!(state.phase, Phase::FinalSecondClick)
                            && state.first_release.is_none_or(|at| at.elapsed() >= Duration::from_millis(500))
                        {
                            return Err("Refreshed second click missed the original double-click interval".into());
                        }
                        click_at(&ui, state.first_position.expect("First click position"));
                        state.phase = match state.phase {
                            Phase::InitialSecondClick => Phase::InitialPlaying,
                            _ => Phase::FinalPlaying,
                        };
                    }
                    Phase::InitialPlaying if playing => {
                        ui.invoke_stop_requested();
                        state.stage_started = Instant::now();
                        state.phase = Phase::EnableAfterStop;
                    }
                    Phase::EnableAfterStop if status.state == PlaybackState::Stopped => {
                        click_checkbox(&ui)?;
                        if !ui.get_audition_play_on_single_click() {
                            return Err("Real checkbox click did not enable single-click preview".into());
                        }
                        click_at(&ui, result_position(&ui)?);
                        state.phase = Phase::SinglePlaying;
                    }
                    Phase::SinglePlaying if playing => {
                        ui.invoke_stop_requested();
                        state.stage_started = Instant::now();
                        state.phase = Phase::DisableAfterStop;
                    }
                    Phase::DisableAfterStop if status.state == PlaybackState::Stopped => {
                        click_checkbox(&ui)?;
                        if ui.get_audition_play_on_single_click() {
                            return Err("Real checkbox click did not disable single-click preview".into());
                        }
                        let position = result_position(&ui)?;
                        state.first_model = Some(ui.get_rows());
                        state.refresh_row_id = Some(ui.get_rows().row_data(0).expect("Selected fixture row").id.to_string());
                        click_at(&ui, position);
                        state.first_release = Some(Instant::now());
                        state.first_position = Some(position);
                        // A Metadata save or server update can refresh this same record
                        // between the two clicks. Use the production search worker so it
                        // publishes the same record again through the real controller.
                        c.provider().update_metadata(
                            records[0].id,
                            &MetadataPatch { favorite: Some(true), ..Default::default() },
                        ).map_err(|error| error.to_string())?;
                        let before = c.search_generation.load(Ordering::Acquire);
                        c.request_search(&ui, true, false);
                        let generation = c.search_generation.load(Ordering::Acquire);
                        if generation <= before {
                            return Err("Production refresh did not advance search generation".into());
                        }
                        state.refresh_generation = Some(generation);
                        observed.borrow_mut().push(format!("refresh requested search_generation={before}->{generation} row_id={:?}", state.refresh_row_id));
                        state.phase = Phase::FinalRefresh;
                    }
                    Phase::FinalRefresh => {
                        let first_release = state.first_release.expect("First click release time");
                        if first_release.elapsed() >= Duration::from_millis(450) {
                            return Err("Production refresh did not publish inside the double-click interval".into());
                        }
                        if !ui.get_searching() {
                            let rows = ui.get_rows();
                            if rows != *state.first_model.as_ref().expect("Original model identity") {
                                return Err("Same-record refresh replaced the native click target".into());
                            }
                            if rows.row_count() != 1 {
                                return Err("Same-record refresh changed fixture row count".into());
                            }
                            let row = rows.row_data(0).expect("Refreshed fixture row");
                            if !row.favorite {
                                return Err("Refresh did not publish the new favorite Metadata".into());
                            }
                            if status.state == PlaybackState::Playing {
                                return Err("Off-mode first click unexpectedly started playback".into());
                            }
                            let id = row.id.to_string();
                            if Some(&id) != state.refresh_row_id.as_ref() {
                                return Err(format!("Same-record refresh changed row id: {id}"));
                            }
                            let generation = c.search_generation.load(Ordering::Acquire);
                            if Some(generation) != state.refresh_generation {
                                return Err(format!("Unexpected concurrent search generation: {generation}"));
                            }
                            observed.borrow_mut().push(format!("refresh published retained ModelRc search_generation={generation} row_id={id} elapsed={:?}", first_release.elapsed()));
                            state.phase = Phase::FinalSecondClick;
                        }
                    }
                    Phase::FinalPlaying if playing => {
                        state.phase = Phase::Complete;
                        slint::quit_event_loop().ok();
                    }
                    _ => {}
                }
                Ok(())
            })();
            if let Err(error) = result {
                state.failure = Some(error);
                slint::quit_event_loop().ok();
            }
        },
    );
    ui.show().unwrap();
    slint::run_event_loop().unwrap();
    timer.stop();
    controller.shutdown();
    ui.hide().unwrap();
    for entry in trace.borrow().iter() {
        println!("{entry}");
    }
    let state = progress.borrow();
    assert!(state.failure.is_none(), "{:?}", state.failure);
    assert!(
        matches!(state.phase, Phase::Complete),
        "Incomplete preview phase: {:?}",
        state.phase
    );
    // The parent removes the fixture after process exit, when all worker handles are gone.
}

fn owned_test_root(root: &Path) -> bool {
    root.is_absolute()
        && root.starts_with(std::env::temp_dir())
        && root.file_name().is_some_and(|name| {
            name.to_string_lossy()
                .starts_with("audiolib-click-preview-")
        })
}

fn result_position(ui: &AppWindow) -> Result<slint::LogicalPosition, String> {
    let row = ElementHandle::find_by_accessible_label(ui, WAV_NAME)
        .find(|element| element.accessible_role() == Some(AccessibleRole::ListItem))
        .ok_or_else(|| "Missing real result row".to_owned())?;
    let origin = row.absolute_position();
    Ok(slint::LogicalPosition::new(
        origin.x + 8.0,
        origin.y + row.size().height / 2.0,
    ))
}

fn click_checkbox(ui: &AppWindow) -> Result<(), String> {
    let checkbox = ElementHandle::find_by_accessible_label(ui, "Single-click play")
        .next()
        .ok_or_else(|| "Missing single-click preview checkbox".to_owned())?;
    let origin = checkbox.absolute_position();
    let size = checkbox.size();
    click_at(
        ui,
        slint::LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0),
    );
    Ok(())
}

fn click_at(ui: &AppWindow, position: slint::LogicalPosition) {
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

fn write_silent_wav(path: &Path) {
    let data_bytes = 3 * 48_000 * 2u32;
    let mut wav = Vec::with_capacity(data_bytes as usize + 44);
    wav.extend(b"RIFF");
    wav.extend((data_bytes + 36).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(48_000u32.to_le_bytes());
    wav.extend(96_000u32.to_le_bytes());
    wav.extend(2u16.to_le_bytes());
    wav.extend(16u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(data_bytes.to_le_bytes());
    wav.resize(data_bytes as usize + 44, 0);
    std::fs::write(path, wav).unwrap();
}
