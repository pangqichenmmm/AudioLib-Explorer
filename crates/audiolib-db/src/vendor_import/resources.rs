// crates/audiolib-db/src/vendor_import/resources.rs
//! Preflight decompression/XML before calamine can materialize a hostile cell or shared string.
use super::{Result, invalid};
use quick_xml::{Reader, events::Event};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read},
    path::Path,
};

pub(super) const TOTAL_DECODED_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Limits {
    entry: u64,
    event: u64,
    cell: u64,
    row: u64,
    shared_text: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            entry: 256 * 1024 * 1024,
            event: 1024 * 1024,
            cell: 1024 * 1024,
            row: 2 * 1024 * 1024,
            shared_text: 64 * 1024 * 1024,
        }
    }
}

/// Limit consumed bytes rather than trusting central-directory lengths or checking
/// a Vec after allocation. quick-xml must ask this reader before growing any event.
struct Guarded<R> {
    inner: R,
    step: u64,
    entry: u64,
    consumed: u64,
}
impl<R: BufRead> BufRead for Guarded<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        let bytes = self.inner.fill_buf()?;
        if bytes.is_empty() {
            return Ok(bytes);
        }
        let allowed = self.step.min(self.entry).min(bytes.len() as u64) as usize;
        if allowed == 0 {
            return Err(io::Error::other(
                "Vendor XML/decompression safety budget exceeded",
            ));
        }
        Ok(&bytes[..allowed])
    }
    fn consume(&mut self, amount: usize) {
        self.inner.consume(amount);
        self.step = self.step.saturating_sub(amount as u64);
        self.entry = self.entry.saturating_sub(amount as u64);
        self.consumed = self.consumed.saturating_add(amount as u64);
    }
}
impl<R: BufRead> Read for Guarded<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let input = self.fill_buf()?;
        let length = input.len().min(output.len());
        output[..length].copy_from_slice(&input[..length]);
        self.consume(length);
        Ok(length)
    }
}

fn xml<R: Read>(input: R, entry_limit: u64, mut shared: bool, limits: Limits) -> (Result<()>, u64) {
    let mut guarded = Guarded {
        inner: BufReader::with_capacity(8192, input),
        step: limits.event,
        entry: entry_limit,
        consumed: 0,
    };
    let head = match guarded.fill_buf() {
        Ok(head) => head,
        Err(error) => return (Err(invalid(error)), 0),
    };
    if head.starts_with(&[0xff, 0xfe])
        || head.starts_with(&[0xfe, 0xff])
        || head.starts_with(&[0, b'<'])
        || head.starts_with(&[b'<', 0])
    {
        return (
            Err(invalid("OOXML safety preflight requires UTF-8 XML parts")),
            0,
        );
    }
    let mut reader = Reader::from_reader(guarded);
    let mut buffer = Vec::with_capacity(8192);
    let mut cell_start: Option<u64> = None;
    let mut row_start: Option<u64> = None;
    let mut shared_bytes = 0u64;
    let mut depth = 0u32;
    let result = (|| -> Result<()> {
        loop {
            let before = reader.get_ref().consumed;
            let mut step = limits.event;
            if let Some(start) = cell_start {
                step = step.min(limits.cell.saturating_sub(before.saturating_sub(start)));
            }
            if let Some(start) = row_start {
                step = step.min(limits.row.saturating_sub(before.saturating_sub(start)));
            }
            reader.get_mut().step = step;
            buffer.clear();
            let event = reader
                .read_event_into(&mut buffer)
                .map_err(|error| invalid(format!("Vendor XML safety preflight: {error}")))?;
            match event {
                Event::DocType(_) => {
                    return Err(invalid("Vendor OOXML DTD declarations are refused"));
                }
                Event::Decl(declaration) => {
                    if let Some(encoding) = declaration.encoding() {
                        let encoding = encoding.map_err(invalid)?;
                        if !encoding.eq_ignore_ascii_case(b"utf-8")
                            && !encoding.eq_ignore_ascii_case(b"utf8")
                        {
                            return Err(invalid("OOXML safety preflight requires UTF-8 XML parts"));
                        }
                    }
                }
                Event::Start(tag) => {
                    depth += 1;
                    if depth > 128 {
                        return Err(invalid("Vendor XML nesting exceeds safety budget"));
                    }
                    match tag.local_name().as_ref() {
                        b"sst" => shared = true,
                        b"c" | b"si" => {
                            if cell_start.is_some() {
                                return Err(invalid("Nested OOXML cell/shared-string element"));
                            }
                            cell_start = Some(before);
                        }
                        b"row" => {
                            if row_start.is_some() {
                                return Err(invalid("Nested OOXML row"));
                            }
                            row_start = Some(before);
                        }
                        _ => {}
                    }
                }
                Event::End(tag) => {
                    depth = depth.saturating_sub(1);
                    match tag.local_name().as_ref() {
                        b"c" | b"si" => cell_start = None,
                        b"row" => row_start = None,
                        _ => {}
                    }
                }
                Event::Text(text) if shared => {
                    shared_bytes = shared_bytes.saturating_add(text.len() as u64);
                }
                Event::CData(text) if shared => {
                    shared_bytes = shared_bytes.saturating_add(text.len() as u64);
                }
                Event::Eof => break,
                _ => {}
            }
            if shared_bytes > limits.shared_text {
                return Err(invalid(
                    "Vendor sharedStrings text exceeds 64 MiB safety budget",
                ));
            }
        }
        if cell_start.is_some() || row_start.is_some() || depth != 0 {
            return Err(invalid("Unterminated OOXML cell or row"));
        }
        Ok(())
    })();
    (result, reader.get_ref().consumed)
}

