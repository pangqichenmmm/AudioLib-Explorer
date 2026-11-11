// app/audiolib-desktop/src/search_task_waveform_review_tests.rs
//! A cancelled tab's Waveform gets a successor without changing the audio source.
use super::super::audio_ref;
use super::*;

#[test]
fn task_switch_restarts_pending_waveform_without_loading_or_playing_audio() {
    isolated("waveform_review::child_task_waveform_restart");
}

// Stage a request while the worker is idle, without notifying it. This gives the
// switch a genuine pending slot and an already-posted partial publication to reject.
fn pending_waveform(controller: &Arc<Controller>, ui: &AppWindow, file: &AudioFile) -> WaveJob {
    controller.invalidate_waveform();
    let generation = controller
        .waveform_generation
        .fetch_add(1, Ordering::AcqRel)
        + 1;
    let cancel = Arc::new(AtomicBool::new(false));
    *controller.waveform_cancel.lock().unwrap() = cancel.clone();
    let job = WaveJob {
        source: waveform_selection::WaveSource::capture(controller, file),
        path: PathBuf::from(&file.file_path),
        remote_id: None,
        sample_rate: file.sample_rate,
        frames: 1,
        start_frame: 0,
        end_frame: 1,
        duration: file.duration,
        view: WaveView::default(),
        generation,
        cancel,
        width: 64,
        height: 32,
    };
    {
        let mut pending = controller.waveform_job.lock().unwrap();
        *pending = Some(job.clone());
        assert_eq!(pending.as_ref().unwrap().generation, generation);
    }
    ui.set_waveform_ready(false);
    ui.set_waveform_info("pending previous task".into());
    // It is already queued for the event loop when the task changes. The actual
    // publication callback must check cancellation/generation a second time.
    controller.publish_waveform(
        &job,
        &audiolib_audio::WaveformData {
            sample_rate: 12_345,
            channels: 1,
            frames: 1,
            samples_per_pixel: 1,
            peaks: vec![vec![audiolib_audio::WaveformPeak::default()]],
        },
        0.25,
    );
    job
}

fn audio_unchanged(
    before: &audiolib_audio::PlaybackStatus,
    after: &audiolib_audio::PlaybackStatus,
) {
    assert_eq!(after.file_path, before.file_path);
    assert_eq!(after.generation, before.generation);
    assert_eq!(after.play_session_id, before.play_session_id);
    assert_eq!(after.state, before.state);
    assert_eq!(after.position, before.position);
}

