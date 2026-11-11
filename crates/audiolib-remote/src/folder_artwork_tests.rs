// crates/audiolib-remote/src/folder_artwork_tests.rs
//! Direct-folder artwork negotiation and representation identity boundaries.
use super::*;
use crate::test_server::{TestResponse, TestServer};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn capabilities() -> Capabilities {
    Capabilities {
        compatibility: Some(audiolib_core::compatibility::compatibility_manifest()),
        ..Default::default()
    }
}

fn state(source_schema: u32) -> crate::CacheState {
    let local = audiolib_core::compatibility::LibraryFormat::target();
    let mut source = local.clone();
    source.schema_version = source_schema;
    if source_schema < 8 {
        source.features.remove("album_entities");
    }
    crate::CacheState {
        library_id: "library".into(),
        schema_version: 8,
        format: Some(Box::new(crate::CacheFormat {
            source: Some(source),
            local,
            generation: 1,
        })),
        ..Default::default()
    }
}

fn response(status: u16) -> TestResponse {
    let mut response = TestResponse::bytes(
        status,
        if status == 304 {
            Vec::new()
        } else {
            b"\x89PNG\r\n\x1a\nfixture".to_vec()
        },
    )
    .with_header("X-Library-Id", "library")
    .with_header("X-Album-Id", "11")
    .with_header("X-Album-Namespace", "namespace")
    .with_header("ETag", "\"folder-cover\"");
    response.content_type = "image/png";
    response
}

#[test]
fn folder_artwork_requires_its_exact_capability_and_album_identity_protocol() {
    assert!(!Capabilities::default().album_folder_artwork_usable());
    assert!(!Capabilities::unknown().album_folder_artwork_usable());
    assert!(capabilities().album_folder_artwork_usable());
    for missing in ["album_folder_artwork", "album_entity_artwork"] {
        let mut capability = capabilities();
        capability
            .compatibility
            .as_mut()
            .unwrap()
            .features
            .remove(missing);
        assert!(!capability.album_folder_artwork_usable());
    }
    let mut future = capabilities();
    future
        .compatibility
        .as_mut()
        .unwrap()
        .features
        .insert("album_folder_artwork".into(), 2);
    assert!(!future.album_folder_artwork_usable());
}

#[test]
fn folder_transport_binds_signed_folder_context_in_200_and_304() {
    let server = TestServer::start(|request| {
        assert_eq!(
            request.path,
            "/api/v1/albums/11/artwork?folder_id=-7&side=40"
        );
        assert_eq!(request.header("authorization"), Some("Bearer token"));
        assert_eq!(request.header("x-audiolib-library-id"), Some("library"));
        assert_eq!(
            request.header("x-audiolib-album-namespace"),
            Some("namespace")
        );
        response(if request.header("if-none-match").is_some() {
            304
        } else {
            200
        })
        .with_header("X-Album-Folder-Id", "-7")
    });
    let client = RemoteClient::new(&server.url, "token").unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        client
            .album_artwork_in_folder_for_library(
                "library",
                "namespace",
                11,
                -7,
                Some(40),
                None,
                &cancel
            )
            .unwrap(),
        ArtworkReply::Data { .. }
    ));
    assert_eq!(
        client
            .album_artwork_in_folder_for_library(
                "library",
                "namespace",
                11,
                -7,
                Some(40),
                Some("\"folder-cover\""),
                &cancel
            )
            .unwrap(),
        ArtworkReply::NotModified
    );
    assert!(matches!(
        client
            .album_artwork_in_folder_for_snapshot(
                &state(8),
                &capabilities(),
                "namespace",
                11,
                -7,
                &[999],
                Some(40),
                None,
                &cancel
            )
            .unwrap(),
        ArtworkReply::Data { .. }
    ));
}