fn archive(path: &Path, total_remaining: &mut u64, limits: Limits) -> Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(path)?).map_err(invalid)?;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(invalid)?;
        let name = entry.name().to_ascii_lowercase();
        let declared = entry.size();
        let allowed = limits.entry.min(*total_remaining);
        let mut input = BufReader::with_capacity(8192, entry);
        let mut prefix = 0u64;
        loop {
            let head = input.fill_buf().map_err(invalid)?;
            let whitespace = head.iter().take_while(|b| b.is_ascii_whitespace()).count();
            if whitespace == 0 {
                break;
            }
            prefix += whitespace as u64;
            input.consume(whitespace);
            if prefix > limits.event || prefix > allowed {
                *total_remaining = total_remaining.saturating_sub(prefix);
                return Err(invalid(
                    "Vendor ZIP/XML leading whitespace exceeds safety budget",
                ));
            }
        }
        let head = input.fill_buf().map_err(invalid)?;
        let xml_content = head.starts_with(b"<")
            || head.starts_with(b"\xef\xbb\xbf<")
            || head.starts_with(&[0xff, 0xfe])
            || head.starts_with(&[0xfe, 0xff])
            || head.starts_with(&[0, b'<'])
            || head.starts_with(&[b'<', 0]);
        let (result, consumed) = if name.ends_with(".xml") || name.ends_with(".rels") || xml_content
        {
            let (result, count) = xml(
                input,
                allowed - prefix,
                name.ends_with("sharedstrings.xml"),
                limits,
            );
            (result, count + prefix)
        } else {
            let mut guarded = Guarded {
                inner: input,
                step: (allowed - prefix).saturating_add(1),
                entry: (allowed - prefix).saturating_add(1),
                consumed: 0,
            };
            let result = io::copy(&mut guarded, &mut io::sink());
            let count = guarded.consumed + prefix;
            match result {
                Ok(_) if count <= allowed => (Ok(()), count),
                Ok(_) => (
                    Err(invalid("Vendor ZIP decompression safety budget exceeded")),
                    count,
                ),
                Err(error) => (Err(invalid(error)), count),
            }
        };
        *total_remaining = total_remaining.saturating_sub(consumed);
        result?;
        if consumed != declared {
            return Err(invalid(
                "Vendor ZIP declared size differs from actual decoded bytes",
            ));
        }
    }
    Ok(())
}

pub(super) fn preflight(path: &Path, total_remaining: &mut u64) -> Result<()> {
    archive(path, total_remaining, Limits::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn zip_file(path: &Path, name: &str, body: &[u8]) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        zip.start_file(
            name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        zip.write_all(body).unwrap();
        zip.finish().unwrap();
    }
    fn small() -> Limits {
        Limits {
            entry: 8192,
            event: 1024,
            cell: 1024,
            row: 2048,
            shared_text: 2048,
        }
    }
    #[test]
    fn event_is_limited_before_a_giant_text_buffer_is_allocated() {
        let text = format!(
            "<worksheet><row><c><is><t>{}</t></is></c></row></worksheet>",
            "A".repeat(4096)
        );
        let mut input = text.as_bytes();
        let (result, consumed) = xml(&mut input, 8192, false, small());
        assert!(result.is_err());
        assert!(consumed <= 1100);
    }
    #[test]
    fn rich_text_cannot_reset_the_per_cell_budget() {
        let runs = "<r><t>abcdefgh</t></r>".repeat(100);
        let text = format!("<worksheet><row><c><is>{runs}</is></c></row></worksheet>");
        let (result, consumed) = xml(text.as_bytes(), 8192, false, small());
        assert!(result.is_err());
        assert!(consumed <= 1100);
    }
    #[test]
    fn dtd_is_rejected_and_budget_is_shared_across_archive_members() {
        assert!(
            xml(b"<!DOCTYPE x><x/>".as_slice(), 8192, false, small())
                .0
                .is_err()
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.zip");
        zip_file(&path, "data.bin", &[1; 4096]);
        let mut remaining = 1024;
        assert!(archive(&path, &mut remaining, small()).is_err());
        assert_eq!(remaining, 0);
    }
    #[test]
    fn shared_string_aggregate_is_bounded_across_many_small_elements() {
        let text = format!("<sst>{}</sst>", "<si><t>abcdefgh</t></si>".repeat(300));
        assert!(xml(text.as_bytes(), 16384, true, small()).0.is_err());
    }
}
