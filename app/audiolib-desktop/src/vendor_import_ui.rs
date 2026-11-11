// app/audiolib-desktop/src/vendor_import_ui.rs
//! Library-bound vendor table preview, mapping, confirmation and durable batch actions.
use super::*;
use audiolib_core::vendor_import::{
    VendorCandidatePage, VendorImportHistory, VendorImportOptions, VendorImportPreview,
    VendorImportStatus, VendorMappingTemplate, VendorRowPreview, VendorSheetMapping,
};
use slint::{ModelRc, SharedString, VecModel};
use std::io::Write;

#[cfg(test)]
#[path = "vendor_import_ui_tests.rs"]
mod tests;

const PAGE_ROWS: u64 = 12;
const FIELDS: &[(&str, &str, &str)] = &[
    ("filename", "文件名", "Filename"),
    ("relative_path", "相对路径", "Relative path"),
    ("description", "描述", "Description"),
    ("keywords", "关键词", "Keywords"),
    ("category", "分类", "Category"),
    ("subcategory", "子分类", "Subcategory"),
    ("library", "库名", "Library"),
    ("publisher", "厂商", "Publisher"),
    ("source", "来源", "Source"),
    ("designer", "设计者", "Designer"),
    ("language", "语言", "Language"),
    ("cat_id", "分类 ID", "Category ID"),
];

pub(super) struct PendingImport {
    preview: VendorImportPreview,
    key: String,
    rows: Vec<VendorRowPreview>,
    offset: u64,
    row_offsets: Vec<u64>,
}

struct CandidateView {
    row_id: String,
    page: VendorCandidatePage,
    previous_offsets: Vec<u64>,
}

fn candidate_total(row: &VendorRowPreview) -> u64 {
    row.candidate_count.max(row.candidates.len() as u64)
}

#[derive(Clone)]
struct ActiveVendorWrite {
    provider: Arc<dyn DataProvider>,
    import_id: String,
    cancellable: bool,
    cancel_requested: Arc<AtomicBool>,
}

impl ActiveVendorWrite {
    fn cancel(&self) {
        // Rollback has durable batch commits but no cancellation protocol. Close
        // must drain it, rather than issuing a misleading import cancel request.
        if !self.cancellable {
            return;
        }
        if self.cancel_requested.swap(true, Ordering::AcqRel) {
            return;
        }
        let operation = self.clone();
        std::thread::spawn(move || {
            if let Err(error) = operation
                .provider
                .vendor_import_cancel(&operation.import_id)
            {
                // The library's durable checkpoint still permits later recovery;
                // never retarget this request to a newly selected provider.
                tracing::warn!(%error, "Request vendor import cancellation");
                operation.cancel_requested.store(false, Ordering::Release);
            }
        });
    }
}

#[derive(Default)]
pub(super) struct VendorUiState {
    pub(super) pending: Option<PendingImport>,
    files: Vec<PathBuf>,
    templates: Vec<VendorMappingTemplate>,
    history: Vec<VendorImportStatus>,
    mapping_fields: Vec<String>,
    request: u64,
    poll: Option<Arc<AtomicBool>>,
    active: Option<ActiveVendorWrite>,
    candidates: Option<CandidateView>,
    #[cfg(test)]
    provider_override: Option<Arc<dyn DataProvider>>,
}

impl Drop for VendorUiState {
    fn drop(&mut self) {
        if let Some(poll) = &self.poll {
            poll.store(false, Ordering::Release);
        }
        if let Some(operation) = &self.active {
            operation.cancel();
        }
    }
}

impl VendorUiState {
    pub(super) fn next_session(&self) -> Self {
        Self {
            pending: None,
            files: Vec::new(),
            templates: Vec::new(),
            history: Vec::new(),
            mapping_fields: Vec::new(),
            request: self.request.wrapping_add(1),
            poll: None,
            active: None,
            candidates: None,
            #[cfg(test)]
            provider_override: None,
        }
    }
}

fn strings(values: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        values
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))
}

fn cn(chinese: bool, chinese_text: &str, english: &str) -> SharedString {
    if chinese { chinese_text } else { english }.into()
}

fn current_options(ui: &AppWindow, pending: Option<&PendingImport>) -> VendorImportOptions {
    let mut options = pending
        .map(|p| p.preview.options.clone())
        .unwrap_or_default();
    options.vendor_root =
        (!ui.get_vi_root().trim().is_empty()).then(|| ui.get_vi_root().to_string());
    options.encoding = match ui.get_vi_encoding() {
        1 => Some("UTF-8".into()),
        2 => Some("UTF-16LE".into()),
        3 => Some("UTF-16BE".into()),
        4 => Some("GB18030".into()),
        _ => None,
    };
    options.delimiter = match ui.get_vi_delimiter() {
        1 => Some(",".into()),
        2 => Some("\t".into()),
        3 => Some(";".into()),
        _ => None,
    };
    options.fuzzy = ui.get_vi_fuzzy();
    options.overwrite = ui.get_vi_overwrite();
    options.preferred_languages = ui
        .get_vi_languages()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    options
}

fn set_options(ui: &AppWindow, options: &VendorImportOptions) {
    ui.set_vi_root(options.vendor_root.clone().unwrap_or_default().into());
    ui.set_vi_encoding(
        match options
            .encoding
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("utf-8" | "utf8") => 1,
            Some("utf-16le") => 2,
            Some("utf-16be") => 3,
            Some("gb18030" | "gbk") => 4,
            _ => 0,
        },
    );
    ui.set_vi_delimiter(match options.delimiter.as_deref() {
        Some(",") => 1,
        Some("\t") => 2,
        Some(";") => 3,
        _ => 0,
    });
    ui.set_vi_fuzzy(options.fuzzy);
    ui.set_vi_overwrite(options.overwrite);
    ui.set_vi_languages(options.preferred_languages.join(",").into());
}

impl Controller {
    fn vendor_provider(self: &Arc<Self>) -> Result<Arc<dyn DataProvider>, String> {
        self.require_metadata_writable()?;
        #[cfg(test)]
        if let Some(provider) = &self.search_v2_ux.lock().unwrap().vendor.provider_override {
            return Ok(provider.clone());
        }
        self.metadata_provider()
    }

    /// Cancel only the operation admitted against its original provider. Normal
    /// window close waits for that worker's terminal reply before draining editors.
    pub(in crate::controller) fn cancel_vendor_for_transition(&self) {
        let operation = self.search_v2_ux.lock().unwrap().vendor.active.clone();
        if let Some(operation) = operation {
            operation.cancel();
        }
    }

    fn vendor_limits(&self) -> Option<VendorLimits> {
        if self.remote_read_only() {
            self.current_vendor_import()
                .map(|info| VendorLimits::from(&info))
        } else {
            self.provider()
                .vendor_import_available()
                .unwrap_or(false)
                .then(VendorLimits::default)
        }
    }

