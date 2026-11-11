# WAV and Vorbis Metadata extraction

The scanner reads embedded Metadata without rewriting source audio. WAV/BWF and AIFF extraction examine bounded container chunks; FLAC/Ogg read bounded headers and comments; MP3 validates actual Layer III frame headers and optional ID3 tags. The database and desktop expose raw source groups, canonical fields and a preferred source description. Library tags, notes, ratings, favorites and colors remain user annotations and are never assigned from embedded fields or vendor tables. Ordinary local writes still target schema8; completed schema9 is a separate server migration contract, not an extraction-version upgrade.

## Supported sources

| Container | Extracted data |
| --- | --- |
| BWF `bext` | Description, originator/reference, origination date/time, time reference, version, UMID and coding history |
| RIFF `LIST/INFO` | All text subchunks, including ICMT, INAM, IKEY, IART, IPRD, IGNR and ICOP |
| `iXML` | Direct BWFXML scene/take/tape/flags plus arbitrary nested USER/vendor fields and attributes |
| `axml` / `aXML` | XML text and attributes, including vendor extensions |
| `_PMX` / `XMP ` | XMP/RDF text, attribute properties and repeated array values |
| `id3 ` / `ID3 ` | ID3v2.2/2.3/2.4 text frames, custom TXXX fields and COMM comments; extended headers and unsynchronisation |
| `SMED` | Legacy Soundminer Blowfish/ECB wrapper containing UTF-16 XML; library/vendor text fields, excluding embedded pictures and Waveform caches |
| Native FLAC | STREAMINFO and Vorbis comments, including repeated text values and canonical Metadata mappings |
| Ogg Vorbis / Ogg FLAC | Identification headers and Vorbis comments across bounded Ogg header pages; `.ogg` / `.oga` source scanning |
| Ogg Opus | Comment probing support only; Opus is not enabled for ordinary scanning or bundled playback |
| MPEG Layer III MP3 | Bounded ID3v2.2/2.3/2.4 text, COMM/TXXX, opt-in APIC artwork and ID3v1 fallback; exact complete-frame accounting, Xing/Info/VBRI and validated gapless trim |
| AIFF / supported AIFC | NAME, AUTH, ANNO, copyright, COMT text/context and ID3; signed PCM 1–32 valid bits, AIFC NONE/twos/sowt and big-endian fl32/fl64 |

XML uses a real parser for namespaces, entities, numeric character references and CDATA. UTF-8 and UTF-16 BOMs and declared XML encodings are decoded. INFO and fixed BWF text use valid UTF-8 first, otherwise Windows-1252; ambiguous unmarked legacy code pages cannot be inferred reliably. ID3 supports its Latin-1, UTF-16 and UTF-8 text encodings.

`attributes.wav_metadata` owns the WAV extraction marker (`version`, currently 4), bounded chunk inventory, source groups (`riff_info`, `ixml`, `axml`, `xmp`, `id3`, `smed`), normalized `fields` and optional `warnings`. Non-WAV comment extraction additionally uses its independent `vorbis_version` marker (currently 1) and a `vorbis` source dictionary. XML keys use expanded namespace names and paths; distinct repeated values become arrays. Unknown vendor text remains available under its source path. Normalized fields include description, title, category/subcategory, CatID, keywords, library, designer, artist, publisher, copyright, microphone and location.

Preferred descriptions use SMED, iXML, XMP, aXML, INFO and ID3 in that order, then BWF as fallback. Conflicting values remain in the source groups and summary arrays; the inspector shows the first preferred value. `bext_description` retains its original BWF meaning.

Vorbis keys are matched without case distinctions and retained in uppercase. `DESCRIPTION` is preferred over `COMMENT` / `COMMENTS` for the canonical description; title, artist, keywords and other known aliases map into the same `fields` contract used by WAV Metadata. Repeated distinct values remain arrays. Picture payload fields (`METADATA_BLOCK_PICTURE`, `COVERART`, `COVERARTMIME`) are excluded from this text extraction and search projection. Invalid optional comment text produces diagnostics; it is not copied into user notes.

The ordinary source-extension set is `.wav`, `.bwf`, `.flac`, `.ogg`, `.oga`, `.mp3`, `.aif` and `.aiff`, defined once in core. Actual bytes identify the container even for an opaque remote `.wav` identity. `.bwf` uses the existing WAV/BWF extractor. An extension does not make an unsupported or corrupt codec playable. AIFF PCM stores signed samples with explicit byte order and storage width; AIFC block-aligned sound data and other compression types remain explicitly unsupported. Metadata probing and playback-codec support remain separate, including the Opus boundary above. Relinking resolves the real supported audio target beneath allowed scan roots.

