// crates/audiolib-update/src/tests.rs
//! Regression coverage for release selection and update data boundaries.
use super::*;
use std::io::Write;

const NATIVE_PRODUCT: Product = if cfg!(windows) {
    Product::Desktop
} else {
    Product::Server
};

fn release(tag: &str, draft: bool, prerelease: bool) -> Release {
    release_for_platform(
        NATIVE_PRODUCT,
        tag,
        draft,
        prerelease,
        native_platform().unwrap(),
    )
}

fn release_for_platform(
    product: Product,
    tag: &str,
    draft: bool,
    prerelease: bool,
    platform: Platform,
) -> Release {
    let name = asset_names_for_platform(product, tag, platform).unwrap()[0].clone();
    Release {
        tag_name: tag.into(),
        body: Some("notes".into()),
        draft,
        prerelease,
        assets: [name.clone(), format!("{name}.sha256")]
            .into_iter()
            .map(|name| Asset {
                browser_download_url: format!(
                    "https://github.com/{REPOSITORY}/releases/download/{tag}/{name}"
                ),
                name,
                size: 128,
                digest: None,
            })
            .collect(),
    }
}

#[test]
fn releases_obey_numeric_beta_order_and_stable_channel() {
    let beta = UpdateClient::new(NATIVE_PRODUCT, Channel::Beta, "0.6.2-beta.2").unwrap();
    assert_eq!(
        beta.select(vec![
            release("v0.6.2-beta.9", false, true),
            release("v0.6.2-beta.10", false, true)
        ])
        .unwrap()
        .unwrap()
        .version,
        "0.6.2-beta.10"
    );
    assert_eq!(
        beta.select(vec![
            release("v0.6.2-beta.10", false, true),
            release("v0.6.2", false, false)
        ])
        .unwrap()
        .unwrap()
        .version,
        "0.6.2"
    );
    let stable = UpdateClient::new(NATIVE_PRODUCT, Channel::Stable, "0.6.1").unwrap();
    assert_eq!(
        stable
            .select(vec![
                release("v0.7.0-beta.1", false, true),
                release("v0.6.2", false, false)
            ])
            .unwrap()
            .unwrap()
            .version,
        "0.6.2"
    );
    assert!(
        stable
            .select(vec![release("v0.7.0", true, false)])
            .unwrap()
            .is_none()
    );
    assert!(
        stable
            .select(vec![release("v0.6.0", false, false)])
            .unwrap()
            .is_none()
    );
}

#[test]
fn wrong_repository_duplicate_and_incomplete_assets_are_reported() {
    let client = UpdateClient::new(NATIVE_PRODUCT, Channel::Stable, "0.6.1").unwrap();
    let mut wrong = release("v0.6.2", false, false);
    wrong.assets[0].browser_download_url = wrong.assets[0]
        .browser_download_url
        .replace("Freedragonlancer", "attacker");
    let error = client.select(vec![wrong]).unwrap_err().to_string();
    assert!(
        error.contains("v0.6.2") && error.contains("no valid package"),
        "{error}"
    );
    let mut duplicate = release("v0.6.2", false, false);
    duplicate.assets.push(duplicate.assets[0].clone());
    let error = client.select(vec![duplicate]).unwrap_err().to_string();
    assert!(
        error.contains("v0.6.2") && error.contains("no valid package"),
        "{error}"
    );
    let mut incomplete = release("v0.6.2", false, false);
    incomplete.assets.pop();
    let error = client.select(vec![incomplete]).unwrap_err().to_string();
    assert!(
        error.contains("v0.6.2") && error.contains("no valid package"),
        "{error}"
    );
}

#[test]
fn historical_release_names_without_v_and_lowercase_server_are_supported() {
    for product in [Product::Desktop, Product::Server] {
        let mut release =
            release_for_platform(product, "v0.6.2", false, false, Platform::WindowsX64);
        let historical =
            asset_names_for_platform(product, "v0.6.2", Platform::WindowsX64).unwrap()[1].clone();
        for (index, asset) in release.assets.iter_mut().enumerate() {
            asset.name = if index == 0 {
                historical.clone()
            } else {
                format!("{historical}.sha256")
            };
            asset.browser_download_url = format!(
                "https://github.com/{REPOSITORY}/releases/download/v0.6.2/{}",
                asset.name
            );
        }
        let client = UpdateClient::new(product, Channel::Stable, "0.6.1").unwrap();
        let update = client
            .select_for_platform(vec![release], Platform::WindowsX64)
            .unwrap()
            .unwrap();
        assert_eq!(update.asset.name, historical);
    }
}

