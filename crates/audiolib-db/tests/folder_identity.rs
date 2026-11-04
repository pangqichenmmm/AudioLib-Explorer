// crates/audiolib-db/tests/folder_identity.rs
//! Stored folder lookup keeps source path identity independent of the reader's OS.
mod support;
use audiolib_core::{AlbumKind, DataProvider, Error};
use audiolib_db::{SqliteProvider, indexed_paths_equal};
use std::path::Path;

#[test]
fn indexed_comparison_preserves_source_case_and_posix_backslashes() {
    for (left, right) in [
        ("/Volumes/SFX/Wind", "/Volumes/SFX/Wind/"),
        ("C:/Sounds/Wind", r"c:\Sounds\Wind"),
        (r"\\?\C:\Sounds\Wind\", "c:/Sounds/Wind"),
        (
            r"\\?\UNC\SERVER\SHARE\Sounds\Wind",
            "//server/share/Sounds/Wind/",
        ),
    ] {
        assert!(indexed_paths_equal(left, right), "{left} != {right}");
    }
    for (left, right) in [
        ("/Volumes/SFX/Wind", "/Volumes/SFX/wind"),
        (r"/Volumes/SFX/Wind\Take", "/Volumes/SFX/Wind/Take"),
        ("C:/Sensitive/Wind", "c:/Sensitive/wind"),
        ("//server/share/Sounds/Wind", "//server/share/sounds/Wind"),
    ] {
        assert!(!indexed_paths_equal(left, right), "{left} == {right}");
    }
}

#[test]
fn snapshot_album_semantic_lookup_uses_stored_paths_without_scanner() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("albums.db");
    let local = SqliteProvider::open(&database).unwrap();
    let conn = support::connection(&database).unwrap();
    for (id, path) in [
        (1, "/offline/SFX/Wind"),
        (2, "/offline/SFX/wind"),
        (3, "C:/Offline/Sounds/Boom"),
        (4, "C:/Offline/Duplicate/Wind"),
        (5, r"c:\Offline\Duplicate\Wind"),
    ] {
        conn.execute(
            "INSERT INTO folders(id,folder_path,folder_name) VALUES(?1,?2,'sounds')",
            rusqlite::params![id, path],
        )
        .unwrap();
        conn.execute("INSERT INTO folder_closure VALUES(?1,?1,0)", [id])
            .unwrap();
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename,folder_id) VALUES(?1,?2,'sound.wav',?1)",
            rusqlite::params![id, format!("{path}/sound.wav")],
        )
        .unwrap();
    }
    drop(conn);
    drop(local);
    let original = std::fs::read(&database).unwrap();
    let snapshot = SqliteProvider::open_read_only(&database).unwrap();
    let provider: &dyn DataProvider = &snapshot;
    for (path, id) in [
        ("/offline/SFX/Wind", 1),
        ("/offline/SFX/wind", 2),
        (r"c:\offline\sounds\BOOM", 3),
    ] {
        let album = provider
            .find_album_by_semantic(AlbumKind::Folder, None, Some(path))
            .unwrap()
            .unwrap();
        assert_eq!(album.folder_id, Some(id));
        assert_eq!(provider.get_audio(id).unwrap().album_id, Some(album.id));
    }
    assert!(
        provider
            .find_album_by_semantic(AlbumKind::Folder, None, Some("/offline/sfx/wind"))
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        provider.find_album_by_semantic(AlbumKind::Folder, None, Some("C:/Offline/Duplicate/Wind")),
        Err(Error::Validation(_))
    ));
    drop(snapshot);
    assert_eq!(std::fs::read(&database).unwrap(), original);
}

#[test]
fn snapshot_folder_lookup_is_exact_first_and_refuses_ambiguous_aliases() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("library.db");
    let local = SqliteProvider::open(&database).unwrap();
    let conn = support::connection(&database).unwrap();
    for (id, path) in [
        (1, "/Volumes/SFX/Wind"),
        (2, "/Volumes/SFX/wind"),
        (3, r"/Volumes/SFX/Wind\Take"),
        (4, "/Volumes/SFX/Wind/Take"),
        (5, "C:/Sounds/Boom"),
        (6, "C:/Sensitive/Foo"),
        (7, "C:/Sensitive/foo"),
        (8, "C:/Duplicate/Wind"),
        (9, r"c:\Duplicate\Wind"),
        (10, r"\\Server\Share\Sounds\Wind"),
    ] {
        conn.execute(
            "INSERT INTO folders(id,folder_path,folder_name) VALUES(?1,?2,'sounds')",
            rusqlite::params![id, path],
        )
        .unwrap();
    }
    drop(conn);
    drop(local);
    let snapshot = SqliteProvider::open_read_only(&database).unwrap();
    for (path, id) in [
        ("/Volumes/SFX/Wind", 1),
        ("/Volumes/SFX/wind", 2),
        (r"/Volumes/SFX/Wind\Take", 3),
        ("/Volumes/SFX/Wind/Take", 4),
        ("c:/sounds/BOOM", 5),
        (r"\\?\C:\Sensitive\Foo\", 6),
        ("c:/Sensitive/foo", 7),
        ("//SERVER/SHARE/Sounds/Wind/", 10),
    ] {
        assert_eq!(
            snapshot
                .find_folder_by_path(Path::new(path))
                .unwrap()
                .unwrap()
                .id,
            id,
            "wrong folder for {path}",
        );
    }
    for path in ["/volumes/sfx/wind", "/Volumes/SFX/not-indexed"] {
        assert!(
            snapshot
                .find_folder_by_path(Path::new(path))
                .unwrap()
                .is_none()
        );
    }
    for path in ["c:/sensitive/FOO", "C:/Duplicate/Wind"] {
        assert!(
            snapshot.find_folder_by_path(Path::new(path)).is_err(),
            "ambiguous stored identity must be refused: {path}",
        );
    }
}