The first successful write of an extended source publishes required feature `audio_sources:1` in the same transaction as its records. The exact formats are `ale-s8-cld1-t3-m2-p1-a1-af1` and `ale-s9-cld1-t3-m2-p1-a1-sd1-af1`; schema numbers stay 8/9. Empty and original-format libraries retain their base format. New clients read both variants; older clients must upgrade before opening an af1 library. Schema-9 legacy projections keep only their historical WAV representation, exclude extended containers even behind a `.wav` suffix, remove the af1 requirement and preserve the original schema-8 ID. Migration, rollback, import and directory-index rebuild preserve an existing af1 requirement. This cannot retroactively stop already-open older SQLite instances, including readers and writers; close older instances before enabling new sources in a shared local library.

MP3 bit depth is unknown/not applicable (`0`), rather than invented PCM precision. Local probing validates every complete frame; absent Xing/Info/VBRI or encoder trim information yields exact coded sample counts without claiming to recover missing encoder delay. Scans store a versioned `source_audio_layout` with the exact frame count, byte range and validated trim. Original remote MP3 auditions validate and reuse that owned layout plus the source frame header, avoiding a network-wide recount. Decoder-only trusted headers prevent bad optional duration tags from overriding the validated timeline; source audio and caches are never rewritten. AIFF auditions read COMM/SSND layout without traversing sample bytes or trailing Metadata. Waveform and seek share the same decoders.

## Controlled vendor Metadata