#[test]
fn known_binary_only_mac_archives_are_not_offered_as_installable_updates() {
    let client = UpdateClient::new(Product::Server, Channel::Beta, "0.5.0-0").unwrap();
    for tag in ["v0.5.0", "v0.5.1", "v0.6.0", "v0.6.1", "v0.6.2"] {
        let mut legacy =
            release_for_platform(Product::Server, tag, false, false, Platform::MacArm64);
        let archive_name = format!("audiolib-server-{}-macos-arm64.tar.gz", &tag[1..]);
        for (index, asset) in legacy.assets.iter_mut().enumerate() {
            asset.name = if index == 0 {
                archive_name.clone()
            } else {
                format!("{archive_name}.sha256")
            };
            asset.browser_download_url = format!(
                "https://github.com/{REPOSITORY}/releases/download/{tag}/{}",
                asset.name
            );
        }
        assert!(
            client
                .select_for_platform(vec![legacy], Platform::MacArm64)
                .is_err(),
            "known binary-only archive {tag} must be reported, not offered or hidden"
        );
        let flat = release_for_platform(Product::Server, tag, false, false, Platform::MacArm64);
        assert_eq!(
            client
                .select_for_platform(vec![flat], Platform::MacArm64)
                .unwrap()
                .unwrap()
                .tag,
            tag,
            "canonical full package must remain eligible"
        );
    }
    let mut future =
        release_for_platform(Product::Server, "v0.6.3", false, false, Platform::MacArm64);
    for asset in &mut future.assets {
        asset.name = asset.name.replace("AudioLibServer-v", "audiolib-server-");
        asset.browser_download_url = format!(
            "https://github.com/{REPOSITORY}/releases/download/v0.6.3/{}",
            asset.name
        );
    }
    assert!(
        client
            .select_for_platform(vec![future], Platform::MacArm64)
            .unwrap()
            .is_some(),
        "do not ban future lowercase flat packages"
    );
}

#[test]
fn historical_binary_only_mac_layout_cannot_bypass_public_package_validation() {
    let root = tempfile::tempdir().unwrap();
    let archive = root.path().join("legacy.tar.gz");
    let encoder = flate2::write::GzEncoder::new(
        std::fs::File::create(&archive).unwrap(),
        flate2::Compression::default(),
    );
    let mut tar = tar::Builder::new(encoder);
    for name in [
        "._audiolib-server-0.6.2-macos-arm64",
        "audiolib-server-0.6.2-macos-arm64/audiolib-server",
    ] {
        let mut header = tar::Header::new_ustar();
        header.set_path(name).unwrap();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, b"old".as_slice()).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
    let payload = root.path().join("payload");
    std::fs::create_dir(&payload).unwrap();
    assert!(package::extract(&archive, &payload, Product::Server, "legacy.tar.gz").is_err());
    assert!(!payload.join("audiolib-server").exists());
    assert!(!payload.join("audiolib-server.exe").exists());
}

