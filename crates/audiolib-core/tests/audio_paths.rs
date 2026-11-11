// crates/audiolib-core/tests/audio_paths.rs
//! Supported source-path resolution preserves the same root boundary as WAV-only callers.
use audiolib_core::{is_supported_audio_extension, validate_audio_path, validate_wav_path};

#[test]
fn supported_audio_types_resolve_real_paths_while_wav_callers_keep_their_strict_contract() {
    let owned = tempfile::tempdir().unwrap();
    let root = owned.path().join("allowed");
    let nested = root.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let roots = [root.clone()];
    for extension in ["wav", "FLAC", "ogg", "OGA", "BWF", "mp3", "AIF", "aiff"] {
        let path = root.join(format!("sound.{extension}"));
        std::fs::write(&path, []).unwrap();
        let indirect = nested.join("..").join(path.file_name().unwrap());
        assert!(is_supported_audio_extension(&indirect));
        assert_eq!(
            validate_audio_path(&indirect, &roots).unwrap(),
            path.canonicalize().unwrap()
        );
        assert_eq!(validate_wav_path(&path, &roots).is_ok(), extension == "wav");
    }
}

#[test]
fn audio_paths_reject_missing_nonfiles_unsupported_types_and_root_escapes() {
    let owned = tempfile::tempdir().unwrap();
    let allowed = owned.path().join("allowed");
    let outside = owned.path().join("allowed-other");
    std::fs::create_dir(&allowed).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let roots = [allowed.clone()];
    for extension in ["wav", "flac", "ogg", "oga", "bwf", "mp3", "aif", "aiff"] {
        let name = format!("outside.{extension}");
        std::fs::write(outside.join(&name), []).unwrap();
        assert!(validate_audio_path(&outside.join(&name), &roots).is_err());
        assert!(validate_audio_path(&allowed.join("../allowed-other").join(name), &roots).is_err());
    }
    for extension in ["opus", "aac", "aifc", "txt"] {
        let path = allowed.join(format!("unsupported.{extension}"));
        std::fs::write(&path, []).unwrap();
        assert!(!is_supported_audio_extension(&path));
        assert!(validate_audio_path(&path, &roots).is_err());
    }
    let folder = allowed.join("directory.flac");
    std::fs::create_dir(&folder).unwrap();
    assert!(validate_audio_path(&folder, &roots).is_err());
    assert!(validate_audio_path(&allowed.join("absent.ogg"), &roots).is_err());
    assert!(validate_audio_path(std::path::Path::new("invalid\0.flac"), &roots).is_err());
}