Vendor CSV, BIFF XLS and OOXML XLSX imports are separate from embedded extraction and user-annotation import. Preview reads owned source copies, detects files/sheets/encoding and matches existing library records; human mappings and ambiguous/fuzzy confirmations are reviewed before commit. The desktop supports writable local libraries and capability/management-gated remote imports through `DataProvider`. Remote parsing and writes run on the server, bound to the actual library identity. See the [library import workflow](Rust_Library_Management.md#厂商-csv--xls--xlsx-资料导入) and [server API](api/README.md).

Committed provenance is stored separately from embedded groups and mirrored into `attributes.vendor_metadata.fields`. Filename and relative-path mappings identify records; description, keywords, category/subcategory, library, publisher, designer and custom canonical fields remain vendor values. They never become assignments to user tags/notes, and they do not rewrite `wav_metadata.vorbis`, XML, BWF or other embedded originals. In the manual import workflow, empty/unmapped columns do not erase an existing value; replacing existing vendor fields requires the explicit overwrite option. Automatic source refresh follows the completeness and preservation rules below.

The preferred display description uses controlled vendor description first, then the embedded description/BWF fallback. The declared library/album name can use the vendor library field. Read-only source tags combine vendor and embedded keywords/categories with duplicates removed; list display can combine those with user tags while `AudioFile.tags` remains the actual user-authored value. Tag filters can also match derived source tags without assigning them to user annotations. Custom vendor fields remain available in the mirror and source search text.

commit uses durable batch checkpoints and an idempotency receipt. A paused commit resumes the same batch/key; replay does not import again. A confirmed rollback removes that batch's contribution and reprojects retained batches against current embedded Metadata. It preserves later user edits and later scans rather than restoring an obsolete whole-record JSON image. Main-library backups retain committed provenance and undo history; owned original tables, in-progress preview stores and mapping templates live beside the library and must be retained separately when continuation/template recovery is required.

## Rescanning and search

Ordinary local, CLI and server scans also discover same-directory CSV/TSV/XLS/XLSX tables.
After audio-header batches drain, the shared scanner parses the tables and associates rows only
with a unique WAV in the table's own directory that was confirmed by this scan. It does not use
the manual importer's global filename, stem or fuzzy fallbacks. A row naming an external,
parent or child directory is left for manual import rather than matched by basename alone.
Recognized headers and all supported sheets use the same bounded parser as manual preview.

Automatic sources retain their raw provenance while the projected values are compared with
embedded Metadata and retained table contributions, removing equivalent Unicode, whitespace and case variants.
Merged tag display also removes duplicates with user tags; the independent vendor source remains
available if the user later removes an annotation. Different source values remain available;
user annotations and embedded originals remain unchanged.
Rescanning the same table is idempotent. A changed table whose rows can all be associated safely
replaces only its own earlier contribution. A table-only change is processed even when every WAV header is unchanged.
Newly scanned WAVs are matched even if the table bytes have not changed. Missing, unreadable,
malformed or unrecognizable tables retain their last valid contribution; a missing NAS file is
never interpreted as an instruction to erase Metadata. Manual import remains the route for
custom mappings and unresolved matches.

An audio rescan re-evaluates retained automatic sources against the new embedded values even
when their table is absent or unreadable. Previously equivalent table text therefore remains
searchable if the WAV's embedded Metadata changes. When any row is unmatched or ambiguous,
safe new matches are imported while all of that table's prior contributions are retained.
This also protects sources when a workbook has been reordered or rows inserted; row coordinates
are not proof that an earlier contribution was deleted. Old values can remain until the table
matches completely or an explicit source rollback removes its contributions. Empty/header-only
tables also retain the last valid source.

Discovery uses bounded channels and an owned disk-backed path spool, not an unbounded list of
all library tables. Parsing completes in owned staging before any Metadata is written. Bounded
transactions retain committed work on cancellation; a later scan re-evaluates the source safely.
The live/final `ScanProgress.vendor_metadata` reports table, match, update, unchanged, ambiguous
and unmatched counts separately from WAV-header counts. Server scans keep their management and
library-identity checks. Table-only projection changes invalidate the snapshot revision, so
clients refresh the read-only s8/s9 snapshot and can search the new text offline. This does not
change the ordinary local schema8 writer or implicitly start a schema9 migration.

Playback and Waveform decoding use a separate lightweight header path that seeks past embedded Metadata. Audition does not repeatedly decrypt SMED artwork or parse large XML/ID3 chunks; full extraction runs only when scanning or explicitly probing Metadata.

An ordinary incremental scan checks both the file fingerprint and the actual container's extraction version: WAV/BWF uses `WAV_METADATA_VERSION`, FLAC/Ogg use `VORBIS_METADATA_VERSION`, and MP3/AIFF have independent revision-1 markers. Filename aliases do not select the wrong extractor. Old RF64/BW64 container identities lacking numeric `container_version:1` are refreshed once to distinguish their actual magic; ordinary RIFF records retain their existing version and unchanged fast path. An older extraction is re-probed once even if size and timestamp are unchanged. There is no implicit startup scan.

Successful rescans replace the extractor-owned source group, clear removed source fields and old source search terms, and preserve the controlled vendor mirror, unrelated extension Metadata and all user annotations. A failed audio probe leaves the existing record unchanged. The existing `bwf_description_tokenized` derived column carries BWF plus public extracted and controlled vendor text; this does not convert an ordinary local schema8 library into schema9. FTS5 and Chinese segmentation find descriptions, categories, keywords and custom fields without copying them into user tags. Search rebuilds and owned-cache Metadata patches use the same derivation, so saving user notes does not drop imported/Vorbis terms.

Search scopes distinguish these sources: Main fields retains the legacy main-text behavior (filename, Metadata, user tags and notes); Filename restricts matching to filename fields; Path requires every query term to match directory/ancestor text; All permits each term to match main fields or directory text. Imported and Vorbis words therefore belong to Main fields/All, not Filename or Path merely because the record has those fields. Scope and ordinary filters are independent; see the [path-search storage contract](api/Search_Storage_B.md).

## Limits and diagnostics

Actual source values are preserved. For example, a supplied commercial-library sample stores `#BEZUG!` in Keywords and TrackTitle and an empty/NSNull Designer. These are source contents, not failed extraction; FXName and XMP title still provide its meaningful title. Manufacturer and Publisher remain distinct fields.

Metadata is limited to 2 MiB per public chunk, 16 MiB per SMED chunk and 32 MiB per file, XML to 16,384 nodes and 32 levels, with bounded field paths and search text. SMED has a larger bound because modern sound libraries include several MiB of UTF-16 base64 artwork. XML DTDs/external entities are rejected. Binary ID3 frames are skipped; compressed/encrypted ID3 text is reported as unsupported. Malformed optional containers generate warnings while valid audio and other successfully parsed Metadata remain accessible. Structurally truncated WAV chunks still fail the probe, preserving the previous database record.

RIFF chunk headers, payloads and odd-length padding must fit inside the declared parent container, which must itself fit inside the physical file. Bytes outside that parent are ignored with a bounded Metadata warning, so private trailers such as `ID3Helper` are not mistaken for RIFF chunks. Valid Metadata after `data` remains readable when it is inside the parent. RF64/BW64 obtain the parent and sentinel `data` lengths from a valid first `ds64` chunk; its size-table bounds are validated without allocating the table. Resolving non-`data` sentinel-sized chunks through that table remains unsupported and is explicitly rejected. Playback still stops its lightweight layout read once `fmt ` and `data` are known.

The RIFF boundary correction itself does not raise the global WAV extraction version. An incremental scan retries files that never entered the library, because they have no successful fingerprint to skip. Full refresh continues preserving unrelated Metadata and user annotations. Synthetic regressions cover parent boundaries, MP3/AIFF extraction and actual decoding, header-only network layout reads, af1 transactions and scan preservation. Source-module tests and `format_pipeline_tests.rs`, database `audio_sources_formats.rs` and server `audio_source_formats.rs` are the focused entry points; their presence does not claim an execution or original-library acceptance result.

Vorbis extraction has separate bounds: at most 4,096 comments, 16 KiB per retained value and 1 MiB of comment text, with bounded header/page traversal. Excess data and malformed optional comment blocks produce warnings rather than unlimited allocation. The source file remains unchanged. The public source-search projection is also bounded; it excludes artwork/base64-picture fields and diagnostics. Vendor imports have file/row, mapped-text and reply-byte limits documented in the import workflow; a bounded candidate shortlist is not a limit on the full disk-backed match set.

Soundminer `SMED` is private and version-dependent. This reader supports the observed legacy wrapper: a big-endian decoded length followed by Blowfish/ECB blocks with the published compatibility key, then BOM-marked XML. It validates lengths before decryption and applies the same XML limits. Unsupported layouts or malformed XML produce a diagnostic; support for every Soundminer version is not claimed. The cipher exists solely for reading this legacy format, never for application security.

The 34 local P-51 Mustang WAV fixtures contain `fmt `, `bext`, `data`, `iXML` and `SMED`; their iXML contains version declarations rather than the professional library fields. The SMED reader restores Category (Vehicles), SubCategory (Aircraft), Microphone, Library and Manufacturer (BOOM Library), plus all other text fields. Embedded Picture data is excluded from the database and search index. The opt-in `real_boom_smed_metadata_fixtures` test reads these local WAVs; the release runtime regression verifies their fields and FTS5 recall in an isolated library.

On 2026-10-08 the release CLI scanned an anonymized commercial WAV sample on a network share. Every file exposed SMED, iXML, INFO, ID3 and XMP text with zero scan errors or Metadata warnings; Description, Category, SubCategory, CatID, Library, Manufacturer and VendorCategory were present in every record. The library-name search returned every sample, and a second incremental scan skipped every unchanged file. Source sizes and modification times were unchanged. The measured full scan took 45.16 seconds including CLI startup while other validation ran; this is a network-path scan with multi-MiB embedded artwork, not an SSD throughput or million-record search benchmark. Source paths, record counts and capacity are omitted from the public report. See validation data.

Current regression entry points include `audiolib-audio/src/vorbis_metadata_tests.rs`, `audiolib-db/tests/vorbis_scan.rs`, `audiolib-db/tests/vendor_import.rs` and server `tests/search_metadata_complete.rs`. Their fixtures contain real small FLAC/Ogg containers and synthetic CSV/BIFF8/OOXML workbooks. They cover extraction/search, rescan preservation, imports/rollback, HTTP snapshot synchronization, cache Metadata updates and offline reopen. These are validation entry points, not a claim that a final release package or native UI acceptance has passed; execution/source identity and remaining platform/hardware coverage belong in the release report. The older WAV measurements above do not certify the new Vorbis/vendor behavior.

References: [Soundminer Metadata formats](https://info.soundminer.com/docs/about-metadata), [Soundminer XMP mappings](https://info.soundminer.com/docs/xmp), [original SMED format analysis](https://reverseengineering.stackexchange.com/questions/26241/extract-closed-format-metadata-from-audio-file), [RustCrypto Blowfish](https://docs.rs/blowfish/0.9.1/blowfish/), [ID3v2.3](https://id3.org/id3v2.3.0), [ID3v2.4 structure](https://id3.org/id3v2.4.0-structure).
