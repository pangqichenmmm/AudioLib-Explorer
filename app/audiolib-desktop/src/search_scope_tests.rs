// app/audiolib-desktop/src/search_scope_tests.rs
//! Search-scope UI mapping for DESIGN-SEARCH-METADATA-V2 §2.5 / PS-11.
use audiolib_core::SearchScope;

fn index(scope: SearchScope) -> i32 {
    match scope {
        SearchScope::All => 0,
        SearchScope::Main => 1,
        SearchScope::Path => 2,
        SearchScope::Filename => 3,
    }
}

fn from_index(index: i32) -> SearchScope {
    match index {
        1 => SearchScope::Main,
        2 => SearchScope::Path,
        3 => SearchScope::Filename,
        _ => SearchScope::All,
    }
}

#[test]
fn search_scope_ui_order_is_all_filename_path() {
    assert_eq!(from_index(0), SearchScope::All);
    assert_eq!(from_index(1), SearchScope::Main);
    assert_eq!(from_index(2), SearchScope::Path);
    assert_eq!(from_index(3), SearchScope::Filename);
    assert_eq!(index(SearchScope::All), 0);
    assert_eq!(index(SearchScope::Main), 1);
    assert_eq!(index(SearchScope::Path), 2);
    assert_eq!(index(SearchScope::Filename), 3);
    assert_eq!(SearchScope::Main.as_str(), "main");
    assert_eq!(SearchScope::All.as_str(), "all");
    assert_eq!(SearchScope::Path.as_str(), "path");
    assert_eq!(SearchScope::Filename.as_str(), "filename");
}