    fn vendor_context(&self, generation: u64, request: u64) -> bool {
        self.library_generation.load(Ordering::Acquire) == generation
            && self.search_v2_ux.lock().unwrap().vendor.request == request
            && !self.shutdown.load(Ordering::Acquire)
    }

    fn begin_vendor_job(&self, ui: &AppWindow) -> Option<(u64, u64)> {
        if !self.available() || ui.get_vi_state() == VI_HIDDEN {
            return None;
        }
        self.search_v2_busy.store(true, Ordering::Release);
        let request = {
            let mut state = self.search_v2_ux.lock().unwrap();
            state.vendor.request = state.vendor.request.wrapping_add(1);
            state.vendor.request
        };
        ui.set_vi_state(VI_BUSY);
        ui.set_vi_cancellable(false);
        ui.set_vi_progress(-1.0);
        Some((self.library_generation.load(Ordering::Acquire), request))
    }

    fn finish_vendor_job(self: &Arc<Self>, ui: &AppWindow) {
        self.search_v2_busy.store(false, Ordering::Release);
        if let Some(poll) = self.search_v2_ux.lock().unwrap().vendor.poll.take() {
            poll.store(false, Ordering::Release);
        }
        self.search_v2_ux.lock().unwrap().vendor.active = None;
        ui.set_vi_cancellable(false);
        ui.set_vi_progress(0.0);
        if self.close_after_operation.swap(false, Ordering::AcqRel) {
            self.request_close(ui);
        }
    }

    pub(super) fn bind_vendor_import(self: &Arc<Self>, ui: &AppWindow) {
        let c = self.clone();
        ui.on_vendor_import_requested(move || c.open_vendor_import());
        let c = self.clone();
        ui.on_vi_pick(move || c.pick_vendor_table());
        let c = self.clone();
        ui.on_vi_apply_mapping(move || c.apply_vendor_mapping(None));
        let c = self.clone();
        ui.on_vi_options_changed(move || {
            if let Some(ui) = c.ui.upgrade()
                && ui.get_vi_has_preview()
                && ui.get_vi_editable()
                && ui.get_vi_state() == VI_PREVIEW
            {
                ui.set_vi_mapping_dirty(true);
                ui.set_vi_can_commit(false);
                if let Ok(row) = ui.get_vi_header_row().trim().parse::<u64>()
                    && row > 0
                    && let Some(sheet) = c
                        .search_v2_ux
                        .lock()
                        .unwrap()
                        .vendor
                        .pending
                        .as_mut()
                        .and_then(|p| {
                            p.preview
                                .sheets
                                .get_mut(ui.get_vi_sheet_index().max(0) as usize)
                        })
                {
                    sheet.header_row = row;
                }
            }
        });
        let c = self.clone();
        ui.on_vi_commit(move || c.vendor_write(true));
        let c = self.clone();
        ui.on_vi_rollback(move || c.vendor_write(false));
        let c = self.clone();
        ui.on_vi_cancel(move || c.cancel_vendor_import());
        let c = self.clone();
        ui.on_vi_sheet_selected(move || {
            if let Some(ui) = c.ui.upgrade() {
                c.publish_vendor_sheet(&ui);
            }
        });
        let c = self.clone();
        ui.on_vi_mapping_changed(move |field, column| c.edit_vendor_mapping(field, column));
        let c = self.clone();
        ui.on_vi_add_mapping(move || c.add_vendor_mapping());
        let c = self.clone();
        ui.on_vi_confirm_row(move |row, choice| c.confirm_vendor_row(row, choice));
        let c = self.clone();
        ui.on_vi_page_rows(move |direction| c.page_vendor_rows(direction));
        let c = self.clone();
        ui.on_vi_candidates_requested(move |row| c.open_vendor_candidates(row));
        let c = self.clone();
        ui.on_vi_candidates_page(move |direction| c.page_vendor_candidates(direction));
        let c = self.clone();
        ui.on_vi_candidate_selected(move || c.publish_vendor_candidate_path());
        let c = self.clone();
        ui.on_vi_candidate_confirmed(move || c.confirm_vendor_candidate(false));
        let c = self.clone();
        ui.on_vi_candidate_skipped(move || c.confirm_vendor_candidate(true));
        let c = self.clone();
        ui.on_vi_candidates_closed(move || {
            if let Some(ui) = c.ui.upgrade()
                && ui.get_vi_state() != VI_BUSY
            {
                ui.set_vi_candidates_visible(false);
                c.search_v2_ux.lock().unwrap().vendor.candidates = None;
            }
        });
        let c = self.clone();
        ui.on_vi_load_history(move || c.load_vendor_catalog());
        let c = self.clone();
        ui.on_vi_select_history(move |index| c.select_vendor_history(index));
        let c = self.clone();
        ui.on_vi_save_template(move || c.save_vendor_template());
        let c = self.clone();
        ui.on_vi_apply_template(move || {
            let Some(ui) = c.ui.upgrade() else { return };
            let index = ui.get_vi_template_index().max(0) as usize;
            let template = c
                .search_v2_ux
                .lock()
                .unwrap()
                .vendor
                .templates
                .get(index)
                .cloned();
            if let Some(template) = template {
                c.apply_vendor_mapping(Some(template));
            }
        });
        let c = self.clone();
        ui.on_vi_export_unmatched(move || c.export_vendor_unmatched());
        let c = self.clone();
        ui.on_vi_close(move || {
            if let Some(ui) = c.ui.upgrade()
                && ui.get_vi_state() != VI_BUSY
            {
                ui.set_vi_state(VI_HIDDEN);
                let mut state = c.search_v2_ux.lock().unwrap();
                state.vendor.request = state.vendor.request.wrapping_add(1);
            }
        });
    }

    fn open_vendor_import(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        if !self.available() {
            return;
        }
        let Some(limits) = self.vendor_limits() else {
            ui.set_vi_available(false);
            ui.set_vi_state(VI_HIDDEN);
            return;
        };
        if self.vendor_provider().is_err() {
            return;
        }
        {
            let mut state = self.search_v2_ux.lock().unwrap();
            state.vendor = state.vendor.next_session();
        }
        set_options(
            &ui,
            &VendorImportOptions {
                preferred_languages: vec!["zh".into(), "en".into()],
                ..Default::default()
            },
        );
        ui.set_vi_available(true);
        ui.set_vi_text(limits_text(limits, self.chinese.load(Ordering::Acquire)).into());
        ui.set_vi_state(VI_CHOOSE);
        ui.set_vi_page(0);
        ui.set_vi_has_preview(false);
        ui.set_vi_editable(true);
        ui.set_vi_resumable(false);
        ui.set_vi_files("".into());
        ui.set_vi_mapping_dirty(false);
        ui.set_vi_can_commit(false);
        ui.set_vi_can_rollback(false);
        ui.set_vi_rollback_confirm(false);
        ui.set_vi_progress_text("".into());
        ui.set_vi_custom_field("".into());
        ui.set_vi_custom_header(0);
        ui.set_vi_candidates_visible(false);
        self.load_vendor_catalog();
    }

