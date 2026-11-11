// crates/audiolib-db/src/vendor_import/matching.rs
//! Indexed matching against an owned, fixed library projection; suggestions require confirmation.
use super::{Result, db_error, invalid, json};
use audiolib_core::vendor_import::{
    VendorCandidatePage, VendorColumnMapping, VendorImportOptions, VendorMatchCandidate,
    VendorRowPreview,
};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use unicode_normalization::UnicodeNormalization;

pub(super) fn fold(value: &str) -> String {
    let portable = value.replace('\\', "/");
    let portable = if let Some(unc) = portable.strip_prefix("//?/UNC/") {
        format!("//{unc}")
    } else {
        portable
            .strip_prefix("//?/")
            .unwrap_or(&portable)
            .to_owned()
    };
    portable
        .chars()
        .map(|c| match c {
            '\u{ff01}'..='\u{ff5e}' => char::from_u32(c as u32 - 0xfee0).unwrap_or(c),
            '\u{3000}' => ' ',
            _ => c,
        })
        .collect::<String>()
        .nfc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .trim_end_matches('/')
        .to_owned()
}

pub(super) fn basename(value: &str) -> String {
    fold(value)
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned()
}
pub(super) fn stem(value: &str) -> String {
    basename(value)
        .rsplit_once('.')
        .map_or_else(|| basename(value), |(s, _)| s.to_owned())
}
pub(super) fn fuzzy(value: &str) -> String {
    stem(value)
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '_')
        .collect()
}

pub(super) fn build_index(stage: &mut Connection, library: &Connection) -> Result<()> {
    stage.execute_batch("CREATE TABLE IF NOT EXISTS candidates(id INTEGER PRIMARY KEY,path TEXT NOT NULL,display_path TEXT NOT NULL,name TEXT NOT NULL,stem TEXT NOT NULL,fuzzy TEXT NOT NULL,user_conflict INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS roots(path TEXT PRIMARY KEY); DELETE FROM candidate_set_members; DELETE FROM candidate_sets; DELETE FROM candidates; DELETE FROM roots;").map_err(db_error)?;
    let tx = stage.transaction().map_err(db_error)?;
    {
        let mut select = library.prepare("SELECT id,file_path,filename,(coalesce(user_tags,'')<>'' OR coalesce(user_notes,'')<>'' OR coalesce(user_rating,0)<>0) FROM audio_files").map_err(db_error)?;
        // User columns are observed only to explain conflicts, never used as import destinations.
        let rows = select
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, bool>(3)?,
                ))
            })
            .map_err(db_error)?;
        let mut insert = tx.prepare("INSERT INTO candidates(id,path,display_path,name,stem,fuzzy,user_conflict) VALUES(?1,?2,?3,?4,?5,?6,?7)").map_err(db_error)?;
        for row in rows {
            let (id, path, name, conflict) = row.map_err(db_error)?;
            insert
                .execute(params![
                    id,
                    fold(&path),
                    path,
                    basename(&name),
                    stem(&name),
                    fuzzy(&name),
                    conflict
                ])
                .map_err(db_error)?;
        }
        let mut roots = library
            .prepare("SELECT folder_path FROM folders WHERE parent_id IS NULL")
            .map_err(db_error)?;
        for root in roots
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(db_error)?
        {
            tx.execute(
                "INSERT OR IGNORE INTO roots(path) VALUES(?1)",
                [fold(&root.map_err(db_error)?)],
            )
            .map_err(db_error)?;
        }
    }
    tx.execute_batch("CREATE INDEX IF NOT EXISTS candidate_path ON candidates(path); CREATE INDEX IF NOT EXISTS candidate_name ON candidates(name); CREATE INDEX IF NOT EXISTS candidate_stem ON candidates(stem); CREATE INDEX IF NOT EXISTS candidate_fuzzy ON candidates(fuzzy);").map_err(db_error)?;
    tx.commit().map_err(db_error)
}

#[derive(Clone, Copy, Default)]
struct CandidateSet {
    id: i64,
    total: u64,
}