#[test]
fn current_server_archives_require_the_platform_migrator_and_keep_unix_tools_executable() {
    let migrator = if cfg!(windows) {
        "audiolib-migrate.exe"
    } else {
        "audiolib-migrate"
    };
    let other_migrator = if cfg!(windows) {
        "audiolib-migrate"
    } else {
        "audiolib-migrate.exe"
    };
    for tar_archive in [false, true] {
        for complete in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let name = if tar_archive {
                "update.tar.gz"
            } else {
                "update.zip"
            };
            let archive = root.path().join(name);
            let mut files = package::required(Product::Server).to_vec();
            if !complete {
                files.retain(|name| *name != migrator);
                // An allowed executable for the other platform cannot stand in
                // for the migrator paired with this platform's server.
                files.push(other_migrator);
            }
            if tar_archive {
                let encoder = flate2::write::GzEncoder::new(
                    std::fs::File::create(&archive).unwrap(),
                    flate2::Compression::default(),
                );
                let mut tar = tar::Builder::new(encoder);
                for file in files {
                    let mut header = tar::Header::new_ustar();
                    header.set_path(file).unwrap();
                    header.set_size(7);
                    // Extraction owns the executable policy; do not rely on
                    // permissive input mode bits to make this test pass.
                    header.set_mode(0o644);
                    header.set_cksum();
                    tar.append(&header, b"fixture".as_slice()).unwrap();
                }
                tar.into_inner().unwrap().finish().unwrap();
            } else {
                let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
                for file in files {
                    zip.start_file(file, zip::write::SimpleFileOptions::default())
                        .unwrap();
                    zip.write_all(b"fixture").unwrap();
                }
                zip.finish().unwrap();
            }
            let payload = root.path().join("payload");
            std::fs::create_dir(&payload).unwrap();
            let result = package::extract(&archive, &payload, Product::Server, name);
            if complete {
                result.unwrap();
                for file in package::required(Product::Server) {
                    assert_eq!(std::fs::read(payload.join(file)).unwrap(), b"fixture");
                }
                #[cfg(unix)]
                if tar_archive {
                    use std::os::unix::fs::PermissionsExt;
                    for executable in ["audiolib-server", "audiolib-migrate"] {
                        assert_eq!(
                            std::fs::metadata(payload.join(executable))
                                .unwrap()
                                .permissions()
                                .mode()
                                & 0o777,
                            0o755
                        );
                    }
                }
            } else {
                let error = result.unwrap_err().to_string();
                assert!(
                    error.contains(&format!("Required update file is missing: {migrator}")),
                    "{error}"
                );
                assert!(!payload.join(migrator).exists());
            }
        }
    }
}

#[test]
fn staged_updates_exclude_another_updater_for_the_same_installation() {
    let root = tempfile::tempdir().unwrap();
    let first_stage = tempfile::Builder::new()
        .prefix(".audiolib-update-")
        .tempdir_in(root.path())
        .unwrap();
    let first = PreparedUpdate::new(first_stage, root.path(), NATIVE_PRODUCT).unwrap();
    let second_stage = tempfile::Builder::new()
        .prefix(".audiolib-update-")
        .tempdir_in(root.path())
        .unwrap();
    assert!(PreparedUpdate::new(second_stage, root.path(), NATIVE_PRODUCT).is_err());
    drop(first);
    let third_stage = tempfile::Builder::new()
        .prefix(".audiolib-update-")
        .tempdir_in(root.path())
        .unwrap();
    assert!(PreparedUpdate::new(third_stage, root.path(), NATIVE_PRODUCT).is_ok());
    assert!(root.path().join(".audiolib-update.lock").is_file());
}