#[test]
fn concurrent_id_path_rewrites_cannot_redirect_audio_or_folder_lookup() {
    use std::sync::{Arc, Barrier};
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("library.db");
    let local = SqliteProvider::open(&database).unwrap();
    let conn = support::connection(&database).unwrap();
    conn.execute_batch(
        "INSERT INTO audio_files(id,file_path,path_key,filename) VALUES
         (1,'/offline/first.wav','/offline/first.wav','first.wav'),
         (2,'/offline/second.wav','/offline/second.wav','second.wav');
         INSERT INTO folders(id,folder_path,folder_name) VALUES
         (1,'/offline/first','first'),(2,'/offline/second','second');",
    )
    .unwrap();
    drop(conn);
    let snapshot = SqliteProvider::open_read_only(&database).unwrap();
    // Open and configure before starting the thread, so a setup failure cannot
    // strand the reader at its start barrier. Both loops have fixed bounds.
    let mut writer = support::connection(&database).unwrap();
    writer
        .busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let start = Arc::new(Barrier::new(2));
    let writer_start = start.clone();
    let thread = std::thread::spawn(move || -> Result<(), String> {
        writer_start.wait();
        for index in 0..512 {
            let tx = writer.transaction().map_err(|error| error.to_string())?;
            let (first_audio, second_audio, first_folder, second_folder) = if index % 2 == 0 {
                (
                    "/offline/second.wav",
                    "/offline/first.wav",
                    "/offline/second",
                    "/offline/first",
                )
            } else {
                (
                    "/offline/first.wav",
                    "/offline/second.wav",
                    "/offline/first",
                    "/offline/second",
                )
            };
            tx.execute("UPDATE audio_files SET file_path='/offline/temporary.wav',path_key='/offline/temporary.wav' WHERE id=1", []).map_err(|error| error.to_string())?;
            tx.execute(
                "UPDATE audio_files SET file_path=?1,path_key=?1 WHERE id=2",
                [second_audio],
            )
            .map_err(|error| error.to_string())?;
            tx.execute(
                "UPDATE audio_files SET file_path=?1,path_key=?1 WHERE id=1",
                [first_audio],
            )
            .map_err(|error| error.to_string())?;
            tx.execute(
                "UPDATE folders SET folder_path='/offline/temporary' WHERE id=1",
                [],
            )
            .map_err(|error| error.to_string())?;
            tx.execute(
                "UPDATE folders SET folder_path=?1 WHERE id=2",
                [second_folder],
            )
            .map_err(|error| error.to_string())?;
            tx.execute(
                "UPDATE folders SET folder_path=?1 WHERE id=1",
                [first_folder],
            )
            .map_err(|error| error.to_string())?;
            tx.commit().map_err(|error| error.to_string())?;
            std::thread::yield_now();
        }
        Ok(())
    });
    start.wait();
    let mut failure = None;
    for _ in 0..1024 {
        let audio = snapshot.find_audio_by_path(Path::new("/offline/first.wav"));
        let folder = snapshot.find_folder_by_path(Path::new("/offline/first"));
        if !matches!(&audio, Ok(Some(audio)) if audio.file_path == "/offline/first.wav")
            || !matches!(&folder, Ok(Some(folder)) if folder.path == "/offline/first")
        {
            failure.get_or_insert_with(|| {
                format!("lookup redirected during ID reuse: {audio:?}, {folder:?}")
            });
        }
        std::thread::yield_now();
    }
    // Always join the bounded writer before asserting, including on a bad lookup.
    thread
        .join()
        .expect("owned rewrite thread panicked")
        .unwrap();
    assert!(failure.is_none(), "{}", failure.unwrap_or_default());
    drop(local);
}