fn lookup(stage: &Connection, field: &str, key: &str, root: Option<&str>) -> Result<CandidateSet> {
    let membership = match field {
        "path" => {
            "SELECT c.id FROM candidates c WHERE c.path=?1 AND (?2 IS NULL OR c.path=?2 OR substr(c.path,1,length(?2)+1)=?2||'/')"
        }
        "name" => {
            "SELECT c.id FROM candidates c WHERE c.name=?1 AND (?2 IS NULL OR c.path=?2 OR substr(c.path,1,length(?2)+1)=?2||'/')"
        }
        "stem" => {
            "SELECT c.id FROM candidates c WHERE c.stem=?1 AND (?2 IS NULL OR c.path=?2 OR substr(c.path,1,length(?2)+1)=?2||'/')"
        }
        "fuzzy" => {
            "SELECT c.id FROM candidates c WHERE c.fuzzy=?1 AND (?2 IS NULL OR c.path=?2 OR substr(c.path,1,length(?2)+1)=?2||'/')"
        }
        "relative" => {
            "SELECT DISTINCT c.id FROM candidates c JOIN roots r ON c.path=r.path||'/'||?1 WHERE ?2 IS NULL"
        }
        _ => return Err(invalid("Invalid vendor matching field")),
    };
    let lookup_key = format!(
        "{:x}",
        Sha256::digest(json(&(field, key, root))?.as_bytes())
    );
    let existing = stage
        .query_row(
            "SELECT id,total FROM candidate_sets WHERE lookup_key=?1",
            [&lookup_key],
            |r| {
                Ok(CandidateSet {
                    id: r.get(0)?,
                    total: crate::unsigned(r, 1)?,
                })
            },
        )
        .optional()
        .map_err(db_error)?;
    if let Some(existing) = existing {
        return Ok(existing);
    }
    stage
        .execute(
            "INSERT INTO candidate_sets(lookup_key,total) VALUES(?1,0)",
            [&lookup_key],
        )
        .map_err(db_error)?;
    let id = stage.last_insert_rowid();
    // Parameter positions are fixed: set id is ?3 after the compiled matching recipe.
    // All members are written by SQLite once per unique lookup, never materialized in Rust.
    stage.execute(&format!("INSERT INTO candidate_set_members(set_id,audio_id) SELECT ?3,id FROM ({membership})"),params![key,root,id]).map_err(db_error)?;
    let total = stage
        .query_row(
            "SELECT count(*) FROM candidate_set_members WHERE set_id=?1",
            [id],
            |r| crate::unsigned(r, 0),
        )
        .map_err(db_error)?;
    stage
        .execute(
            "UPDATE candidate_sets SET total=?2 WHERE id=?1",
            params![id, super::sql_u64(total)?],
        )
        .map_err(db_error)?;
    Ok(CandidateSet { id, total })
}

fn bounded_page(
    stage: &Connection,
    set: i64,
    offset: u64,
    limit: u64,
    budget: usize,
) -> Result<VendorCandidatePage> {
    let total = if set == 0 {
        0
    } else {
        stage
            .query_row("SELECT total FROM candidate_sets WHERE id=?1", [set], |r| {
                crate::unsigned(r, 0)
            })
            .map_err(db_error)?
    };
    let mut statement=stage.prepare("SELECT c.id,c.display_path FROM candidate_set_members m JOIN candidates c ON c.id=m.audio_id WHERE m.set_id=?1 ORDER BY m.audio_id LIMIT ?2 OFFSET ?3").map_err(db_error)?;
    let encoded = statement
        .query_map(
            params![
                set,
                super::sql_u64(limit.clamp(1, 100))?,
                super::sql_u64(offset)?
            ],
            |r| {
                Ok(VendorMatchCandidate {
                    id: r.get(0)?,
                    path: r.get(1)?,
                })
            },
        )
        .map_err(db_error)?;
    let mut candidates = Vec::new();
    let mut bytes = 128usize;
    for value in encoded {
        let value = value.map_err(db_error)?;
        let length = json(&value)?.len() + 1;
        if bytes + length > budget {
            if candidates.is_empty() {
                return Err(invalid("Candidate path exceeds response safety limit"));
            }
            break;
        }
        bytes += length;
        candidates.push(value);
    }
    Ok(VendorCandidatePage {
        offset,
        total,
        candidates,
    })
}

pub(super) fn page(
    stage: &Connection,
    set: i64,
    offset: u64,
    limit: u64,
) -> Result<VendorCandidatePage> {
    bounded_page(stage, set, offset, limit, 2 * 1024 * 1024)
}

pub(super) fn inline_page(stage: &Connection, set: i64) -> Result<VendorCandidatePage> {
    bounded_page(stage, set, 0, 32, 64 * 1024)
}