#[test]
#[ignore = "isolated tab Waveform cancellation and successor regression"]
fn child_task_waveform_restart() {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    let target = active_id(ui);
    let files = fixture
        .provider
        .search(&SearchRequest::default())
        .unwrap()
        .results;
    let wind = files
        .iter()
        .find(|file| file.filename == "wind.wav")
        .unwrap()
        .clone();
    let gore = files
        .iter()
        .find(|file| file.filename == "gore one.wav")
        .unwrap()
        .clone();

    ui.invoke_search_task_created();
    wait_for(ui, |ui| {
        ui.get_search_tasks().row_count() == 2
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
    });
    {
        let mut workspace = controller.task_workspace.lock().unwrap();
        let task = workspace
            .workspace
            .session
            .tasks
            .iter_mut()
            .find(|task| task.id == target)
            .unwrap();
        task.view.selected_audio = Some(audio_ref(&wind));
    }
    let before = fixture.engine.handle().status();
    assert_eq!(before.generation, 0);
    let obsolete = pending_waveform(controller, ui, &gore);
    ui.invoke_search_task_selected(target.into());
    assert!(obsolete.cancel.load(Ordering::Acquire));
    assert!(controller.waveform_generation.load(Ordering::Acquire) > obsolete.generation);
    let wanted = wind.clone();
    wait_for(ui, move |ui| {
        !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && ui.get_selected_id().as_str() == wanted.id.to_string()
            && ui.get_waveform_ready()
            && ui.get_waveform_info().contains("48.0 kHz")
    });
    assert_eq!(controller.wave_view.lock().unwrap().0, wind.file_path);
    assert!(!ui.get_waveform_info().contains("25%"));
    audio_unchanged(&before, &fixture.engine.handle().status());

    // No timing dependence on a real Load: exercise the intermediate source
    // status directly. Only the matching selected row may provide its Metadata.
    let mut loading = before.clone();
    loading.state = PlaybackState::Loading;
    loading.file_path = Some(PathBuf::from(&wind.file_path));
    assert_eq!(
        controller.task_waveform_source(&loading).unwrap().file_path,
        wind.file_path
    );
    loading.file_path = Some(PathBuf::from(&gore.file_path));
    assert!(controller.task_waveform_source(&loading).is_none());
    loading.state = PlaybackState::Ready;
    loading.sample_rate = gore.sample_rate;
    assert_eq!(
        controller.task_waveform_source(&loading).unwrap().file_path,
        gore.file_path,
        "a loaded global source keeps precedence over another selected row"
    );

    // With no loaded audio, an empty task must not retain another task's preview.
    ui.invoke_search_task_created();
    wait_for(ui, |ui| {
        !ui.get_search_workspace_loading() && !ui.get_task_restoring() && !ui.get_has_selection()
    });
    assert!(!ui.get_waveform_ready());
    assert!(ui.get_waveform_info().is_empty());
    audio_unchanged(&before, &fixture.engine.handle().status());

    // Loading the owned WAV is setup only: Ready does not start an output device.
    // Let the status observer see that generation before cancelling the next image.
    fixture
        .engine
        .handle()
        .send_and_wait(
            AudioCommand::Load(PathBuf::from(&gore.file_path)),
            Duration::from_secs(4),
        )
        .unwrap();
    wait_for(ui, |ui| ui.get_audio_ready() && ui.get_waveform_ready());
    let loaded = fixture.engine.handle().status();
    assert_eq!(loaded.state, PlaybackState::Ready);
    let obsolete = pending_waveform(controller, ui, &wind);
    ui.invoke_search_task_created();
    assert!(obsolete.cancel.load(Ordering::Acquire));
    wait_for(ui, |ui| {
        !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_has_selection()
            && ui.get_waveform_ready()
            && ui.get_waveform_info().contains("48.0 kHz")
    });
    assert_eq!(controller.wave_view.lock().unwrap().0, gore.file_path);
    assert!(!ui.get_waveform_info().contains("25%"));
    audio_unchanged(&loaded, &fixture.engine.handle().status());

    // A failed identity read also cancelled an in-flight image. Its retained
    // window must resume the loaded source even though no target selection exists.
    let id = active_id(ui);
    let obsolete = pending_waveform(controller, ui, &wind);
    let (_, _, epoch) = controller.prepare_search_task_restore(ui, &id).unwrap();
    assert!(obsolete.cancel.load(Ordering::Acquire));
    controller.finish_search_task_restore(
        ui,
        &id,
        epoch,
        Err("injected identity read failure".into()),
    );
    wait_for(ui, |ui| {
        ui.get_waveform_ready() && ui.get_waveform_info().contains("48.0 kHz")
    });
    assert!(controller.task_workspace.lock().unwrap().restore_failed);
    assert_eq!(controller.wave_view.lock().unwrap().0, gore.file_path);
    assert!(!ui.get_waveform_info().contains("25%"));
    audio_unchanged(&loaded, &fixture.engine.handle().status());
    finish(&fixture);
}

#[test]
fn remote_task_waveform_keeps_the_loaded_source_and_case_distinct_identity_without_audio_commands()
{
    isolated("waveform_review::child_remote_loaded_waveform_priority");
}

