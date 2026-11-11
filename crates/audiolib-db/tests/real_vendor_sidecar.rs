// crates/audiolib-db/tests/real_vendor_sidecar.rs
//! Explicit real-table acceptance using only owned copies, generated WAVs and public product APIs.
#![cfg(feature = "scanner")]

use audiolib_core::{
    DataProvider, Error, MetadataPatch, ScanState, SearchRequest, SearchScope,
    vendor_import::{VendorImportOptions, VendorImportPreview, VendorLimits, VendorSheetPreview},
};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};
use unicode_normalization::UnicodeNormalization;

const EMBEDDED: &str = "ownedrealsampleembedded";
const USER_TAG: &str = "ownedrealsampleusertag";
const USER_NOTE: &str = "ownedrealsampleusernote";

fn hash(path: &Path) -> [u8; 32] {
    let file = File::open(path).expect("Owned fixture must remain readable");
    let limit = VendorLimits::default().max_file_bytes;
    assert!(
        file.metadata().unwrap().len() <= limit,
        "Owned fixture exceeds the product table-size budget"
    );
    let mut file = file.take(limit + 1);
    let mut hasher = Sha256::new();
    let mut bytes = [0_u8; 65_536];
    let mut total = 0_u64;
    loop {
        let count = file.read(&mut bytes).expect("Owned fixture read failed");
        if count == 0 {
            return hasher.finalize().into();
        }
        total += count as u64;
        assert!(total <= limit, "Owned fixture grew beyond its size budget");
        hasher.update(&bytes[..count]);
    }
}

fn wav(path: &Path) {
    let samples = [0_u8; 256];
    let mut bext = [0_u8; 602];
    bext[..EMBEDDED.len()].copy_from_slice(EMBEDDED.as_bytes());
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(36_u32 + 8 + bext.len() as u32 + samples.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&8_000_u32.to_le_bytes());
    bytes.extend_from_slice(&16_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"bext");
    bytes.extend_from_slice(&(bext.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&bext);
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&samples);
    // The name came from the table, but an existing owned file is never overwritten.
    use std::io::Write;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .expect("Generated fixture filename collided")
        .write_all(&bytes)
        .expect("Cannot create owned tiny WAV");
}

fn plain_wav_name(value: &str) -> Option<String> {
    let name = value.trim();
    if name.is_empty()
        || name.len() > 240
        || name.starts_with('.')
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        || !Path::new(name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
    {
        return None;
    }
    let stem = Path::new(name).file_stem()?.to_str()?;
    let device = stem.split('.').next()?.to_ascii_uppercase();
    if stem.is_empty()
        || matches!(
            device.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
        )
        || (device.chars().count() == 4
            && (device.starts_with("COM") || device.starts_with("LPT"))
            && device
                .chars()
                .nth(3)
                .is_some_and(|character| matches!(character, '0'..='9' | '¹' | '²' | '³')))
    {
        return None;
    }
    Some(name.into())
}

struct Candidate {
    name: String,
    values: BTreeMap<String, String>,
}

fn mapped_cell<'a>(
    sheet: &VendorSheetPreview,
    cells: &'a [String],
    header: &str,
) -> Option<&'a str> {
    sheet
        .headers
        .iter()
        .position(|candidate| candidate == header)
        .and_then(|index| cells.get(index))
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
}

/// Inspect the product's owned staging data only; no business SQL is inserted or updated.
fn candidates(database: &Path, preview: &VendorImportPreview) -> Vec<Candidate> {
    let name = database.file_name().unwrap().to_string_lossy();
    let stage = database
        .with_file_name(format!("{name}.vendor-import"))
        .join(&preview.import_id)
        .join("preview.db");
    let connection = Connection::open_with_flags(stage, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("Product preview staging must be readable");
    let mut statement = connection
        .prepare("SELECT source_file,sheet,cells FROM rows ORDER BY ordinal LIMIT 64")
        .expect("Product preview must expose its owned raw row staging");
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("Cannot read bounded owned staging rows");
    let mut selected = Vec::new();
    let mut names = BTreeSet::new();
    for row in rows {
        let (source, sheet_name, encoded) = row.expect("Owned staging row is invalid");
        let cells: Vec<String> =
            serde_json::from_str(&encoded).expect("Product staging cells must be JSON");
        let Some(sheet) = preview
            .sheets
            .iter()
            .find(|sheet| sheet.source_file == source && sheet.sheet == sheet_name)
        else {
            continue;
        };
        let identity = sheet
            .mapping
            .filename
            .iter()
            .chain(sheet.mapping.relative_path.iter())
            .filter_map(|header| mapped_cell(sheet, &cells, header))
            .find_map(plain_wav_name);
        let Some(name) = identity else { continue };
        let values = sheet
            .mapping
            .fields
            .iter()
            .filter_map(|(field, header)| {
                mapped_cell(sheet, &cells, header).map(|value| (field.clone(), value.into()))
            })
            .collect::<BTreeMap<_, _>>();
        if values.is_empty() || !names.insert(name.to_lowercase()) {
            continue;
        }
        selected.push(Candidate { name, values });
        if selected.len() == 3 {
            break;
        }
    }
    selected
}

fn main_request(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        scope: SearchScope::Main,
        ..Default::default()
    }
}