#[test]
fn tar_pax_paths_are_validated_and_bounded() {
    for malicious in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("update.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            std::fs::File::create(&archive).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(encoder);
        if malicious {
            tar.append_pax_extensions([("path", b"../LICENSE".as_slice())])
                .unwrap();
        } else {
            tar.append_pax_extensions([("mtime", b"1700000000.12345".as_slice())])
                .unwrap();
        }
        for name in package::required(NATIVE_PRODUCT) {
            let mut header = tar::Header::new_ustar();
            header.set_path(name).unwrap();
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, b"new".as_slice()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        let payload = root.path().join("payload");
        std::fs::create_dir(&payload).unwrap();
        assert_eq!(
            package::extract(&archive, &payload, NATIVE_PRODUCT, "update.tar.gz").is_ok(),
            !malicious
        );
        assert!(!root.path().join("LICENSE").exists());
    }
}

#[test]
fn checksum_requires_exact_filename_and_single_line() {
    let good = format!("{}  expected.zip\n", "Ab".repeat(32));
    assert_eq!(
        package::parse_checksum(good.as_bytes(), "expected.zip").unwrap(),
        "ab".repeat(32)
    );
    assert!(package::parse_checksum(good.as_bytes(), "other.zip").is_err());
    assert!(package::parse_checksum(format!("{good}{good}").as_bytes(), "expected.zip").is_err());
    assert!(package::parse_checksum(b"abc expected.zip", "expected.zip").is_err());
}

#[test]
fn update_paths_cannot_target_runtime_data_or_traverse() {
    for name in [
        "../LICENSE",
        "/LICENSE",
        "C:/LICENSE",
        "doc/../LICENSE",
        "doc\\LICENSE",
        "settings.json",
        "library.db",
        "config/rust_settings.json",
        "licenses/evil.exe",
        "licenses/library.db-wal",
        "licenses/library.sqlite3-shm",
        "licenses/tokens.json",
        "licenses/NUL",
        "licenses/NUL.txt",
        "licenses/com1/LICENSE",
        "licenses/LPt9.md",
        "licenses/COM¹.txt",
        "licenses/notice?.txt",
        "LICENSE.",
        "doc//INDEX.md",
        "doc/INDEX.md:stream",
    ] {
        assert!(!package::allowed(Product::Desktop, name), "accepted {name}");
    }
    assert!(package::allowed(
        Product::Desktop,
        "config/rust_settings.example.json"
    ));
    assert!(package::allowed(
        Product::Desktop,
        "licenses/serde-1.0/LICENSE-MIT"
    ));
}

#[test]
fn installed_beta_marker_is_bound_to_binary_core_version() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("update-channel.txt"), "beta\n").unwrap();
    std::fs::write(root.path().join("update-version.txt"), "0.6.2-beta.12\n").unwrap();
    let client = UpdateClient::for_install(Product::Desktop, "0.6.2", root.path()).unwrap();
    assert_eq!(client.channel, Channel::Beta);
    assert_eq!(client.current.to_string(), "0.6.2-beta.12");
    assert!(UpdateClient::for_install(Product::Desktop, "0.6.1", root.path()).is_err());
}

#[test]
fn installed_development_markers_retain_identity_and_require_matching_binary_version() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("update-channel.txt"), "beta\n").unwrap();
    for (marker, development) in [
        ("0.8.6-0", true),
        ("0.8.6-0+local.0123456789ab", true),
        ("0.8.6", false),
        ("0.8.6-beta.2", false),
        ("0.8.6-rc.1", false),
        ("0.8.6-0.1", false),
        ("0.8.6+local.0123456789ab", false),
    ] {
        std::fs::write(
            root.path().join("update-version.txt"),
            format!("{marker}\n"),
        )
        .unwrap();
        for product in [Product::Desktop, Product::Server] {
            let client = UpdateClient::for_install(product, "0.8.6", root.path()).unwrap();
            assert_eq!(client.current_version(), marker);
            assert_eq!(client.is_development(), development, "{marker}");
            assert_eq!(client.channel(), Channel::Beta);
            assert!(UpdateClient::for_install(product, "0.8.5", root.path()).is_err());
        }
    }
}

#[test]
fn development_detection_does_not_change_server_release_selection() {
    let client =
        UpdateClient::new(Product::Server, Channel::Beta, "0.8.6-0+local.0123456789ab").unwrap();
    assert!(client.is_development());
    let selected = client
        .select_for_platform(
            vec![release_for_platform(
                Product::Server,
                "v0.8.6",
                false,
                false,
                Platform::WindowsX64,
            )],
            Platform::WindowsX64,
        )
        .unwrap()
        .unwrap();
    assert_eq!(selected.version, "0.8.6");
}

#[test]
fn zip_rejects_private_and_symlink_entries_before_deployment() {
    for (name, symlink) in [
        ("../LICENSE", false),
        ("settings.json", false),
        ("LICENSE", true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("update.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        if symlink {
            zip.add_symlink(name, "../secret", zip::write::SimpleFileOptions::default())
                .unwrap();
        } else {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"bad").unwrap();
        }
        zip.finish().unwrap();
        let payload = root.path().join("payload");
        std::fs::create_dir(&payload).unwrap();
        assert!(package::extract(&archive, &payload, Product::Desktop, "update.zip").is_err());
        assert!(!root.path().join("LICENSE").exists());
    }
}