#[test]
fn folder_transport_rejects_ignored_query_wrong_or_duplicate_identity_before_200_or_304() {
    for status in [200, 304] {
        for invalid in 0..7 {
            let server = TestServer::start(move |_| {
                let mut reply = response(status);
                match invalid {
                    0 => reply,
                    1 => reply.with_header("X-Album-Folder-Id", "8"),
                    2 => reply
                        .with_header("X-Album-Folder-Id", "7")
                        .with_header("X-Album-Folder-Id", "7"),
                    3 => reply.with_header("X-Album-Folder-Id", "07"),
                    _ => {
                        let header = match invalid {
                            4 => "X-Library-Id",
                            5 => "X-Album-Id",
                            _ => "X-Album-Namespace",
                        };
                        reply
                            .headers
                            .retain(|(key, _)| !key.eq_ignore_ascii_case(header));
                        reply
                            .with_header(header, "different")
                            .with_header("X-Album-Folder-Id", "7")
                    }
                }
            });
            assert!(
                matches!(
                    RemoteClient::new(&server.url, "token")
                        .unwrap()
                        .album_artwork_in_folder_for_library(
                            "library",
                            "namespace",
                            11,
                            7,
                            None,
                            Some("\"folder-cover\""),
                            &AtomicBool::new(false)
                        ),
                    Err(RemoteError::LibraryChanged(_))
                ),
                "status={status} case={invalid}"
            );
        }
    }
}

