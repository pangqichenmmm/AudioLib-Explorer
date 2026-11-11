// crates/audiolib-update/src/deploy/server_pair_tests.rs
//! Real Product::Server deployment and recoverable partial-install failures.
//! These exercise ordinary error rollback, not power-loss atomicity.
use super::*;

struct Fixture {
    _root: tempfile::TempDir,
    install: PathBuf,
    stage: PathBuf,
    plan: Plan,
}

fn server_name() -> &'static str {
    if cfg!(windows) {
        "audiolib-server.exe"
    } else {
        "audiolib-server"
    }
}

fn migrator_name() -> &'static str {
    if cfg!(windows) {
        "audiolib-migrate.exe"
    } else {
        "audiolib-migrate"
    }
}

fn fixture(prior_migrator: bool) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let install = root.path().join("install");
    let stage = root.path().join("stage");
    std::fs::create_dir(&install).unwrap();
    std::fs::create_dir_all(stage.join("payload")).unwrap();
    for name in package::required(Product::Server) {
        if *name != migrator_name() || prior_migrator {
            std::fs::write(install.join(name), format!("previous {name}")).unwrap();
        }
        std::fs::write(
            stage.join("payload").join(name),
            format!("candidate {name}"),
        )
        .unwrap();
    }
    for (name, bytes) in [
        ("library.db", b"private library".as_slice()),
        ("config.toml", b"private configuration".as_slice()),
        ("tokens.json", b"private credentials".as_slice()),
    ] {
        std::fs::write(install.join(name), bytes).unwrap();
    }
    let mut files = Vec::new();
    collect(
        &stage.join("payload"),
        &stage.join("payload"),
        Product::Server,
        &mut files,
    )
    .unwrap();
    // Match PreparedUpdate::plan's actual deterministic install order.
    files.sort_by(|left, right| left.name.cmp(&right.name));
    let plan = Plan {
        install: install.clone(),
        product: Product::Server,
        executable: server_name().into(),
        arguments: Vec::new(),
        working_directory: install.clone(),
        files,
    };
    Fixture {
        _root: root,
        install,
        stage,
        plan,
    }
}

fn assert_private_files(fixture: &Fixture) {
    for (name, bytes) in [
        ("library.db", b"private library".as_slice()),
        ("config.toml", b"private configuration".as_slice()),
        ("tokens.json", b"private credentials".as_slice()),
    ] {
        assert_eq!(std::fs::read(fixture.install.join(name)).unwrap(), bytes);
    }
}

#[test]
fn complete_server_pair_is_installed_and_private_files_are_preserved() {
    for prior_migrator in [false, true] {
        let fixture = fixture(prior_migrator);
        let changed = deploy(&fixture.plan, &fixture.stage, None).unwrap();
        assert_eq!(changed.len(), package::required(Product::Server).len());
        for name in package::required(Product::Server) {
            assert_eq!(
                std::fs::read(fixture.install.join(name)).unwrap(),
                format!("candidate {name}").as_bytes()
            );
        }
        assert_private_files(&fixture);
    }
}

#[test]
fn missing_migrator_plan_is_rejected_before_any_installed_file_changes() {
    let mut fixture = fixture(true);
    fixture
        .plan
        .files
        .retain(|file| file.name != migrator_name());
    let error = deploy(&fixture.plan, &fixture.stage, None).unwrap_err();
    assert!(error.to_string().contains("Incomplete update plan"));
    assert!(!fixture.stage.join("backup").exists());
    for name in package::required(Product::Server) {
        assert_eq!(
            std::fs::read(fixture.install.join(name)).unwrap(),
            format!("previous {name}").as_bytes()
        );
    }
    assert_private_files(&fixture);
}

#[test]
fn second_binary_failure_restores_both_previous_binaries_or_prior_migrator_absence() {
    for prior_migrator in [false, true] {
        let fixture = fixture(prior_migrator);
        let first_binary = fixture
            .plan
            .files
            .iter()
            .position(|file| file.name == migrator_name())
            .unwrap();
        let second_binary = fixture
            .plan
            .files
            .iter()
            .position(|file| file.name == server_name())
            .unwrap();
        assert!(first_binary < second_binary);
        // The real installer has already moved the candidate migrator into
        // place, then backs up the server and takes its existing fault hook.
        let error = deploy(&fixture.plan, &fixture.stage, Some(second_binary + 1)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Injected failure after backing up an installed file")
        );
        assert!(!fixture.stage.join("payload").join(migrator_name()).exists());
        assert!(fixture.stage.join("payload").join(server_name()).is_file());
        for name in package::required(Product::Server) {
            let installed = fixture.install.join(name);
            if *name == migrator_name() && !prior_migrator {
                assert!(!installed.exists(), "rollback must restore prior absence");
            } else {
                assert_eq!(
                    std::fs::read(installed).unwrap(),
                    format!("previous {name}").as_bytes()
                );
            }
        }
        assert_private_files(&fixture);
    }
}