pub(super) fn validate_options(options: &VendorImportOptions) -> Result<()> {
    let mappings = options
        .mapping
        .iter()
        .chain(options.sheet_mappings.iter().map(|m| &m.mapping));
    for mapping in mappings {
        for field in mapping.fields.keys() {
            if field.is_empty()
                || field.len() > 64
                || !field
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                return Err(invalid(
                    "Vendor field must contain 1..64 ASCII letters, digits or underscores",
                ));
            }
        }
    }
    if options
        .sheet_mappings
        .iter()
        .any(|m| m.header_row.is_some_and(|row| !(1..=10).contains(&row)))
    {
        return Err(invalid(
            "Header row must be within the first ten source rows",
        ));
    }
    Ok(())
}

pub(super) fn resolve(
    stage: &Connection,
    mut row: VendorRowPreview,
    cells: &[String],
    headers: &[String],
    mapping: &VendorColumnMapping,
    options: &VendorImportOptions,
) -> Result<(VendorRowPreview, i64)> {
    let cell = |column: Option<&String>| -> String {
        column
            .and_then(|key| headers.iter().position(|h| fold(h) == fold(key)))
            .and_then(|index| cells.get(index))
            .map_or_else(String::new, |s| s.trim().to_owned())
    };
    row.values = mapping
        .fields
        .iter()
        .filter_map(|(field, column)| {
            let value = cell(Some(column));
            (!value.is_empty()).then(|| (field.clone(), value))
        })
        .collect::<BTreeMap<_, _>>();
    if json(&row.values)?.len() > 512 * 1024 {
        return Err(invalid(
            "Mapped row exceeds 512 KiB; reduce duplicated source-column mappings",
        ));
    }
    let relative = cell(mapping.relative_path.as_ref());
    let filename = cell(mapping.filename.as_ref());
    let root = options.vendor_root.as_deref().map(fold);
    if let Some(root) = &root {
        let known: bool = stage.query_row("SELECT EXISTS(SELECT 1 FROM roots WHERE path=?1 OR substr(?1,1,length(path)+1)=path||'/')", [root], |r| r.get(0)).map_err(db_error)?;
        if !known {
            return Err(invalid("Vendor root must be inside a library scan root"));
        }
    }
    let mut candidates = CandidateSet::default();
    if !relative.is_empty() {
        let relative = fold(&relative);
        if relative.starts_with('/') || relative.as_bytes().get(1) == Some(&b':') {
            candidates = lookup(stage, "path", &relative, root.as_deref())?;
        } else if let Some(root) = &root {
            candidates = lookup(stage, "path", &format!("{root}/{relative}"), Some(root))?;
        } else {
            candidates = lookup(stage, "relative", &relative, None)?;
        }
    }
    let name = if filename.is_empty() {
        basename(&relative)
    } else {
        basename(&filename)
    };
    if candidates.total == 0 && !name.is_empty() {
        if let Some(root) = root.as_deref() {
            candidates = lookup(stage, "name", &name, Some(root))?;
        }
        if candidates.total == 0 {
            candidates = lookup(stage, "name", &name, None)?;
        }
        if candidates.total == 0 {
            candidates = lookup(stage, "stem", &stem(&name), root.as_deref())?;
        }
        if candidates.total == 0 && options.fuzzy {
            candidates = lookup(stage, "fuzzy", &fuzzy(&name), root.as_deref())?;
            row.suggested = candidates.total != 0;
        }
    }
    let display = inline_page(stage, candidates.id)?;
    row.matched_id = if let Some(id) = options.confirmations.get(&row.row_id) {
        let belongs:bool=stage.query_row("SELECT EXISTS(SELECT 1 FROM candidate_set_members WHERE set_id=?1 AND audio_id=?2)",params![candidates.id,id],|r|r.get(0)).map_err(db_error)?;
        if !belongs {
            return Err(invalid(
                "A confirmed audio id is not a candidate for that preview row",
            ));
        }
        Some(*id)
    } else if candidates.total == 1 && !row.suggested {
        Some(
            display
                .candidates
                .first()
                .ok_or_else(|| invalid("Candidate index is inconsistent"))?
                .id,
        )
    } else {
        None
    };
    row.user_conflict = match row.matched_id {
        Some(id) => stage
            .query_row(
                "SELECT user_conflict FROM candidates WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .map_err(db_error)?,
        None => false,
    };
    row.candidate_count = candidates.total;
    row.candidates_truncated = candidates.total > display.candidates.len() as u64;
    row.candidates = display.candidates;
    Ok((row, candidates.id))
}

pub(super) fn encoded(row: &VendorRowPreview) -> Result<String> {
    let mut stored = row.clone();
    stored.candidates.clear();
    json(&stored)
}
