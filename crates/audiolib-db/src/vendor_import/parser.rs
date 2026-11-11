// crates/audiolib-db/src/vendor_import/parser.rs
//! Bounded table readers. CSV and XLSX rows are streamed; BIFF is size-checked before loading.
use super::{Result, invalid};
use audiolib_core::vendor_import::{
    VendorColumnMapping, VendorFileResult, VendorFormat, VendorImportOptions, VendorLimits,
    VendorSheetPreview, sniff_format,
};
use calamine::{Reader, Xls, Xlsx};
use encoding_rs::{Encoding, GB18030, UTF_8, UTF_16BE, UTF_16LE};
use encoding_rs_io::DecodeReaderBytesBuilder;
use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

pub(super) enum Event {
    Sheet(VendorSheetPreview),
    Row {
        sheet: String,
        row: u64,
        cells: Vec<String>,
    },
}

pub(super) fn canonical(header: &str) -> Option<&'static str> {
    let normalized: String = header
        .chars()
        .filter(|c| !c.is_whitespace() && !"_-.:".contains(*c))
        .flat_map(char::to_lowercase)
        .collect();
    Some(match normalized.as_str() {
        "filename" | "file" | "wavfilename" | "soundfilename" | "文件名" | "文件名称" => {
            "filename"
        }
        "filepath" | "relativepath" | "path" | "完整路径" | "路径" | "相对路径" => {
            "relative_path"
        }
        "description" | "desc" | "bwfdescription" | "bwdescription" | "描述" | "说明"
        | "中文描述" | "英文描述" => "description",
        "keywords" | "keyword" | "tags" | "关键词" | "关键字" | "标签" => "keywords",
        "category" | "类别" | "分类" => "category",
        "subcategory" | "子类" | "子分类" => "subcategory",
        "library" | "libraryname" | "库名" | "音效库" => "library",
        "publisher" | "vendor" | "manufacturer" | "厂商" | "发行商" => "publisher",
        "source" | "来源" => "source",
        "designer" | "设计师" => "designer",
        "language" | "lang" | "语言" => "language",
        "catid" | "ucscatid" => "cat_id",
        _ => return None,
    })
}

fn infer(headers: &[String]) -> VendorColumnMapping {
    let mut mapping = VendorColumnMapping::default();
    for header in headers {
        match canonical(header) {
            Some("filename") => mapping.filename = Some(header.clone()),
            Some("relative_path") => mapping.relative_path = Some(header.clone()),
            Some(field) => {
                mapping
                    .fields
                    .entry(field.into())
                    .or_insert_with(|| header.clone());
            }
            None => {}
        }
    }
    mapping
}

struct Sheet<'a, F> {
    name: &'a str,
    source: &'a str,
    options: &'a VendorImportOptions,
    pending: Vec<(u64, Vec<String>)>,
    info: Option<VendorSheetPreview>,
    emit: &'a mut F,
    truncated: u64,
    limit: u64,
}