#[test]
fn deployment_and_rollback_preserve_private_files() {
    for rollback in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("install");
        let stage = root.path().join("stage");
        std::fs::create_dir(&install).unwrap();
        std::fs::create_dir_all(stage.join("payload")).unwrap();
        for name in package::required(Product::Desktop) {
            std::fs::write(install.join(name), b"old").unwrap();
            std::fs::write(stage.join("payload").join(name), b"new").unwrap();
        }
        std::fs::write(install.join("settings.json"), b"user settings").unwrap();
        std::fs::write(install.join("library.db"), b"user database").unwrap();
        deploy::exercise_deploy(&install, &stage, rollback).unwrap();
        assert_eq!(
            std::fs::read(install.join("AudioLibExplorer.exe")).unwrap(),
            if rollback { b"old" } else { b"new" }
        );
        assert_eq!(
            std::fs::read(install.join("settings.json")).unwrap(),
            b"user settings"
        );
        assert_eq!(
            std::fs::read(install.join("library.db")).unwrap(),
            b"user database"
        );
    }
}

fn fixture_response(body: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut header = Vec::new();
        let mut byte = [0];
        while !header.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        let _ = stream.write_all(&body);
    });
    (format!("http://{address}/fixture"), thread)
}

#[test]
fn http_download_enforces_checksum_declared_size_and_response_bounds() {
    use sha2::{Digest, Sha256};
    let body = b"verified payload".to_vec();
    let good = format!("{:x}", Sha256::digest(&body));
    for (size, checksum, succeeds) in [
        (body.len() as u64, good.clone(), true),
        (body.len() as u64, "0".repeat(64), false),
        (body.len() as u64 - 1, good.clone(), false),
        (body.len() as u64 + 1, good, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (url, fixture) = fixture_response(body.clone());
        let mut client = UpdateClient::new(NATIVE_PRODUCT, Channel::Stable, "0.0.0").unwrap();
        // Only this private fixture replaces the TLS-only production agent.
        client.agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .into();
        let asset = Asset {
            name: "fixture.zip".into(),
            browser_download_url: url,
            size,
            digest: None,
        };
        assert_eq!(
            client
                .download(&asset, &root.path().join("archive"), &checksum)
                .is_ok(),
            succeeds
        );
        fixture.join().unwrap();
    }
    let (url, fixture) = fixture_response(vec![b'x'; 4097]);
    let mut client = UpdateClient::new(NATIVE_PRODUCT, Channel::Stable, "0.0.0").unwrap();
    client.agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into();
    assert!(client.fetch(&url, 4096, Duration::from_secs(5)).is_err());
    fixture.join().unwrap();
}

#[test]
fn newest_release_without_package_falls_back_to_older_valid_release() {
    let client = UpdateClient::new(NATIVE_PRODUCT, Channel::Stable, "0.6.1").unwrap();
    let mut newest = release("v0.6.3", false, false);
    newest.assets.clear();
    let update = client
        .select(vec![newest, release("v0.6.2", false, false)])
        .unwrap()
        .unwrap();
    assert_eq!(update.tag, "v0.6.2");
}

#[test]
fn releases_other_channels_ignore_stay_silent() {
    let client = UpdateClient::new(NATIVE_PRODUCT, Channel::Stable, "0.6.1").unwrap();
    let mut beta = release("v0.6.2-beta.1", false, true);
    beta.assets.clear();
    let mut old = release("v0.6.0", false, false);
    old.assets.clear();
    assert!(client.select(vec![beta, old]).unwrap().is_none());
    assert!(client.select(Vec::new()).unwrap().is_none());
}

/// Serves one canned JSON body per connection, in order, and records request targets.
fn mock_releases(bodies: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let mut targets = Vec::new();
        for body in bodies {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut header = Vec::new();
            let mut byte = [0];
            while !header.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            let line = String::from_utf8_lossy(&header);
            targets.push(line.split(' ').nth(1).unwrap_or_default().to_owned());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
        targets
    });
    (format!("http://{address}/repos/x/y/releases"), thread)
}

fn release_json(release: &Release) -> serde_json::Value {
    serde_json::json!({
        "tag_name": release.tag_name,
        "body": release.body,
        "draft": release.draft,
        "prerelease": release.prerelease,
        "assets": release.assets,
    })
}

