// crates/audiolib-db/src/migration.rs
//! Shared owned-library inspection and migration entry points for desktop, CLI and server.
mod checkpoint;
mod diagnose;
mod inspect;
mod job;
mod plan;
mod recipe;
mod runtime;
mod stage;
pub use diagnose::{DiagnosticCode, DiagnosticFinding, LibraryDiagnosis, diagnose};
pub use inspect::{BusinessDigest, LibraryInspection, TableDigest, inspect};
pub(crate) use job::admit_import_identity;
pub use job::{
    ImportIdentityAdmission, MigrationJob, MigrationPhase, MigrationProgress, apply, resume,
    rollback, verify,
};
pub use plan::{LocalHistoryMigration, MigrationPlan, SpaceBudget, plan, plan_local};
pub use recipe::SchemaChange;
pub(crate) use recipe::changes as schema_changes;
pub use runtime::discover;
pub use runtime::prepare_local_writable;
pub(crate) use runtime::prepare_writable;

/// Opaque import admission held for an explicit migration tool's full operation.
/// Dropping it releases the shared receipt lease without changing the receipt.
pub struct ImportOwnerAdmission {
    _lease: Option<crate::import_lease::ExistingImportOpenLease>,
}

/// Admit an explicit plan/apply/resume/verify/rollback operation without creating
/// sidecars. Hold the returned guard through the complete operation. A pending
/// local import requires its matching persisted history scope; server scope is
/// rejected. Only verify/rollback may set `recovery` to admit an unavailable
/// history root, while still matching its saved path, library identity and limit.
///
/// Internal runtime migration already holds an import owner and must not call
/// this wrapper, which would reenter its exclusive receipt lock.
pub fn admit_import_owner(
    path: &std::path::Path,
    local: Option<LocalHistoryMigration>,
    recovery: bool,
) -> audiolib_core::Result<ImportOwnerAdmission> {
    let lease = crate::import_lease::ExistingImportOpenLease::acquire_existing(path)?;
    if let Some(lease) = &lease {
        lease.authorize_migration(local.as_ref(), recovery)?;
    }
    Ok(ImportOwnerAdmission { _lease: lease })
}

pub(crate) fn digest_business(
    conn: &rusqlite::Connection,
    cancel: &std::sync::atomic::AtomicBool,
) -> audiolib_core::Result<BusinessDigest> {
    inspect::business_digest(conn, cancel)
}

/// Explicit new-library import may replace its local identity and preference alias.
/// Every other value, including unknown extension rows and implicit identities,
/// remains subject to the ordinary lossless business projection.
pub(crate) fn digest_import_admission(
    conn: &rusqlite::Connection,
    cancel: &std::sync::atomic::AtomicBool,
) -> audiolib_core::Result<BusinessDigest> {
    let mut projection = inspect::business_digest(conn, cancel)?;
    if let Some(table) = projection
        .tables
        .iter_mut()
        .find(|table| table.table == "audiolib_album_id_sequence")
    {
        table
            .columns
            .retain(|column| column != "namespace" && column != "legacy_scope");
    }
    inspect::business_digest_matching(conn, Some(&projection), cancel)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod local_history_tests;