#[test]
fn native_snapshot_without_folder_capability_requires_upgrade_without_http_downgrade() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let server = TestServer::start(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
        TestResponse::bytes(404, Vec::new())
    });
    let client = RemoteClient::new(&server.url, "token").unwrap();
    for revision in [None, Some(2)] {
        let mut capability = capabilities();
        let features = &mut capability.compatibility.as_mut().unwrap().features;
        features.remove("album_folder_artwork");
        if let Some(revision) = revision {
            features.insert("album_folder_artwork".into(), revision);
        }
        let failure = client
            .album_artwork_in_folder_for_snapshot(
                &state(8),
                &capability,
                "namespace",
                11,
                7,
                &[1, 2],
                None,
                None,
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert!(matches!(&failure, RemoteError::Protocol(message) if message.contains("upgrade")));
    }
    let mut without_provenance = state(8);
    without_provenance.format = None;
    let mut capability = capabilities();
    capability
        .compatibility
        .as_mut()
        .unwrap()
        .features
        .remove("album_folder_artwork");
    assert!(matches!(
        client.album_artwork_in_folder_for_snapshot(
            &without_provenance,
            &capability,
            "namespace",
            11,
            7,
            &[1, 2],
            None,
            None,
            &AtomicBool::new(false)
        ),
        Err(RemoteError::Protocol(_))
    ));
    assert!(matches!(
        client.album_artwork_in_directory_for_snapshot(
            &state(8),
            &capability,
            "namespace",
            11,
            None,
            "",
            &[1, 2],
            None,
            None,
            &AtomicBool::new(false)
        ),
        Err(RemoteError::Protocol(_))
    ));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn legacy_snapshot_folder_transport_uses_only_supplied_direct_folder_members() {
    let paths = Arc::new(Mutex::new(Vec::new()));
    let observed = paths.clone();
    let server = TestServer::start(move |request| {
        assert_eq!(request.header("x-audiolib-album-namespace"), None);
        observed.lock().unwrap().push(request.path.clone());
        if request.path == "/api/v1/audio/1/artwork?side=40" {
            TestResponse::bytes(404, Vec::new())
        } else {
            assert_eq!(request.path, "/api/v1/audio/2/artwork?side=40");
            response(200)
        }
    });
    let mut capability = capabilities();
    capability
        .compatibility
        .as_mut()
        .unwrap()
        .features
        .remove("album_folder_artwork");
    let reply = RemoteClient::new(&server.url, "token")
        .unwrap()
        .album_artwork_in_folder_for_snapshot(
            &state(7),
            &capability,
            "local-only-namespace",
            987654,
            765432,
            &[2, 1, 2],
            Some(40),
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(matches!(reply, ArtworkReply::Data { .. }));
    assert!(matches!(
        RemoteClient::new(&server.url, "token")
            .unwrap()
            .album_artwork_in_directory_for_snapshot(
                &state(7),
                &capability,
                "local-only-namespace",
                987654,
                None,
                "/local/path/is/not/sent",
                &[2, 1],
                Some(40),
                None,
                &AtomicBool::new(false)
            )
            .unwrap(),
        ArtworkReply::Data { .. }
    ));
    assert_eq!(
        *paths.lock().unwrap(),
        [
            "/api/v1/audio/1/artwork?side=40",
            "/api/v1/audio/2/artwork?side=40",
            "/api/v1/audio/1/artwork?side=40",
            "/api/v1/audio/2/artwork?side=40"
        ]
    );
}

#[test]
fn global_album_transport_remains_usable_without_the_new_folder_feature() {
    let server = TestServer::start(|request| {
        assert_eq!(request.path, "/api/v1/albums/11/artwork");
        response(200)
    });
    let mut capability = capabilities();
    capability
        .compatibility
        .as_mut()
        .unwrap()
        .features
        .remove("album_folder_artwork");
    assert!(matches!(
        RemoteClient::new(&server.url, "token")
            .unwrap()
            .album_artwork_for_snapshot(
                &state(8),
                &capability,
                "namespace",
                11,
                &[],
                None,
                None,
                &AtomicBool::new(false)
            )
            .unwrap(),
        ArtworkReply::Data { .. }
    ));
}

#[test]
fn unassigned_directory_transport_percent_encodes_literals_and_verifies_utf8_hex_on_200_and_304() {
    let directory = "/FX/%_[x]&+ 金属\\literal";
    let expected_hex = directory
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let echoed = expected_hex.clone();
    let server = TestServer::start(move |request| {
        assert_eq!(
            request.path,
            "/api/v1/albums/11/artwork?unassigned_directory=%2FFX%2F%25_%5Bx%5D%26%2B%20%E9%87%91%E5%B1%9E%5Cliteral&side=40"
        );
        assert_eq!(
            request.header("x-audiolib-album-namespace"),
            Some("namespace")
        );
        response(if request.header("if-none-match").is_some() {
            304
        } else {
            200
        })
        .with_header("X-Album-Directory-Identity", &echoed)
    });
    let client = RemoteClient::new(&server.url, "token").unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        client
            .album_artwork_in_unassigned_directory_for_library(
                "library",
                "namespace",
                11,
                directory,
                Some(40),
                None,
                &cancel
            )
            .unwrap(),
        ArtworkReply::Data { .. }
    ));
    assert_eq!(
        client
            .album_artwork_in_unassigned_directory_for_library(
                "library",
                "namespace",
                11,
                directory,
                Some(40),
                Some("\"folder-cover\""),
                &cancel
            )
            .unwrap(),
        ArtworkReply::NotModified
    );
    assert!(matches!(
        client
            .album_artwork_in_directory_for_snapshot(
                &state(8),
                &capabilities(),
                "namespace",
                11,
                None,
                directory,
                &[],
                Some(40),
                None,
                &cancel
            )
            .unwrap(),
        ArtworkReply::Data { .. }
    ));
}