fn list_json(releases: &[Release]) -> String {
    serde_json::Value::Array(releases.iter().map(release_json).collect()).to_string()
}

/// Client against a local plain-HTTP mock (only tests replace the HTTPS-only agent).
fn mock_client(url: &str, current: &str) -> UpdateClient {
    let mut client = UpdateClient::new(NATIVE_PRODUCT, Channel::Stable, current)
        .unwrap()
        .with_releases_api(url);
    client.agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into();
    client
}

#[test]
fn default_releases_api_is_unchanged() {
    let client = UpdateClient::new(NATIVE_PRODUCT, Channel::Stable, "0.6.1").unwrap();
    assert_eq!(client.releases_api(), RELEASES_API);
    assert_eq!(
        RELEASES_API,
        "https://api.github.com/repos/Freedragonlancer/AudioLib-Explorer/releases"
    );
}

#[test]
fn empty_first_page_is_retried_then_succeeds() {
    let (url, server) = mock_releases(vec![
        "[]".into(),
        list_json(&[release("v0.6.2", false, false)]),
    ]);
    let update = mock_client(&url, "0.6.1").check().unwrap().unwrap();
    assert_eq!(update.tag, "v0.6.2");
    assert!(update.skipped_newer.is_none());
    assert!(update.skipped_releases.is_empty());
    let targets = server.join().unwrap();
    assert_eq!(
        targets,
        [
            "/repos/x/y/releases?per_page=100&page=1",
            "/repos/x/y/releases?per_page=100&page=1"
        ]
    );
}

#[test]
fn empty_list_on_every_attempt_is_an_error_not_up_to_date() {
    let bodies = vec!["[]".to_string(); 1 + EMPTY_LIST_RETRIES as usize];
    let (url, server) = mock_releases(bodies);
    let error = mock_client(&url, "0.6.1").check().unwrap_err().to_string();
    assert!(error.contains("empty release list"), "{error}");
    assert_eq!(
        server.join().unwrap().len(),
        1 + EMPTY_LIST_RETRIES as usize
    );
}

#[test]
fn newer_release_without_package_is_an_error_naming_the_version() {
    let mut newest = release("v0.7.0", false, false);
    newest.assets.clear();
    let (url, server) = mock_releases(vec![list_json(&[newest, release("v0.6.0", false, false)])]);
    let error = mock_client(&url, "0.6.1").check().unwrap_err().to_string();
    assert!(error.contains("v0.7.0"), "{error}");
    assert!(error.contains("no valid package"), "{error}");
    server.join().unwrap();
}

#[test]
fn newer_release_without_package_falls_back_and_reports_the_skip() {
    let mut newest = release("v0.7.0", false, false);
    newest.assets.clear();
    let (url, server) = mock_releases(vec![list_json(&[newest, release("v0.6.2", false, false)])]);
    let update = mock_client(&url, "0.6.1").check().unwrap().unwrap();
    assert_eq!(update.tag, "v0.6.2");
    assert_eq!(
        update.skipped_releases,
        [SkippedRelease {
            tag: "v0.7.0".into(),
            reason: "no valid package for this platform".into(),
        }]
    );
    let skipped = update.skipped_newer.unwrap();
    assert!(
        skipped.contains("v0.7.0") && skipped.contains("v0.6.2"),
        "{skipped}"
    );
    server.join().unwrap();
}

#[test]
fn full_pages_continue_to_the_next_page() {
    let mut first: Vec<Release> = (0..100)
        .map(|n| release(&format!("v0.5.{n}"), false, false))
        .collect();
    first[0] = release("v0.6.2", false, false);
    let (url, server) = mock_releases(vec![
        list_json(&first),
        list_json(&[release("v0.6.3", false, false)]),
    ]);
    let update = mock_client(&url, "0.6.1").check().unwrap().unwrap();
    assert_eq!(update.tag, "v0.6.3");
    let targets = server.join().unwrap();
    assert_eq!(
        targets,
        [
            "/repos/x/y/releases?per_page=100&page=1",
            "/repos/x/y/releases?per_page=100&page=2"
        ]
    );
}