    fn load_vendor_catalog(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        if !self.available() || ui.get_vi_state() == VI_HIDDEN {
            return;
        }
        let provider = match self.vendor_provider() {
            Ok(p) => p,
            Err(e) => {
                ui.set_vi_text(e.into());
                return;
            }
        };
        let generation = self.library_generation.load(Ordering::Acquire);
        let request = self.search_v2_ux.lock().unwrap().vendor.request;
        let c = self.clone();
        std::thread::spawn(move || {
            let result = provider.vendor_import_history().and_then(|history| {
                provider
                    .vendor_import_templates()
                    .map(|templates| (history, templates))
            });
            post(c.ui.clone(), move |ui| {
                if !c.vendor_context(generation, request) || ui.get_vi_state() == VI_HIDDEN {
                    return;
                }
                match result {
                    Ok((history, templates)) => c.publish_vendor_catalog(&ui, history, templates),
                    Err(error) => ui.set_vi_catalog_error(error.to_string().into()),
                }
            });
        });
    }

    fn publish_vendor_catalog(
        &self,
        ui: &AppWindow,
        history: VendorImportHistory,
        templates: Vec<VendorMappingTemplate>,
    ) {
        ui.set_vi_catalog_error("".into());
        ui.set_vi_templates(strings(
            templates.iter().map(|template| template.name.clone()),
        ));
        ui.set_vi_history(ModelRc::new(VecModel::from(
            history
                .imports
                .iter()
                .map(|item| crate::VendorHistoryRow {
                    id: item.preview.import_id.clone().into(),
                    text: format!(
                        "{} · {} · {}/{} · {}",
                        item.preview.import_id,
                        item.result
                            .as_ref()
                            .and_then(|result| result.state.as_deref())
                            .or(item.preview.state.as_deref())
                            .unwrap_or("preview"),
                        item.completed,
                        item.total,
                        item.preview
                            .files
                            .iter()
                            .map(|file| file.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                    .into(),
                })
                .collect::<Vec<_>>(),
        )));
        let mut state = self.search_v2_ux.lock().unwrap();
        state.vendor.history = history.imports;
        state.vendor.templates = templates;
    }

    fn pick_vendor_table(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(limits) = self.vendor_limits() else {
            return;
        };
        let provider = match self.vendor_provider() {
            Ok(p) => p,
            Err(e) => {
                ui.set_vi_text(e.into());
                return;
            }
        };
        let mut options = {
            let state = self.search_v2_ux.lock().unwrap();
            current_options(&ui, state.vendor.pending.as_ref())
        };
        // Every new preview requires its own human confirmations, even if a table
        // was also present in an already committed batch.
        options.confirmations.clear();
        let owner = crate::dialogs::owner_handle(ui.window());
        let chinese = self.chinese.load(Ordering::Acquire);
        let previous = ui.get_vi_state();
        let Some((generation, request)) = self.begin_vendor_job(&ui) else {
            return;
        };
        ui.set_vi_progress_text(cn(
            chinese,
            "正在检查并解析资料表…",
            "Checking and parsing tables…",
        ));
        let mut files = self.search_v2_ux.lock().unwrap().vendor.files.clone();
        let c = self.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<_, String> {
                let Some(path) = crate::dialogs::choose(
                    false,
                    crate::dialogs::FileKind::VendorTable,
                    chinese,
                    owner,
                )?
                else {
                    return Ok(None);
                };
                audiolib_core::vendor_import::precheck(&path, limits)
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.text(chinese))?;
                if !files.contains(&path) {
                    files.push(path);
                }
                provider
                    .vendor_import_preview_files(&files, &options)
                    .map(|preview| Some((files, preview)))
                    .map_err(|e| e.to_string())
            })();
            post(c.ui.clone(), move |ui| {
                if !c.vendor_context(generation, request) {
                    return;
                }
                match result {
                    Ok(Some((files, preview))) => {
                        c.search_v2_ux.lock().unwrap().vendor.files = files;
                        c.publish_vendor_preview(&ui, preview, None);
                    }
                    Ok(None) => ui.set_vi_state(previous),
                    Err(error) => {
                        ui.set_vi_text(error.into());
                        ui.set_vi_state(previous);
                    }
                }
                c.finish_vendor_job(&ui);
            });
        });
    }

    fn publish_vendor_preview(
        &self,
        ui: &AppWindow,
        preview: VendorImportPreview,
        status: Option<&VendorImportStatus>,
    ) {
        // Resolve the old file/sheet identity before changing any UI models.
        // Re-parsing this import can reorder sheets; a different import/session
        // must start from its own first sheet instead of inheriting an old index.
        let selected_sheet = {
            let state = self.search_v2_ux.lock().unwrap();
            state
                .vendor
                .pending
                .as_ref()
                .filter(|pending| {
                    !preview.import_id.is_empty() && pending.preview.import_id == preview.import_id
                })
                .and_then(|pending| {
                    usize::try_from(ui.get_vi_sheet_index())
                        .ok()
                        .and_then(|index| pending.preview.sheets.get(index))
                })
                .and_then(|selected| {
                    preview.sheets.iter().position(|sheet| {
                        sheet.source_file == selected.source_file && sheet.sheet == selected.sheet
                    })
                })
                .and_then(|index| i32::try_from(index).ok())
                .unwrap_or(0)
        };
        let chinese = self.chinese.load(Ordering::Acquire);
        ui.set_vi_candidates_visible(false);
        set_options(ui, &preview.options);
        let summary = audiolib_core::vendor_import::preview_summary(&preview, chinese);
        ui.set_vi_text(
            match status
                .and_then(|status| status.error.as_deref())
                .filter(|error| !error.trim().is_empty())
            {
                Some(error) => format!("{summary}\n{error}"),
                None => summary,
            }
            .into(),
        );
        ui.set_vi_files(
            preview
                .files
                .iter()
                .map(|file| {
                    format!(
                        "{} · {} · {} · {} rows{}",
                        file.name,
                        file.format,
                        file.encoding,
                        file.rows,
                        file.error
                            .as_ref()
                            .map(|e| format!(" · {e}"))
                            .unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        );
        ui.set_vi_warnings(preview.warnings.join("\n").into());
        ui.set_vi_sheets(strings(preview.sheets.iter().map(|sheet| {
            format!(
                "{} / {} · {} rows",
                sheet.source_file, sheet.sheet, sheet.rows
            )
        })));
        ui.set_vi_sheet_index(selected_sheet);
        ui.set_vi_has_preview(true);
        ui.set_vi_page(0);
        let phase = status
            .and_then(|s| s.result.as_ref())
            .and_then(|r| r.state.as_deref())
            .or(preview.state.as_deref())
            .unwrap_or("preview");
        let resumable = status.and_then(|s| s.idempotency_key.as_ref()).is_some()
            && phase == "paused"
            && preview.can_commit;
        let editable = status.and_then(|s| s.idempotency_key.as_ref()).is_none()
            && matches!(phase, "preview" | "cancelled");
        ui.set_vi_editable(editable);
        ui.set_vi_resumable(resumable);
        ui.set_vi_can_commit(preview.can_commit && (editable || resumable));
        ui.set_vi_mapping_dirty(false);
        ui.set_vi_can_rollback(
            status
                .and_then(|s| s.result.as_ref())
                .is_some_and(|r| r.written > 0 && r.state.as_deref() != Some("rolled_back")),
        );
        ui.set_vi_rollback_confirm(false);
        let done = matches!(
            phase,
            "done" | "committed" | "rolled_back" | "running" | "rolling_back"
        );
        ui.set_vi_state(if done { VI_COMMITTED } else { VI_PREVIEW });
        let rows = preview
            .samples
            .iter()
            .take(PAGE_ROWS as usize)
            .cloned()
            .collect();
        {
            let mut state = self.search_v2_ux.lock().unwrap();
            let key = status
                .and_then(|s| s.idempotency_key.clone())
                .or_else(|| {
                    state
                        .vendor
                        .pending
                        .as_ref()
                        .filter(|p| p.preview.import_id == preview.import_id)
                        .map(|p| p.key.clone())
                })
                .unwrap_or_else(|| idempotency_key(&preview.import_id));
            state.vendor.pending = Some(PendingImport {
                preview,
                rows,
                key,
                offset: 0,
                row_offsets: Vec::new(),
            });
            state.vendor.candidates = None;
        }
        self.publish_vendor_sheet(ui);
        self.publish_vendor_rows(ui);
    }

    fn publish_vendor_sheet(&self, ui: &AppWindow) {
        let mut state = self.search_v2_ux.lock().unwrap();
        let Some(sheet) = state.vendor.pending.as_ref().and_then(|p| {
            p.preview
                .sheets
                .get(ui.get_vi_sheet_index().max(0) as usize)
        }) else {
            ui.set_vi_mapping_rows(Default::default());
            ui.set_vi_headers(Default::default());
            return;
        };
        ui.set_vi_header_row(sheet.header_row.to_string().into());
        let chinese = self.chinese.load(Ordering::Acquire);
        ui.set_vi_headers(strings(
            std::iter::once(
                if chinese {
                    "不导入"
                } else {
                    "Do not import"
                }
                .to_string(),
            )
            .chain(sheet.headers.iter().cloned()),
        ));
        let fields = FIELDS
            .iter()
            .map(|(field, _, _)| (*field).to_string())
            .chain(
                sheet
                    .mapping
                    .fields
                    .keys()
                    .filter(|field| !FIELDS.iter().any(|(key, _, _)| *key == field.as_str()))
                    .cloned(),
            )
            .collect::<Vec<_>>();
        ui.set_vi_mapping_rows(ModelRc::new(VecModel::from(
            fields
                .iter()
                .map(|field| {
                    let column = match field.as_str() {
                        "filename" => sheet.mapping.filename.as_ref(),
                        "relative_path" => sheet.mapping.relative_path.as_ref(),
                        _ => sheet.mapping.fields.get(field),
                    };
                    let label = FIELDS
                        .iter()
                        .find(|(key, _, _)| *key == field.as_str())
                        .map(|(_, name_cn, name_en)| cn(chinese, name_cn, name_en))
                        .unwrap_or_else(|| field.clone().into());
                    crate::VendorMappingRow {
                        label,
                        selected: column
                            .and_then(|c| sheet.headers.iter().position(|h| h == c))
                            .map_or(0, |i| i as i32 + 1),
                    }
                })
                .collect::<Vec<_>>(),
        )));
        state.vendor.mapping_fields = fields;
    }

    fn edit_vendor_mapping(&self, field: i32, column: i32) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_vi_state() != VI_PREVIEW || !ui.get_vi_editable() {
            return;
        }
        let mut state = self.search_v2_ux.lock().unwrap();
        let Some(field) = usize::try_from(field)
            .ok()
            .and_then(|index| state.vendor.mapping_fields.get(index).cloned())
        else {
            return;
        };
        let Some(pending) = state.vendor.pending.as_mut() else {
            return;
        };
        let Some(sheet) = pending
            .preview
            .sheets
            .get_mut(ui.get_vi_sheet_index().max(0) as usize)
        else {
            return;
        };
        let value = (column > 0)
            .then(|| sheet.headers.get(column as usize - 1).cloned())
            .flatten();
        match field.as_str() {
            "filename" => sheet.mapping.filename = value,
            "relative_path" => sheet.mapping.relative_path = value,
            _ => {
                if let Some(value) = value {
                    sheet.mapping.fields.insert(field.clone(), value);
                } else {
                    sheet.mapping.fields.remove(&field);
                }
            }
        }
        ui.set_vi_mapping_dirty(true);
        ui.set_vi_can_commit(false);
        drop(state);
        self.publish_vendor_sheet(&ui);
    }

    fn add_vendor_mapping(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_vi_state() != VI_PREVIEW || !ui.get_vi_editable() {
            return;
        }
        let field = ui.get_vi_custom_field().trim().to_string();
        if field.is_empty()
            || field.len() > 64
            || !field
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || field == "filename"
            || field == "relative_path"
        {
            ui.set_vi_text(cn(ui.get_chinese(), "字段名需为 1–64 个英文字母、数字或下划线。文件名与相对路径请使用已有映射。", "Use 1–64 ASCII letters, digits or underscores. Use existing mappings for filename and relative path."));
            return;
        }
        let mut state = self.search_v2_ux.lock().unwrap();
        let Some(sheet) = state.vendor.pending.as_mut().and_then(|p| {
            p.preview
                .sheets
                .get_mut(ui.get_vi_sheet_index().max(0) as usize)
        }) else {
            return;
        };
        let Some(header) = usize::try_from(ui.get_vi_custom_header() - 1)
            .ok()
            .and_then(|index| sheet.headers.get(index).cloned())
        else {
            return;
        };
        sheet.mapping.fields.insert(field, header);
        drop(state);
        ui.set_vi_custom_field("".into());
        ui.set_vi_custom_header(0);
        ui.set_vi_mapping_dirty(true);
        ui.set_vi_can_commit(false);
        self.publish_vendor_sheet(&ui);
    }

    fn apply_vendor_mapping(self: &Arc<Self>, template: Option<VendorMappingTemplate>) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_vi_state() != VI_PREVIEW || !ui.get_vi_editable() {
            return;
        }
        let provider = match self.vendor_provider() {
            Ok(p) => p,
            Err(e) => {
                ui.set_vi_text(e.into());
                return;
            }
        };
        let (id, mut options) = {
            let mut state = self.search_v2_ux.lock().unwrap();
            let Some(pending) = state.vendor.pending.as_mut() else {
                return;
            };
            let Ok(header_row) = ui.get_vi_header_row().trim().parse::<u64>() else {
                ui.set_vi_text(cn(
                    ui.get_chinese(),
                    "表头行必须是正整数。",
                    "Header row must be a positive integer.",
                ));
                return;
            };
            if header_row == 0 {
                return;
            }
            if let Some(sheet) = pending
                .preview
                .sheets
                .get_mut(ui.get_vi_sheet_index().max(0) as usize)
            {
                sheet.header_row = header_row;
                if let Some(template) = &template {
                    sheet.mapping = template.mapping.clone();
                }
            }
            let mut options = current_options(&ui, Some(pending));
            options.sheet_mappings = pending
                .preview
                .sheets
                .iter()
                .map(|sheet| VendorSheetMapping {
                    source_file: sheet.source_file.clone(),
                    sheet: sheet.sheet.clone(),
                    header_row: Some(sheet.header_row),
                    mapping: sheet.mapping.clone(),
                })
                .collect();
            (pending.preview.import_id.clone(), options)
        };
        if let Some(template) = template {
            options.template = Some(template.id);
            options.encoding = template.encoding;
            options.delimiter = template.delimiter;
        }
        let Some((generation, request)) = self.begin_vendor_job(&ui) else {
            return;
        };
        let c = self.clone();
        std::thread::spawn(move || {
            let result = provider.vendor_import_mapping(&id, &options);
            post(c.ui.clone(), move |ui| {
                if !c.vendor_context(generation, request) {
                    return;
                }
                match result {
                    Ok(preview) => c.publish_vendor_preview(&ui, preview, None),
                    Err(error) => {
                        ui.set_vi_text(error.to_string().into());
                        ui.set_vi_state(VI_PREVIEW);
                    }
                }
                c.finish_vendor_job(&ui);
            });
        });
    }

    fn publish_vendor_rows(&self, ui: &AppWindow) {
        let state = self.search_v2_ux.lock().unwrap();
        let Some(pending) = &state.vendor.pending else {
            return;
        };
        let chinese = self.chinese.load(Ordering::Acquire);
        ui.set_vi_rows(ModelRc::new(VecModel::from(
            pending
                .rows
                .iter()
                .map(|row| {
                    let confirmed = pending
                        .preview
                        .options
                        .confirmations
                        .get(&row.row_id)
                        .copied();
                    let total = candidate_total(row);
                    let selected = pending
                        .preview
                        .options
                        .confirmations
                        .get(&row.row_id)
                        .and_then(|id| {
                            row.candidates
                                .iter()
                                .position(|candidate| candidate.id == *id)
                        })
                        .map_or(0, |i| i as i32 + 1);
                    let status = if confirmed.is_some() {
                        if chinese { "已确认" } else { "Confirmed" }
                    } else if row.suggested {
                        if chinese {
                            "模糊建议，需确认"
                        } else {
                            "Fuzzy suggestion; confirm"
                        }
                    } else if total > 1 {
                        if chinese {
                            "歧义，需确认"
                        } else {
                            "Ambiguous; confirm"
                        }
                    } else if row.matched_id.is_some() {
                        if chinese { "已匹配" } else { "Matched" }
                    } else {
                        if chinese { "未匹配" } else { "Unmatched" }
                    };
                    crate::VendorPreviewRow {
                        title: format!(
                            "{} / {} #{} · {}{}",
                            row.source_file,
                            row.sheet,
                            row.row,
                            status,
                            if row.user_conflict {
                                if chinese {
                                    " · 保留用户标注"
                                } else {
                                    " · user annotations retained"
                                }
                            } else {
                                ""
                            }
                        )
                        .into(),
                        values: row
                            .values
                            .iter()
                            .map(|(key, value)| format!("{key}: {value}"))
                            .collect::<Vec<_>>()
                            .join(" · ")
                            .into(),
                        candidates: strings(
                            std::iter::once(
                                if chinese {
                                    "不导入此行"
                                } else {
                                    "Skip this row"
                                }
                                .to_string(),
                            )
                            .chain(
                                row.candidates.iter().map(|candidate| {
                                    format!("{} · {}", candidate.id, candidate.path)
                                }),
                            ),
                        ),
                        needs_choice: row.suggested || total > 1,
                        candidate_label: if chinese {
                            format!("查看全部 {total} 个候选…")
                        } else {
                            format!("View all {total} candidates…")
                        }
                        .into(),
                        more_candidates: row.candidates_truncated
                            || total > row.candidates.len() as u64,
                        confirmation: confirmed
                            .map(|id| {
                                if chinese {
                                    format!("已确认记录 #{id}")
                                } else {
                                    format!("Confirmed record #{id}")
                                }
                            })
                            .unwrap_or_default()
                            .into(),
                        paged_choice: confirmed.is_some() && selected == 0,
                        selected,
                    }
                })
                .collect::<Vec<_>>(),
        )));
        ui.set_vi_rows_label(
            format!(
                "{}–{} / {}",
                if pending.rows.is_empty() {
                    0
                } else {
                    pending.offset + 1
                },
                pending.offset + pending.rows.len() as u64,
                pending.preview.rows
            )
            .into(),
        );
        ui.set_vi_rows_previous(!pending.row_offsets.is_empty());
        ui.set_vi_rows_next(
            !pending.rows.is_empty()
                && pending.offset + (pending.rows.len() as u64) < pending.preview.rows,
        );
    }

    fn confirm_vendor_row(&self, index: i32, choice: i32) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_vi_state() != VI_PREVIEW || !ui.get_vi_editable() {
            return;
        }
        let mut state = self.search_v2_ux.lock().unwrap();
        let Some(pending) = state.vendor.pending.as_mut() else {
            return;
        };
        let Some(row) = pending.rows.get(index.max(0) as usize) else {
            return;
        };
        if choice > 0 {
            if let Some(candidate) = row.candidates.get(choice as usize - 1) {
                pending
                    .preview
                    .options
                    .confirmations
                    .insert(row.row_id.clone(), candidate.id);
            }
        } else {
            pending.preview.options.confirmations.remove(&row.row_id);
        }
        ui.set_vi_mapping_dirty(true);
        ui.set_vi_can_commit(false);
        drop(state);
        self.publish_vendor_rows(&ui);
    }

    fn page_vendor_rows(self: &Arc<Self>, direction: i32) {
        let Some(ui) = self.ui.upgrade() else { return };
        let provider = match self.vendor_provider() {
            Ok(p) => p,
            Err(_) => return,
        };
        let previous = ui.get_vi_state();
        let (id, offset) = {
            let state = self.search_v2_ux.lock().unwrap();
            let Some(p) = &state.vendor.pending else {
                return;
            };
            (
                p.preview.import_id.clone(),
                if direction < 0 {
                    let Some(offset) = p.row_offsets.last() else {
                        return;
                    };
                    *offset
                } else {
                    if p.rows.is_empty() || p.offset + p.rows.len() as u64 >= p.preview.rows {
                        return;
                    }
                    p.offset.saturating_add(p.rows.len() as u64)
                },
            )
        };
        let Some((generation, request)) = self.begin_vendor_job(&ui) else {
            return;
        };
        ui.set_vi_candidates_visible(false);
        self.search_v2_ux.lock().unwrap().vendor.candidates = None;
        let c = self.clone();
        std::thread::spawn(move || {
            let result = provider.vendor_import_rows(&id, offset, PAGE_ROWS);
            post(c.ui.clone(), move |ui| {
                if !c.vendor_context(generation, request) {
                    return;
                }
                match result {
                    Ok(rows) => {
                        if let Some(p) = c.search_v2_ux.lock().unwrap().vendor.pending.as_mut() {
                            if rows.is_empty() && offset < p.preview.rows {
                                ui.set_vi_text(cn(ui.get_chinese(),"预览页为空，请重试；当前页仍保留。","The preview page was empty. Retry; the current page is retained."));
                            } else {
                                if direction < 0 {
                                    p.row_offsets.pop();
                                } else {
                                    p.row_offsets.push(p.offset);
                                }
                                p.rows = rows;
                                p.offset = offset;
                            }
                        }
                        c.publish_vendor_rows(&ui);
                    }
                    Err(error) => ui.set_vi_text(error.to_string().into()),
                }
                ui.set_vi_state(previous);
                c.finish_vendor_job(&ui);
            });
        });
    }

    fn open_vendor_candidates(self: &Arc<Self>, index: i32) {
        let Some(ui) = self.ui.upgrade() else { return };
        if !self.available() || !ui.get_vi_has_preview() {
            return;
        }
        let row = {
            let state = self.search_v2_ux.lock().unwrap();
            usize::try_from(index)
                .ok()
                .and_then(|index| state.vendor.pending.as_ref()?.rows.get(index).cloned())
        };
        let Some(row) = row else { return };
        let total = candidate_total(&row);
        ui.set_vi_candidates_title(
            format!("{} / {} #{}", row.source_file, row.sheet, row.row).into(),
        );
        ui.set_vi_candidates_visible(true);
        ui.set_vi_candidates_error("".into());
        self.search_v2_ux.lock().unwrap().vendor.candidates = Some(CandidateView {
            row_id: row.row_id,
            page: VendorCandidatePage {
                offset: 0,
                total,
                candidates: Vec::new(),
            },
            previous_offsets: Vec::new(),
        });
        self.publish_vendor_candidates(&ui);
        self.load_vendor_candidate_page(0, 0);
    }

    fn page_vendor_candidates(self: &Arc<Self>, direction: i32) {
        let offset = {
            let state = self.search_v2_ux.lock().unwrap();
            let Some(view) = &state.vendor.candidates else {
                return;
            };
            if direction < 0 {
                let Some(offset) = view.previous_offsets.last() else {
                    return;
                };
                *offset
            } else {
                let next = view
                    .page
                    .offset
                    .saturating_add(view.page.candidates.len() as u64);
                if view.page.candidates.is_empty() || next >= view.page.total {
                    return;
                }
                next
            }
        };
        self.load_vendor_candidate_page(offset, direction);
    }

    fn load_vendor_candidate_page(self: &Arc<Self>, offset: u64, direction: i32) {
        let Some(ui) = self.ui.upgrade() else { return };
        let provider = match self.vendor_provider() {
            Ok(provider) => provider,
            Err(error) => {
                ui.set_vi_candidates_error(error.into());
                return;
            }
        };
        let (id, row_id) = {
            let state = self.search_v2_ux.lock().unwrap();
            let Some(p) = &state.vendor.pending else {
                return;
            };
            let Some(view) = &state.vendor.candidates else {
                return;
            };
            (p.preview.import_id.clone(), view.row_id.clone())
        };
        let previous = ui.get_vi_state();
        let Some((generation, request)) = self.begin_vendor_job(&ui) else {
            return;
        };
        let c = self.clone();
        std::thread::spawn(move || {
            let result = provider.vendor_import_candidates(&id, &row_id, offset, 32);
            post(c.ui.clone(), move |ui| {
                if !c.vendor_context(generation, request) {
                    return;
                }
                match result {
                    Ok(page)
                        if page.offset == offset
                            && (!page.candidates.is_empty() || offset >= page.total) =>
                    {
                        if let Some(view) =
                            c.search_v2_ux.lock().unwrap().vendor.candidates.as_mut()
                        {
                            if direction < 0 {
                                view.previous_offsets.pop();
                            } else if direction > 0 {
                                view.previous_offsets.push(view.page.offset);
                            }
                            view.page = page;
                        }
                        c.publish_vendor_candidates(&ui);
                    }
                    Ok(_) => ui.set_vi_candidates_error(cn(
                        ui.get_chinese(),
                        "候选页不完整，请重试；当前选择未变更。",
                        "The candidate page was incomplete. Retry; the current choice is retained.",
                    )),
                    Err(error) => ui.set_vi_candidates_error(error.to_string().into()),
                }
                ui.set_vi_state(previous);
                c.finish_vendor_job(&ui);
            });
        });
    }

    fn publish_vendor_candidates(&self, ui: &AppWindow) {
        let state = self.search_v2_ux.lock().unwrap();
        let Some(view) = &state.vendor.candidates else {
            return;
        };
        let confirmed = state
            .vendor
            .pending
            .as_ref()
            .and_then(|p| p.preview.options.confirmations.get(&view.row_id))
            .copied();
        let chinese = ui.get_chinese();
        ui.set_vi_candidates(strings(
            std::iter::once(
                if chinese {
                    "选择此页候选"
                } else {
                    "Choose a candidate from this page"
                }
                .to_string(),
            )
            .chain(
                view.page
                    .candidates
                    .iter()
                    .map(|candidate| format!("{} · {}", candidate.id, candidate.path)),
            ),
        ));
        ui.set_vi_candidate_index(
            confirmed
                .and_then(|id| {
                    view.page
                        .candidates
                        .iter()
                        .position(|candidate| candidate.id == id)
                })
                .map_or(0, |index| index as i32 + 1),
        );
        ui.set_vi_candidates_range(
            format!(
                "{}–{} / {}",
                if view.page.candidates.is_empty() {
                    0
                } else {
                    view.page.offset + 1
                },
                view.page.offset + view.page.candidates.len() as u64,
                view.page.total
            )
            .into(),
        );
        ui.set_vi_candidates_previous(!view.previous_offsets.is_empty());
        ui.set_vi_candidates_next(
            !view.page.candidates.is_empty()
                && view.page.offset + (view.page.candidates.len() as u64) < view.page.total,
        );
        ui.set_vi_candidates_error("".into());
        drop(state);
        self.publish_vendor_candidate_path();
    }

    fn publish_vendor_candidate_path(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let state = self.search_v2_ux.lock().unwrap();
        let path = state
            .vendor
            .candidates
            .as_ref()
            .and_then(|view| {
                usize::try_from(ui.get_vi_candidate_index())
                    .ok()
                    .and_then(|index| index.checked_sub(1))
                    .and_then(|index| view.page.candidates.get(index))
            })
            .map(|candidate| candidate.path.clone())
            .unwrap_or_default();
        ui.set_vi_candidate_path(path.into());
    }

    fn confirm_vendor_candidate(&self, skip: bool) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_vi_state() != VI_PREVIEW || !ui.get_vi_editable() {
            return;
        }
        let mut state = self.search_v2_ux.lock().unwrap();
        let Some(view) = &state.vendor.candidates else {
            return;
        };
        let row_id = view.row_id.clone();
        let chosen = if skip {
            None
        } else {
            let Some(candidate) = usize::try_from(ui.get_vi_candidate_index())
                .ok()
                .and_then(|index| index.checked_sub(1))
                .and_then(|index| view.page.candidates.get(index))
            else {
                return;
            };
            Some(candidate.id)
        };
        let Some(p) = state.vendor.pending.as_mut() else {
            return;
        };
        if !p.rows.iter().any(|row| row.row_id == row_id) {
            return;
        }
        if let Some(id) = chosen {
            p.preview.options.confirmations.insert(row_id, id);
        } else {
            p.preview.options.confirmations.remove(&row_id);
        }
        state.vendor.candidates = None;
        drop(state);
        ui.set_vi_candidates_visible(false);
        ui.set_vi_mapping_dirty(true);
        ui.set_vi_can_commit(false);
        self.publish_vendor_rows(&ui);
    }

    fn select_vendor_history(self: &Arc<Self>, index: i32) {
        let Some(ui) = self.ui.upgrade() else { return };
        let provider = match self.vendor_provider() {
            Ok(p) => p,
            Err(_) => return,
        };
        let id = self
            .search_v2_ux
            .lock()
            .unwrap()
            .vendor
            .history
            .get(index.max(0) as usize)
            .map(|s| s.preview.import_id.clone());
        let Some(id) = id else { return };
        let Some((generation, request)) = self.begin_vendor_job(&ui) else {
            return;
        };
        let c = self.clone();
        std::thread::spawn(move || {
            let result = provider.vendor_import_status(&id);
            post(c.ui.clone(), move |ui| {
                if !c.vendor_context(generation, request) {
                    return;
                }
                match result {
                    Ok(status) => {
                        c.publish_vendor_preview(&ui, status.preview.clone(), Some(&status))
                    }
                    Err(error) => {
                        ui.set_vi_text(error.to_string().into());
                        ui.set_vi_state(VI_CHOOSE);
                    }
                }
                c.finish_vendor_job(&ui);
            });
        });
    }

    fn save_vendor_template(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_vi_template_name().trim().is_empty() {
            ui.set_vi_catalog_error(cn(
                ui.get_chinese(),
                "请输入模板名称。",
                "Enter a template name.",
            ));
            return;
        }
        let provider = match self.vendor_provider() {
            Ok(p) => p,
            Err(_) => return,
        };
        let previous = ui.get_vi_state();
        let template = {
            let state = self.search_v2_ux.lock().unwrap();
            let Some(p) = &state.vendor.pending else {
                return;
            };
            let Some(sheet) = p
                .preview
                .sheets
                .get(ui.get_vi_sheet_index().max(0) as usize)
            else {
                return;
            };
            let options = current_options(&ui, Some(p));
            VendorMappingTemplate {
                id: String::new(),
                name: ui.get_vi_template_name().trim().into(),
                mapping: sheet.mapping.clone(),
                encoding: options.encoding,
                delimiter: options.delimiter,
            }
        };
        let Some((generation, request)) = self.begin_vendor_job(&ui) else {
            return;
        };
        let c = self.clone();
        std::thread::spawn(move || {
            let result = provider.vendor_import_save_template(&template);
            post(c.ui.clone(), move |ui| {
                if !c.vendor_context(generation, request) {
                    return;
                }
                if let Err(error) = result {
                    ui.set_vi_catalog_error(error.to_string().into());
                }
                ui.set_vi_state(previous);
                c.finish_vendor_job(&ui);
                c.load_vendor_catalog();
            });
        });
    }

    fn vendor_write(self: &Arc<Self>, commit: bool) {
        let Some(ui) = self.ui.upgrade() else { return };
        if (commit && (!ui.get_vi_can_commit() || ui.get_vi_mapping_dirty()))
            || (!commit && (!ui.get_vi_can_rollback() || !ui.get_vi_rollback_confirm()))
        {
            return;
        }
        let provider = match self.vendor_provider() {
            Ok(p) => p,
            Err(e) => {
                ui.set_vi_text(e.into());
                return;
            }
        };
        let (id, key) = {
            let state = self.search_v2_ux.lock().unwrap();
            let Some(p) = &state.vendor.pending else {
                return;
            };
            (p.preview.import_id.clone(), p.key.clone())
        };
        self.capture_metadata(&ui);
        let barrier = match self.metadata.lock().unwrap().enqueue_all() {
            Ok(b) => b,
            Err(e) => {
                ui.set_vi_text(e.into());
                return;
            }
        };
        self.metadata_wake.notify_all();
        let Some((generation, request)) = self.begin_vendor_job(&ui) else {
            return;
        };
        self.search_v2_ux.lock().unwrap().vendor.active = Some(ActiveVendorWrite {
            provider: provider.clone(),
            import_id: id.clone(),
            cancellable: commit,
            cancel_requested: Arc::new(AtomicBool::new(false)),
        });
        let active = Arc::new(AtomicBool::new(true));
        self.search_v2_ux.lock().unwrap().vendor.poll = Some(active.clone());
        ui.set_vi_rollback_confirm(false);
        ui.set_vi_cancellable(commit);
        let selected_id = self.selected.lock().unwrap().as_ref().map(|file| file.id);
        let c = self.clone();
        let polling_provider = provider.clone();
        let polling_id = id.clone();
        std::thread::spawn(move || {
            while active.load(Ordering::Acquire) {
                if let Ok(status) = polling_provider.vendor_import_status(&polling_id) {
                    let c2 = c.clone();
                    post(c.ui.clone(), move |ui| {
                        if !c2.vendor_context(generation, request) || ui.get_vi_state() != VI_BUSY {
                            return;
                        }
                        ui.set_vi_progress(if status.total == 0 {
                            -1.0
                        } else {
                            status.completed as f32 / status.total as f32
                        });
                        ui.set_vi_progress_text(
                            format!("{} / {}", status.completed, status.total).into(),
                        );
                        // An old/stale status can still describe the previously
                        // committed import while this rollback has just started.
                        ui.set_vi_cancellable(commit && status.cancellable);
                    });
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        let c = self.clone();
        std::thread::spawn(move || {
            let outcome = super::super::survive_panic(
                || {
                    let result = c.wait_metadata(barrier, true).and_then(|_| {
                        if commit {
                            provider.vendor_import_commit(&id, &key)
                        } else {
                            provider.vendor_import_rollback(&id)
                        }
                        .map_err(|e| e.to_string())
                    });
                    let status = provider.vendor_import_status(&id).ok();
                    let selected = selected_id.and_then(|id| provider.get_audio(id).ok());
                    Ok::<_, String>((result, status, selected))
                },
                || {
                    "Vendor Metadata operation failed with an internal error; see the log for details".into()
                },
            );
            let (result, status, selected) =
                outcome.unwrap_or_else(|error| (Err(error), None, None));
            post(c.ui.clone(), move |ui| {
                if !c.vendor_context(generation, request) {
                    return;
                }
                match result {
                    Ok(reply) => {
                        if let Some(status) = &status {
                            c.publish_vendor_preview(&ui, status.preview.clone(), Some(status));
                        } else {
                            let paused = reply.state.as_deref() == Some("paused");
                            ui.set_vi_state(if paused { VI_PREVIEW } else { VI_COMMITTED });
                            ui.set_vi_can_commit(paused);
                            ui.set_vi_resumable(paused);
                            ui.set_vi_editable(false);
                            ui.set_vi_can_rollback(
                                reply.written > reply.restored
                                    && reply.state.as_deref() != Some("rolled_back"),
                            );
                        }
                        ui.set_vi_text(if reply.state.as_deref()==Some("paused") {
                            if ui.get_chinese(){format!("已暂停，已写入 {} 行。可继续此批或从历史回滚。",reply.written)}
                            else{format!("Paused after {} written rows. Continue this batch or roll it back from history.",reply.written)}
                        } else if commit {audiolib_core::vendor_import::committed_text(&reply,ui.get_chinese())} else {audiolib_core::vendor_import::rolled_back_text(&reply,ui.get_chinese())}.into());
                        if c.remote_read_only() {
                            ui.set_remote_notice(
                                super::super::remote_mode::refresh_notice_text(ui.get_chinese())
                                    .into(),
                            );
                            ui.set_remote_notice_kind(super::super::remote_mode::NOTICE_UPDATE);
                            ui.set_remote_notice_refresh(true);
                            c.wake_revision_poller();
                        } else {
                            c.refresh_same_library_view(&ui, selected, &Default::default());
                        }
                    }
                    Err(error) => {
                        if let Some(status) = &status {
                            c.publish_vendor_preview(&ui, status.preview.clone(), Some(status));
                        } else {
                            ui.set_vi_state(VI_PREVIEW);
                        }
                        ui.set_vi_text(error.into());
                    }
                }
                c.finish_vendor_job(&ui);
                if !c.remote_read_only() {
                    c.refresh_library_info();
                    c.reload_tree();
                    c.request_search(&ui, true, false);
                }
                c.load_vendor_catalog();
            });
        });
    }

    fn cancel_vendor_import(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_vi_state() != VI_BUSY || !ui.get_vi_cancellable() {
            return;
        }
        self.cancel_vendor_for_transition();
        ui.set_vi_cancellable(false);
        ui.set_vi_progress_text(cn(
            ui.get_chinese(),
            "已请求取消，正在等待批次安全结束…",
            "Cancellation requested; waiting for the batch to finish safely…",
        ));
    }

    fn export_vendor_unmatched(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        let provider = match self.vendor_provider() {
            Ok(p) => p,
            Err(_) => return,
        };
        let id = self
            .search_v2_ux
            .lock()
            .unwrap()
            .vendor
            .pending
            .as_ref()
            .map(|p| p.preview.import_id.clone());
        let Some(id) = id else { return };
        let owner = crate::dialogs::owner_handle(ui.window());
        let chinese = ui.get_chinese();
        let previous = ui.get_vi_state();
        let Some((generation, request)) = self.begin_vendor_job(&ui) else {
            return;
        };
        let c = self.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<(), String> {
                let Some(path) =
                    crate::dialogs::choose(true, crate::dialogs::FileKind::Csv, chinese, owner)?
                else {
                    return Ok(());
                };
                let mut output = std::io::BufWriter::new(
                    std::fs::File::create(path).map_err(|e| e.to_string())?,
                );
                writeln!(output, "source_file,sheet,row,status,values")
                    .map_err(|e| e.to_string())?;
                let mut offset = 0;
                loop {
                    let rows = provider
                        .vendor_import_rows(&id, offset, 500)
                        .map_err(|e| e.to_string())?;
                    if rows.is_empty() {
                        break;
                    }
                    offset += rows.len() as u64;
                    for row in rows {
                        if row.matched_id.is_some() && !row.suggested && candidate_total(&row) <= 1
                        {
                            continue;
                        }
                        let values =
                            serde_json::to_string(&row.values).map_err(|e| e.to_string())?;
                        let cells = [
                            row.source_file,
                            row.sheet,
                            row.row.to_string(),
                            if row.candidates.is_empty() {
                                "unmatched"
                            } else {
                                "needs_confirmation"
                            }
                            .into(),
                            values,
                        ];
                        writeln!(
                            output,
                            "{}",
                            cells
                                .iter()
                                .map(|s| csv_cell(s))
                                .collect::<Vec<_>>()
                                .join(",")
                        )
                        .map_err(|e| e.to_string())?;
                    }
                }
                output.flush().map_err(|e| e.to_string())
            })();
            post(c.ui.clone(), move |ui| {
                if !c.vendor_context(generation, request) {
                    return;
                }
                if let Err(error) = result {
                    ui.set_vi_text(error.into());
                }
                ui.set_vi_state(previous);
                c.finish_vendor_job(&ui);
            });
        });
    }
}

fn csv_cell(value: &str) -> String {
    let prefix = if value
        .trim_start()
        .starts_with(['=', '+', '-', '@', '\t', '\r'])
    {
        "'"
    } else {
        ""
    };
    format!("\"{prefix}{}\"", value.replace('"', "\"\""))
}
