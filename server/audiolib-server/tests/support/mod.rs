// server/audiolib-server/tests/support/mod.rs
//! Explicit modern-client negotiation for normal router regression tests.
use audiolib_core::compatibility::{
    ACCEPT_FORMATS_HEADER, CLIENT_COMPATIBILITY_HEADER, PLAN_B_FORMAT, compatibility_manifest,
};
use axum::http::request::Builder;

pub fn modern(request: Builder) -> Builder {
    // Exercise the deprecated playback endpoints as actual headerless clients.
    // Their compatibility contract must not depend on normal mutation admission.
    if request
        .uri_ref()
        .is_some_and(|uri| uri.path().ends_with("/plays"))
    {
        return request;
    }
    request
        .header(ACCEPT_FORMATS_HEADER, PLAN_B_FORMAT)
        .header(
            CLIENT_COMPATIBILITY_HEADER,
            serde_json::to_string(&compatibility_manifest()).unwrap(),
        )
        .header(
            "x-audiolib-snapshot-schema-max",
            audiolib_db::MAX_SNAPSHOT_SCHEMA.to_string(),
        )
        .header("x-audiolib-search-storage-max", "1")
        .header("x-audiolib-tokenization-max", "3")
}