fn query_has(provider: &SqliteProvider, query: &str, id: i64) -> bool {
    provider
        .search(&main_request(query))
        .unwrap_or_else(|error| {
            let kind = match error {
                Error::Validation(_) => "Validation",
                Error::Database(_) => "Database",
                Error::Audio(_) => "Audio",
                Error::Io(_) => "Io",
                Error::Cancelled => "Cancelled",
                Error::Busy(_) => "Busy",
                Error::Migration { .. } => "Migration",
                Error::NotFound(_) => "NotFound",
                Error::NewerDatabase { .. } => "NewerDatabase",
            };
            panic!(
                "Product Main search failed: error_kind={kind}, term_sha256={:x}",
                Sha256::digest(query.as_bytes())
            );
        })
        .results
        .iter()
        .any(|file| file.id == id)
}

struct Expected {
    id: i64,
    path: PathBuf,
    hash: [u8; 32],
    modified: SystemTime,
    values: BTreeMap<String, String>,
    terms: BTreeMap<String, String>,
}

fn normalized(value: &str) -> String {
    value
        .nfc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn field_values(field: &str, value: &str) -> Vec<String> {
    if field == "keywords" {
        value
            .split([',', ';', '|', '\r', '\n', '，', '；', '、'])
            .map(normalized)
            .filter(|value| !value.is_empty())
            .collect()
    } else {
        vec![normalized(value)]
    }
}

fn assert_preserved(provider: &SqliteProvider, expected: &[Expected]) {
    for item in expected {
        let file = provider.get_audio(item.id).expect("Owned WAV disappeared");
        assert!(file.tags == USER_TAG, "User tags changed");
        assert!(file.notes == USER_NOTE, "User note changed");
        assert!(file.favorite, "User favorite changed");
        assert!(
            file.bext_description == EMBEDDED,
            "Embedded description changed"
        );
        let fields = &file.attributes["vendor_metadata"]["fields"];
        assert!(
            item.values.iter().any(|(field, original)| {
                let Some(projected) = fields[field].as_array() else {
                    return false;
                };
                let actual = projected
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .flat_map(|value| field_values(field, value))
                    .collect::<BTreeSet<_>>();
                field_values(field, original)
                    .iter()
                    .any(|value| !value.is_empty() && actual.contains(value))
            }),
            "Automatic vendor fields must contain actual mapped values, even when filename words already match"
        );
        assert_eq!(hash(&item.path), item.hash, "Owned WAV bytes changed");
        assert_eq!(
            fs::metadata(&item.path).unwrap().modified().unwrap(),
            item.modified,
            "Owned WAV modification time changed"
        );
        for term in item.terms.values() {
            assert!(
                query_has(provider, term, item.id),
                "Mapped table Metadata did not participate in Main search"
            );
        }
    }
}

#[test]
#[ignore = "requires AUDIOLIB_VENDOR_TABLE_FIXTURE pointing at a separately owned real table copy"]
fn real_vendor_sidecar_uses_product_parser_and_automatic_scan() {
    let source = PathBuf::from(
        env::var_os("AUDIOLIB_VENDOR_TABLE_FIXTURE")
            .expect("Set AUDIOLIB_VENDOR_TABLE_FIXTURE to an owned table copy"),
    );
    assert!(
        source.is_file(),
        "Owned table fixture is not a regular file"
    );
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .expect("Owned table needs a supported extension");
    assert!(
        matches!(extension.as_str(), "csv" | "tsv" | "xls" | "xlsx"),
        "Owned table extension is unsupported"
    );
    let source_hash = hash(&source);
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("input");
    fs::create_dir(&input).unwrap();
    let owned_table = input.join(format!("catalog.{extension}"));
    fs::copy(&source, &owned_table).expect("Cannot copy read-only source into owned staging");
    assert_eq!(
        hash(&owned_table),
        source_hash,
        "Owned copy identity differs"
    );
    let parse_database = root.path().join("parse.db");
    let parser = SqliteProvider::open(&parse_database).unwrap();
    assert_eq!(parser.stats().unwrap().audio_files, 0);
    let preview = parser
        .vendor_import_preview_files_impl(
            std::slice::from_ref(&owned_table),
            &VendorImportOptions::default(),
        )
        .unwrap_or_else(|_| panic!("Product parser could not preview the owned real table"));
    assert_eq!(preview.files.len(), 1);
    assert!(
        preview.files.iter().all(|file| file.error.is_none()),
        "Product parser rejected the owned table"
    );
    assert!(preview.rows > 0, "Product parser found no data rows");
    assert!(
        preview.sheets.iter().any(|sheet| {
            (sheet.mapping.filename.is_some() || sheet.mapping.relative_path.is_some())
                && !sheet.mapping.fields.is_empty()
                && sheet.rows > 0
        }),
        "Real table must expose recognized identity and Metadata headers"
    );
    assert_eq!(
        parser.stats().unwrap().audio_files,
        0,
        "Preview wrote library records"
    );
    let selected = candidates(&parse_database, &preview);
    assert!(
        !selected.is_empty(),
        "First 64 product rows contain no safely matchable plain WAV name with Metadata"
    );
    let audio = root.path().join("audio");
    fs::create_dir(&audio).unwrap();
    for candidate in &selected {
        wav(&audio.join(&candidate.name));
    }
    let provider = Arc::new(SqliteProvider::open(&root.path().join("scan.db")).unwrap());
    let service = ScanService::new(provider.clone());
    let options = ScanOptions {
        workers: 1,
        ..Default::default()
    };
    let first = service
        .scan(&audio, &options, &ScanCancellation::default(), |_| {})
        .unwrap();
    assert_eq!(first.state, ScanState::Completed);
    assert_eq!(first.errors, 0);
    assert_eq!(first.added, selected.len() as u64);
    let files = provider.search(&SearchRequest::default()).unwrap().results;
    let mut expected = Vec::new();
    let mut rejected_nonready_terms = 0_u64;
    for candidate in selected {
        let file = files
            .iter()
            .find(|file| file.filename == candidate.name)
            .expect("Generated WAV was not scanned");
        provider
            .update_metadata(
                file.id,
                &MetadataPatch {
                    tags: Some(USER_TAG.into()),
                    notes: Some(USER_NOTE.into()),
                    favorite: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        let terms = candidate
            .values
            .iter()
            .filter_map(|(field, value)| {
                let terms = value
                    .split(|character: char| !character.is_alphanumeric())
                    .filter(|term| (1..=64).contains(&term.chars().count()))
                    .take(32)
                    .filter(|term| {
                        let ready = main_request(term).validate().is_ok();
                        if !ready {
                            rejected_nonready_terms += 1;
                        }
                        ready
                    })
                    .collect::<Vec<_>>();
                terms
                    .iter()
                    .copied()
                    .find(|term| !query_has(&provider, term, file.id))
                    .or_else(|| terms.first().copied())
                    .map(|term| (field.clone(), term.to_string()))
            })
            .collect::<BTreeMap<_, _>>();
        assert!(
            !terms.is_empty(),
            "Selected real row has no searchable Metadata term"
        );
        let path = audio.join(candidate.name);
        expected.push(Expected {
            id: file.id,
            hash: hash(&path),
            modified: fs::metadata(&path).unwrap().modified().unwrap(),
            path,
            values: candidate.values,
            terms,
        });
    }
    let scan_table = audio.join(format!("catalog.{extension}"));
    fs::copy(&owned_table, &scan_table).unwrap();
    let scanned = service
        .scan(&audio, &options, &ScanCancellation::default(), |_| {})
        .unwrap_or_else(|_| panic!("Automatic scan of the owned real table failed"));
    assert_eq!(scanned.state, ScanState::Completed);
    assert_eq!(scanned.errors, 0);
    assert_eq!(scanned.added, 0);
    assert_eq!(scanned.updated, 0, "No WAV header changed");
    assert_eq!(scanned.unchanged, expected.len() as u64);
    assert_eq!(scanned.vendor_metadata.tables, 1);
    assert!(scanned.vendor_metadata.rows > 0);
    assert!(scanned.vendor_metadata.matched >= expected.len() as u64);
    assert!(scanned.vendor_metadata.updated > 0);
    assert_preserved(&provider, &expected);
    let repeated = service
        .scan(&audio, &options, &ScanCancellation::default(), |_| {})
        .unwrap_or_else(|_| panic!("Repeated scan of the owned real table failed"));
    assert_eq!(repeated.state, ScanState::Completed);
    assert_eq!(repeated.errors, 0);
    assert_eq!(repeated.added, 0);
    assert_eq!(repeated.updated, 0);
    assert_eq!(repeated.unchanged, expected.len() as u64);
    assert_eq!(
        repeated.vendor_metadata.updated, 0,
        "Repeated table scan must be idempotent"
    );
    assert_preserved(&provider, &expected);
    assert_eq!(hash(&owned_table), source_hash);
    assert_eq!(hash(&scan_table), source_hash);
    assert_eq!(hash(&source), source_hash, "Read-only source table changed");
    let fields = expected
        .iter()
        .flat_map(|item| item.terms.keys().cloned())
        .collect::<BTreeSet<_>>();
    println!(
        "real_vendor_sidecar rows={} matched={} generated={} sheets={} fields={} repeat_metadata_updated={} rejected_nonready_terms={}",
        scanned.vendor_metadata.rows,
        scanned.vendor_metadata.matched,
        expected.len(),
        preview.sheets.len(),
        fields.into_iter().collect::<Vec<_>>().join(","),
        repeated.vendor_metadata.updated,
        rejected_nonready_terms
    );
}