#[test]
fn unassigned_empty_and_root_contexts_are_distinct_and_directory_budget_is_utf8_bytes() {
    for (directory, query, echo) in [("", "", ""), ("/", "%2F", "2f"), ("C:/", "c%3A", "633a")] {
        let expected = format!("/api/v1/albums/11/artwork?unassigned_directory={query}");
        let server = TestServer::start(move |request| {
            assert_eq!(request.path, expected);
            response(200).with_header("X-Album-Directory-Identity", echo)
        });
        assert!(matches!(
            RemoteClient::new(&server.url, "token")
                .unwrap()
                .album_artwork_in_unassigned_directory_for_library(
                    "library",
                    "namespace",
                    11,
                    directory,
                    None,
                    None,
                    &AtomicBool::new(false)
                )
                .unwrap(),
            ArtworkReply::Data { .. }
        ));
    }
    let client = RemoteClient::new("http://127.0.0.1:9", "token").unwrap();
    for oversized in ["x".repeat(4097), "界".repeat(1366)] {
        assert!(matches!(
            client.album_artwork_in_unassigned_directory_for_library(
                "library",
                "namespace",
                11,
                &oversized,
                None,
                None,
                &AtomicBool::new(false)
            ),
            Err(RemoteError::Validation(_))
        ));
    }
    let directory = "x".repeat(4096);
    let path = format!("/api/v1/albums/11/artwork?unassigned_directory={directory}");
    let server = TestServer::start(move |request| {
        assert_eq!(request.path, path);
        response(200).with_header("X-Album-Directory-Identity", &"78".repeat(4096))
    });
    assert!(matches!(
        RemoteClient::new(&server.url, "token")
            .unwrap()
            .album_artwork_in_unassigned_directory_for_library(
                "library",
                "namespace",
                11,
                &directory,
                None,
                None,
                &AtomicBool::new(false)
            )
            .unwrap(),
        ArtworkReply::Data { .. }
    ));
}

#[test]
fn unassigned_transport_rejects_missing_wrong_duplicate_and_noncanonical_hex_on_empty_304() {
    let directory = "/FX/leaf";
    let expected = directory
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    for status in [200, 304] {
        for invalid in 0..6 {
            let hex = expected.clone();
            let server = TestServer::start(move |_| match invalid {
                0 => response(status),
                1 => response(status).with_header("X-Album-Directory-Identity", "2f6f74686572"),
                2 => response(status)
                    .with_header("X-Album-Directory-Identity", &hex)
                    .with_header("X-Album-Directory-Identity", &hex),
                3 => {
                    response(status).with_header("X-Album-Directory-Identity", &hex.to_uppercase())
                }
                4 => response(status).with_header("X-Album-Directory-Identity", "0"),
                _ => response(status).with_header("X-Album-Folder-Id", "7"),
            });
            assert!(
                matches!(
                    RemoteClient::new(&server.url, "token")
                        .unwrap()
                        .album_artwork_in_unassigned_directory_for_library(
                            "library",
                            "namespace",
                            11,
                            directory,
                            None,
                            Some("\"folder-cover\""),
                            &AtomicBool::new(false)
                        ),
                    Err(RemoteError::LibraryChanged(_))
                ),
                "status={status} case={invalid}"
            );
        }
    }
}

#[test]
fn generic_registered_folder_checks_actual_directory_even_for_an_empty_304_body() {
    for status in [200, 304] {
        for changed in [false, true] {
            let directory = if changed { "/FX/After" } else { "/FX/Before" };
            let hex = directory
                .bytes()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let server = TestServer::start(move |request| {
                assert_eq!(request.path, "/api/v1/albums/11/artwork?folder_id=7");
                response(status)
                    .with_header("X-Album-Folder-Id", "7")
                    .with_header("X-Album-Directory-Identity", &hex)
            });
            let result = RemoteClient::new(&server.url, "token")
                .unwrap()
                .album_artwork_in_directory_for_snapshot(
                    &state(8),
                    &capabilities(),
                    "namespace",
                    11,
                    Some(7),
                    "/FX/Before",
                    &[],
                    None,
                    Some("\"folder-cover\""),
                    &AtomicBool::new(false),
                );
            if changed {
                assert!(
                    matches!(result, Err(RemoteError::LibraryChanged(_))),
                    "status={status}"
                );
            } else if status == 304 {
                assert_eq!(result.unwrap(), ArtworkReply::NotModified);
            } else {
                assert!(matches!(result.unwrap(), ArtworkReply::Data { .. }));
            }
        }
    }
}