impl<F: FnMut(Event) -> Result<()>> Sheet<'_, F> {
    fn start(&mut self) -> Result<()> {
        if self.info.is_some() || self.pending.is_empty() {
            return Ok(());
        }
        let override_mapping = self
            .options
            .sheet_mappings
            .iter()
            .find(|m| m.source_file == self.source && m.sheet == self.name);
        let selected = if let Some(row) = override_mapping.and_then(|m| m.header_row) {
            self.pending
                .iter()
                .position(|(n, _)| *n == row)
                .ok_or_else(|| {
                    invalid("Requested header row is not present in the first ten source rows")
                })?
        } else {
            self.pending
                .iter()
                .enumerate()
                .max_by_key(|(index, (_, cells))| {
                    (
                        cells.iter().filter(|s| canonical(s).is_some()).count(),
                        std::cmp::Reverse(*index),
                    )
                })
                .map_or(0, |(i, _)| i)
        };
        let (header_row, headers) = &self.pending[selected];
        let mapping = override_mapping
            .map(|m| m.mapping.clone())
            .or_else(|| self.options.mapping.clone())
            .unwrap_or_else(|| infer(headers));
        let info = VendorSheetPreview {
            source_file: self.source.into(),
            sheet: self.name.into(),
            header_row: *header_row,
            headers: headers.clone(),
            mapping,
            rows: 0,
        };
        (self.emit)(Event::Sheet(info.clone()))?;
        self.info = Some(info);
        let data = std::mem::take(&mut self.pending);
        for (row, cells) in data.into_iter().skip(selected + 1) {
            self.data(row, cells)?;
        }
        Ok(())
    }

    fn data(&mut self, row: u64, cells: Vec<String>) -> Result<()> {
        if cells.iter().all(|s| s.trim().is_empty()) {
            return Ok(());
        }
        if let Some(info) = &mut self.info {
            info.rows += 1;
        }
        (self.emit)(Event::Row {
            sheet: self.name.into(),
            row,
            cells,
        })
    }

    fn push(&mut self, row: u64, mut cells: Vec<String>) -> Result<()> {
        // The first ten buffered records may become headers; they obey the same
        // decoded text bound as subsequent data rather than bypassing truncation.
        let mut remaining = self.limit as usize;
        let mut truncated = false;
        for cell in &mut cells {
            if cell.len() > remaining {
                let mut boundary = remaining;
                while boundary > 0 && !cell.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                cell.truncate(boundary);
                truncated = true;
            }
            remaining = remaining.saturating_sub(cell.len());
        }
        self.truncated += u64::from(truncated);
        if self.info.is_none() {
            self.pending.push((row, cells));
            if self.pending.len() == 10 {
                self.start()?;
            }
            Ok(())
        } else {
            self.data(row, cells)
        }
    }

    fn finish(mut self) -> Result<u64> {
        self.start()?;
        if let Some(info) = self.info.take() {
            (self.emit)(Event::Sheet(info))?;
        }
        Ok(self.truncated)
    }
}

fn strict_utf8(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0; 65540];
    let mut carry = 0;
    loop {
        let read = file.read(&mut buffer[carry..65536])?;
        let length = carry + read;
        match std::str::from_utf8(&buffer[..length]) {
            Ok(_) => carry = 0,
            Err(error) if error.error_len().is_none() && read != 0 => {
                carry = length - error.valid_up_to();
                buffer.copy_within(error.valid_up_to()..length, 0);
            }
            Err(_) => return Ok(false),
        }
        if read == 0 {
            return Ok(carry == 0);
        }
    }
}

fn encoding(path: &Path, requested: Option<&str>) -> Result<&'static Encoding> {
    if let Some(name) = requested {
        return Encoding::for_label(name.as_bytes())
            .ok_or_else(|| invalid("Unknown text encoding"));
    }
    let mut head = [0; 3];
    let count = File::open(path)?.read(&mut head)?;
    if head[..count].starts_with(&[0xff, 0xfe]) {
        return Ok(UTF_16LE);
    }
    if head[..count].starts_with(&[0xfe, 0xff]) {
        return Ok(UTF_16BE);
    }
    if head[..count].starts_with(&[0xef, 0xbb, 0xbf]) {
        return Ok(UTF_8);
    }
    if strict_utf8(path)? {
        Ok(UTF_8)
    } else {
        Ok(GB18030)
    }
}

fn text_reader(path: &Path, encoding: &'static Encoding) -> Result<impl Read> {
    Ok(DecodeReaderBytesBuilder::new()
        .encoding(Some(encoding))
        .build(File::open(path)?))
}