#[test]
#[ignore = "isolated remote loaded source priority and path identity regression"]
fn child_remote_loaded_waveform_priority() {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    let files = fixture
        .provider
        .search(&SearchRequest::default())
        .unwrap()
        .results;
    let source = files
        .iter()
        .find(|file| file.filename == "gore one.wav")
        .unwrap();
    let external = files
        .iter()
        .find(|file| file.filename == "wind.wav")
        .unwrap();
    // The sole Load is fixture setup; all checks below call task source/resume
    // helpers and must leave this Ready source and its play session unchanged.
    fixture
        .engine
        .handle()
        .send_and_wait(
            AudioCommand::Load(PathBuf::from(&source.file_path)),
            Duration::from_secs(4),
        )
        .unwrap();
    wait_for(ui, |ui| ui.get_audio_ready() && ui.get_waveform_ready());
    let before = fixture.engine.handle().status();
    assert_eq!(before.state, PlaybackState::Ready);
    let loaded = AudioFile::from(audiolib_core::AudioFileData {
        id: 900_001,
        filename: "loaded.wav".into(),
        file_path: "/server/Audio/loaded.wav".into(),
        sample_rate: source.sample_rate,
        channels: source.channels,
        duration: source.duration,
        ..Default::default()
    });
    let selected = AudioFile::from(audiolib_core::AudioFileData {
        id: 900_002,
        filename: "selected.wav".into(),
        file_path: "/server/Audio/selected.wav".into(),
        ..loaded.clone().into_data()
    });
    controller.set_remote_wave_file_for_test(PathBuf::from(&source.file_path), loaded.clone());
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote_busy.store(true, Ordering::Release);
    *controller.selected.lock().unwrap() = Some(selected.clone());
    assert_eq!(controller.waveform_file(&before).unwrap().id, selected.id);
    assert_eq!(
        controller.task_waveform_source(&before).unwrap().id,
        loaded.id
    );
    let mut playing = before.clone();
    playing.state = PlaybackState::Playing;
    assert_eq!(
        controller.task_waveform_source(&playing).unwrap().id,
        loaded.id
    );
    let mut loading = before.clone();
    loading.state = PlaybackState::Loading;
    loading.sample_rate = 0;
    assert_eq!(
        controller.task_waveform_source(&loading).unwrap().id,
        loaded.id
    );
    controller.resume_task_waveform(ui);
    assert_eq!(controller.wave_view.lock().unwrap().0, loaded.file_path);
    // No remote endpoint is configured in this owned fixture. A queued analysis
    // can fail as unsynced, but cannot issue an audio Load/Play or make HTTP calls.
    audio_unchanged(&before, &fixture.engine.handle().status());

    let mut ipc = before.clone();
    ipc.file_path = Some(PathBuf::from(&external.file_path));
    let waveform = controller.task_waveform_source(&ipc).unwrap();
    assert_eq!(waveform.id, 0);
    assert_eq!(waveform.file_path, external.file_path);
    let mut empty = before.clone();
    empty.state = PlaybackState::Empty;
    empty.file_path = None;
    empty.sample_rate = 0;
    assert_eq!(
        controller.task_waveform_source(&empty).unwrap().id,
        selected.id
    );

    assert!(controller.task_selection_is_loaded(&loaded, &before));
    let wrong_case = AudioFile::from(audiolib_core::AudioFileData {
        file_path: "/server/audio/loaded.wav".into(),
        ..loaded.clone().into_data()
    });
    assert!(!controller.task_selection_is_loaded(&wrong_case, &before));
    let wrong_id = AudioFile::from(audiolib_core::AudioFileData {
        id: loaded.id + 1,
        ..loaded.clone().into_data()
    });
    assert!(!controller.task_selection_is_loaded(&wrong_id, &before));

    // A Windows reader must not case-fold a POSIX snapshot source in either the
    // already-loaded check or the selected-row fallback while a Load finishes.
    controller.remote_mode.store(false, Ordering::Release);
    let mut native = before.clone();
    native.file_path = Some(PathBuf::from(&loaded.file_path));
    assert!(controller.task_selection_is_loaded(&loaded, &native));
    assert!(!controller.task_selection_is_loaded(&wrong_case, &native));
    native.state = PlaybackState::Loading;
    native.sample_rate = 0;
    *controller.selected.lock().unwrap() = Some(wrong_case);
    assert!(controller.task_waveform_source(&native).is_none());
    *controller.selected.lock().unwrap() = Some(loaded.clone());
    assert_eq!(
        controller.task_waveform_source(&native).unwrap().file_path,
        loaded.file_path
    );
    audio_unchanged(&before, &fixture.engine.handle().status());
    controller.remote_busy.store(false, Ordering::Release);
    *controller.selected.lock().unwrap() = None;
    finish(&fixture);
}