fn delimiter(path: &Path, encoding: &'static Encoding, requested: Option<&str>) -> Result<u8> {
    if let Some(value) = requested {
        return match value {
            "," => Ok(b','),
            ";" => Ok(b';'),
            "|" => Ok(b'|'),
            "\t" | "tab" => Ok(b'\t'),
            _ => Err(invalid("Delimiter must be comma, tab, semicolon or pipe")),
        };
    }
    let mut sample = Vec::new();
    text_reader(path, encoding)?
        .take(65536)
        .read_to_end(&mut sample)?;
    Ok((*b",\t;|")
        .into_iter()
        .max_by_key(|candidate| {
            let mut frequencies = BTreeMap::<usize, usize>::new();
            for record in csv::ReaderBuilder::new()
                .has_headers(false)
                .flexible(true)
                .delimiter(*candidate)
                .from_reader(sample.as_slice())
                .byte_records()
                .take(10)
                .flatten()
            {
                if record.len() > 1 {
                    *frequencies.entry(record.len()).or_default() += 1;
                }
            }
            frequencies
                .into_iter()
                .map(|(columns, count)| (count, columns))
                .max()
                .unwrap_or_default()
        })
        .unwrap_or(b','))
}

/// Emit each accepted file into an owned staging transaction. Caller rolls that file back on error.
pub(super) fn parse(
    path: &Path,
    source: &str,
    options: &VendorImportOptions,
    limits: VendorLimits,
    decoded_remaining: &mut u64,
    mut emit: impl FnMut(Event) -> Result<()>,
) -> Result<VendorFileResult> {
    let bytes = std::fs::metadata(path)?.len();
    if bytes == 0 {
        return Err(invalid("No data: empty file"));
    }
    if bytes > limits.max_file_bytes {
        return Err(invalid("File exceeds 50 MiB input limit"));
    }
    let mut head = [0; 8];
    let count = File::open(path)?.read(&mut head)?;
    let format = sniff_format(&head[..count]);
    if format == VendorFormat::Xlsx {
        super::resources::preflight(path, decoded_remaining)?;
    }
    let mut result = VendorFileResult {
        name: source.into(),
        ..Default::default()
    };
    let mut row_count = 0;
    let mut bounded_emit = |event: Event| {
        if matches!(&event, Event::Row { .. }) {
            row_count += 1;
            if row_count > limits.max_rows {
                return Err(invalid("File exceeds 200000 data rows across sheets"));
            }
        }
        if format != VendorFormat::Xlsx
            && let Event::Row { cells, .. } = &event
        {
            let decoded = cells.iter().map(|s| s.len() as u64).sum::<u64>();
            if decoded > *decoded_remaining {
                return Err(invalid(
                    "Vendor batch exceeds 512 MiB decoded safety budget",
                ));
            }
            *decoded_remaining -= decoded;
        }
        emit(event)
    };
    match format {
        VendorFormat::Text => {
            let enc = encoding(path, options.encoding.as_deref())?;
            let separator = delimiter(path, enc, options.delimiter.as_deref())?;
            result.format = "text".into();
            result.encoding = enc.name().into();
            result.delimiter = Some(char::from(separator).to_string());
            let mut sheet = Sheet {
                name: "text",
                source,
                options,
                pending: Vec::new(),
                info: None,
                emit: &mut bounded_emit,
                truncated: 0,
                limit: limits.max_row_text_bytes,
            };
            let mut reader = csv::ReaderBuilder::new()
                .has_headers(false)
                .flexible(true)
                .delimiter(separator)
                .from_reader(text_reader(path, enc)?);
            for (index, record) in reader.records().enumerate() {
                let record = record.map_err(invalid)?;
                sheet.push(index as u64 + 1, record.iter().map(str::to_owned).collect())?;
            }
            result.truncated_rows = sheet.finish()?;
        }
        VendorFormat::Xls => {
            result.format = "xls".into();
            result.encoding = "BIFF cached values".into();
            let mut book = Xls::new(File::open(path)?).map_err(invalid)?;
            for name in book.sheet_names().to_vec() {
                let range = book.worksheet_range(&name).map_err(invalid)?;
                let first = range.start().map_or(0, |position| position.0);
                let mut sheet = Sheet {
                    name: &name,
                    source,
                    options,
                    pending: Vec::new(),
                    info: None,
                    emit: &mut bounded_emit,
                    truncated: 0,
                    limit: limits.max_row_text_bytes,
                };
                for (index, row) in range.rows().enumerate() {
                    sheet.push(
                        u64::from(first) + index as u64 + 1,
                        row.iter().map(ToString::to_string).collect(),
                    )?;
                }
                result.truncated_rows += sheet.finish()?;
            }
        }
        VendorFormat::Xlsx => {
            result.format = "xlsx".into();
            result.encoding = "OOXML cached values".into();
            let mut book = Xlsx::new(File::open(path)?).map_err(invalid)?;
            for name in book.sheet_names().to_vec() {
                let mut cells = book.worksheet_cells_reader(&name).map_err(invalid)?;
                let mut sheet = Sheet {
                    name: &name,
                    source,
                    options,
                    pending: Vec::new(),
                    info: None,
                    emit: &mut bounded_emit,
                    truncated: 0,
                    limit: limits.max_row_text_bytes,
                };
                let mut position = None;
                let mut row = Vec::new();
                while let Some(cell) = cells.next_cell().map_err(invalid)? {
                    let (r, c) = cell.get_position();
                    if c >= 16384 {
                        return Err(invalid(
                            "Spreadsheet column is outside the XLSX format limit",
                        ));
                    }
                    if position.is_some_and(|previous| previous != r) {
                        sheet.push(
                            u64::from(position.unwrap_or(0)) + 1,
                            std::mem::take(&mut row),
                        )?;
                    }
                    position = Some(r);
                    if row.len() <= c as usize {
                        row.resize_with(c as usize + 1, String::new);
                    }
                    row[c as usize] = calamine::Data::from(cell.get_value().clone()).to_string();
                }
                if let Some(r) = position {
                    sheet.push(u64::from(r) + 1, row)?;
                }
                result.truncated_rows += sheet.finish()?;
            }
        }
    }
    result.rows = row_count;
    if row_count == 0 {
        return Err(invalid("No data: table contains no data rows"));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/vendor_import")
            .join(name)
    }
    #[test]
    fn every_real_format_checks_exact_bytes_before_loading_and_all_sheet_row_totals() {
        assert_eq!(VendorLimits::default().max_file_bytes, 52_428_800);
        assert_eq!(VendorLimits::default().max_rows, 200_000);
        for (name, rows) in [("basic.csv", 2), ("basic.xls", 3), ("basic.xlsx", 3)] {
            let path = input(name);
            let bytes = std::fs::metadata(&path).unwrap().len();
            for allowance in [bytes - 1, bytes, bytes + 1] {
                let limits = VendorLimits {
                    max_file_bytes: allowance,
                    ..VendorLimits::default()
                };
                let mut remaining = super::super::resources::TOTAL_DECODED_BYTES;
                let result = parse(
                    &path,
                    name,
                    &VendorImportOptions::default(),
                    limits,
                    &mut remaining,
                    |_| Ok(()),
                );
                assert_eq!(result.is_ok(), allowance >= bytes, "{name}: {allowance}");
            }
            // Small real multi-sheet fixtures exercise the same exact threshold
            // logic without generating a second 200k stress workbook per unit test.
            for allowance in [rows - 1, rows, rows + 1] {
                let limits = VendorLimits {
                    max_rows: allowance,
                    ..VendorLimits::default()
                };
                let mut remaining = super::super::resources::TOTAL_DECODED_BYTES;
                let result = parse(
                    &path,
                    name,
                    &VendorImportOptions::default(),
                    limits,
                    &mut remaining,
                    |_| Ok(()),
                );
                assert_eq!(
                    result.is_ok(),
                    allowance >= rows,
                    "{name}: rows {allowance}"
                );
            }
        }
    }
}
