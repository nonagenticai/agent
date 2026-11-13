//! Column preview for datatable sources (B3 T7, `previewSourceColumns`): the header row, up to five sample rows and
//! the row count of a CSV or XLSX (uploaded, or fetched from the source URL). Pure functions over bytes; nothing is
//! stored. Ported from care-bus-agents@96c608c `crates/carebus-api/src/tabular.rs`; an XLSX preview also names every
//! sheet of the workbook and the one read (UI `ColumnPreview.sheets` / `.sheet`).
//!
//! XLSX is read directly (zip + quick-xml, no calamine): `xl/workbook.xml` names the sheets,
//! `xl/_rels/workbook.xml.rels` maps a sheet to its part, `xl/sharedStrings.xml` holds the shared
//! strings, and the sheet part is streamed row by row. Every byte decompressed counts against
//! [`MAX_XLSX_EXPANDED_BYTES`] and the archive may hold at most [`MAX_XLSX_ENTRIES`] parts, so a zip
//! bomb ends in [`PreviewError::TooLarge`], not in memory exhaustion.
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, Cursor, Read};

use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use serde::Serialize;

pub const MAX_SAMPLE_ROWS: usize = 5;
/// Columns a previewed table may have (CSV header width, XLSX cell column).
pub const MAX_COLUMNS: usize = 1_000;
/// Bytes of one previewed cell (header or sample) kept; longer text is cut at a character boundary. With
/// [`MAX_COLUMNS`] and [`MAX_SAMPLE_ROWS`] this bounds a preview to about 6 MiB of text.
pub const MAX_CELL_BYTES: usize = 1024;
/// Shared-string entries (`<si>`) an XLSX may hold.
pub const MAX_SHARED_STRINGS: usize = 1_000_000;
/// Bytes of shared-string text an XLSX may hold.
pub const MAX_SHARED_STRINGS_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TableFormat {
    Csv,
    Xlsx,
}

impl TableFormat {
    /// From the uploaded file name's extension (case-insensitive).
    pub fn from_file_name(name: &str) -> Option<Self> {
        let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
        match ext.as_str() {
            "csv" | "txt" => Some(Self::Csv),
            "xlsx" => Some(Self::Xlsx),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ColumnPreview {
    pub format: TableFormat,
    pub columns: Vec<String>,
    pub sample_rows: Vec<Vec<String>>,
    pub row_count: u64,
    /// XLSX: every sheet of the workbook, in workbook order; empty for a CSV.
    pub sheets: Vec<String>,
    /// XLSX: the sheet read; `None` for a CSV.
    pub sheet: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PreviewError {
    #[error("the file has no header row")]
    Empty,
    #[error("the file is not a readable {0}")]
    Unreadable(&'static str),
    #[error("the workbook has no sheet named \"{0}\"")]
    SheetNotFound(String),
    #[error("the workbook is too large to preview: {0}")]
    TooLarge(&'static str),
    #[error("the table has more than 1,000 columns")]
    TooManyColumns,
}

/// `s` cut to at most [`MAX_CELL_BYTES`], at a character boundary.
fn cut(s: &str) -> &str {
    if s.len() <= MAX_CELL_BYTES {
        return s;
    }
    let mut end = MAX_CELL_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// [`cut`] in place.
fn cut_owned(s: &mut String) {
    let n = cut(s).len();
    s.truncate(n);
}

/// Blank header cells become `column_<n>` (1-based); a repeated name gets `_2`, `_3`, ... (the first suffix
/// not already taken). Linear: a set of the names taken and, per name, the next suffix to try.
fn header(cells: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(cells.len());
    let mut taken: HashSet<String> = HashSet::with_capacity(cells.len());
    let mut next: HashMap<String, usize> = HashMap::new();
    for (i, c) in cells.into_iter().enumerate() {
        let base = match c.trim() {
            "" => format!("column_{}", i + 1),
            t => t.to_string(),
        };
        let name = if taken.contains(&base) {
            let n = next.entry(base.clone()).or_insert(2);
            let mut name = format!("{base}_{n}");
            while taken.contains(&name) {
                *n += 1;
                name = format!("{base}_{n}");
            }
            *n += 1;
            name
        } else {
            base
        };
        taken.insert(name.clone());
        out.push(name);
    }
    out
}

/// Streams the rows once and skips blank rows. With `header_row` the first non-blank row is the header;
/// without it every row is data and the columns are `column_1..N`, N the widest row in the whole file (strata_ingest
/// names every row's cells `column_N`, so a wider row past the sample still has its column). Every data
/// row is counted and at most `MAX_SAMPLE_ROWS` are kept. Memory stays bounded whatever the input size.
fn finish(
    format: TableFormat,
    mut rows: impl Iterator<Item = Result<Vec<String>, PreviewError>>,
    header_row: bool,
) -> Result<ColumnPreview, PreviewError> {
    let blank = |r: &Vec<String>| r.iter().all(|c| c.trim().is_empty());
    if !header_row {
        let (sample_rows, row_count, width) = data_rows(rows, None)?;
        if row_count == 0 {
            return Err(PreviewError::Empty);
        }
        if width > MAX_COLUMNS {
            return Err(PreviewError::TooManyColumns);
        }
        let columns: Vec<String> = (1..=width).map(|i| format!("column_{i}")).collect();
        let sample_rows = sample_rows
            .into_iter()
            .map(|mut r| {
                r.resize(width, String::new());
                r
            })
            .collect();
        return Ok(ColumnPreview {
            format,
            columns,
            sample_rows,
            row_count,
            sheets: Vec::new(),
            sheet: None,
        });
    }
    let mut head = None;
    for r in rows.by_ref() {
        let r = r?;
        if !blank(&r) {
            head = Some(r);
            break;
        }
    }
    let mut head = head.ok_or(PreviewError::Empty)?;
    if head.len() > MAX_COLUMNS {
        return Err(PreviewError::TooManyColumns);
    }
    head.iter_mut().for_each(cut_owned);
    let columns = header(head);
    let (sample_rows, row_count, _) = data_rows(rows, Some(columns.len()))?;
    Ok(ColumnPreview {
        format,
        columns,
        sample_rows,
        row_count,
        sheets: Vec::new(),
        sheet: None,
    })
}

/// The non-blank rows: all counted, the first `MAX_SAMPLE_ROWS` kept with their cells cut, and padded or
/// truncated to `width` when the header fixes it.
fn data_rows(
    rows: impl Iterator<Item = Result<Vec<String>, PreviewError>>,
    width: Option<usize>,
) -> Result<(Vec<Vec<String>>, u64, usize), PreviewError> {
    let blank = |r: &Vec<String>| r.iter().all(|c| c.trim().is_empty());
    let mut sample_rows = Vec::new();
    let mut row_count = 0u64;
    let mut widest = 0usize;
    for r in rows {
        let mut r = r?;
        if blank(&r) {
            continue;
        }
        row_count += 1;
        widest = widest.max(r.len());
        if sample_rows.len() < MAX_SAMPLE_ROWS {
            if let Some(w) = width {
                r.resize(w, String::new());
            }
            r.iter_mut().for_each(cut_owned);
            sample_rows.push(r);
        }
    }
    Ok((sample_rows, row_count, widest))
}

/// The delimiter (`,` `;` or tab) that splits the first line into the most fields; `,` when none occurs.
fn sniff_delimiter(bytes: &[u8]) -> u8 {
    let first = bytes.split(|b| *b == b'\n').next().unwrap_or_default();
    let count = |d: u8| first.iter().filter(|b| **b == d).count();
    let mut best = (b',', count(b','));
    for &d in b";\t" {
        if count(d) > best.1 {
            best = (d, count(d));
        }
    }
    best.0
}

/// Column preview of a CSV; `header_row` false reads the first row as data (see [`finish`]).
pub fn preview_csv(bytes: &[u8], header_row: bool) -> Result<ColumnPreview, PreviewError> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(sniff_delimiter(bytes))
        .from_reader(bytes);
    let rows = rdr.byte_records().map(|rec| {
        let rec = rec.map_err(|_| PreviewError::Unreadable("CSV file"))?;
        // refused on the raw field count, before any field becomes a String or a column name
        if rec.len() > MAX_COLUMNS {
            return Err(PreviewError::TooManyColumns);
        }
        Ok(rec
            .iter()
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect::<Vec<_>>())
    });
    finish(TableFormat::Csv, rows, header_row)
}

// ---------------------------------------------------------------------------------------------
// XLSX

/// Total bytes an XLSX preview may decompress, over all parts it reads.
pub const MAX_XLSX_EXPANDED_BYTES: u64 = 50 * 1024 * 1024;
/// Parts (zip entries) an XLSX may hold.
pub const MAX_XLSX_ENTRIES: usize = 10_000;
const XLSX: PreviewError = PreviewError::Unreadable("XLSX file");

type Archive<'d> = zip::ZipArchive<Cursor<&'d [u8]>>;

/// The decompression allowance shared by every part read for one preview.
struct Budget {
    left: Cell<u64>,
    blown: Cell<bool>,
}

impl Budget {
    fn new(bytes: u64) -> Self {
        Self {
            left: Cell::new(bytes),
            blown: Cell::new(false),
        }
    }

    /// A failure while reading: the cap when it was hit, else an unreadable file.
    fn error(&self) -> PreviewError {
        if self.blown.get() {
            PreviewError::TooLarge("it expands beyond 50 MiB")
        } else {
            XLSX
        }
    }
}

/// A reader that charges every byte it yields to the [`Budget`] and fails once it is spent.
struct Capped<'b, R> {
    inner: R,
    budget: &'b Budget,
}

impl<R: Read> Read for Capped<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        let left = self.budget.left.get();
        if n as u64 > left {
            self.budget.blown.set(true);
            return Err(io::Error::other("XLSX expansion cap reached"));
        }
        self.budget.left.set(left - n as u64);
        Ok(n)
    }
}

type PartReader<'a, 'd> = Reader<BufReader<Capped<'a, zip::read::ZipFile<'a, Cursor<&'d [u8]>>>>>;

/// An XML reader over part `name` (exact, then ASCII case-insensitive); `None` when absent.
fn open_part<'a, 'd>(
    zip: &'a mut Archive<'d>,
    budget: &'a Budget,
    name: &str,
) -> Result<Option<PartReader<'a, 'd>>, PreviewError> {
    let index = zip.index_for_name(name).or_else(|| {
        zip.file_names()
            .position(|n| n.is_ok_and(|n| n.eq_ignore_ascii_case(name)))
    });
    let Some(index) = index else {
        return Ok(None);
    };
    let file = zip.by_index(index).map_err(|_| XLSX)?;
    Ok(Some(Reader::from_reader(BufReader::new(Capped {
        inner: file,
        budget,
    }))))
}

/// The value of the attribute whose local name is `local` (prefixed when `prefixed`, e.g. `r:id`).
fn attr(e: &BytesStart<'_>, local: &str, prefixed: bool) -> Result<Option<String>, PreviewError> {
    for a in e.attributes() {
        let a = a.map_err(|_| XLSX)?;
        if a.key.local_name().as_ref() == local && a.key.prefix().is_some() == prefixed {
            let v = a
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|_| XLSX)?;
            return Ok(Some(v.into_owned()));
        }
    }
    Ok(None)
}

/// Appends a text-bearing event (text, CDATA, entity or character reference) to `out`.
/// Returns whether the event carried text.
fn push_text(ev: &Event<'_>, out: &mut String) -> Result<bool, PreviewError> {
    match ev {
        Event::Text(t) => out.push_str(&t.xml10_content()),
        Event::CData(c) => out.push_str(&c.xml10_content()),
        Event::GeneralRef(r) => {
            if r.is_char_ref() {
                out.push(r.resolve_char_ref().ok().flatten().ok_or(XLSX)?);
            } else {
                let name = r.xml10_content();
                out.push_str(quick_xml::escape::resolve_predefined_entity(&name).ok_or(XLSX)?);
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// `(sheet name, relationship id)` in workbook order.
fn workbook_sheets(
    r: &mut PartReader<'_, '_>,
    budget: &Budget,
) -> Result<Vec<(String, String)>, PreviewError> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf).map_err(|_| budget.error())? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "sheet" => {
                let name = attr(&e, "name", false)?.ok_or(XLSX)?;
                let rid = attr(&e, "id", true)?.ok_or(XLSX)?;
                out.push((name, rid));
            }
            Event::Eof => return Ok(out),
            _ => {}
        }
        buf.clear();
    }
}

/// The part path (inside the zip) of relationship `rid` in `xl/_rels/workbook.xml.rels`.
fn relationship_target(
    r: &mut PartReader<'_, '_>,
    budget: &Budget,
    rid: &str,
) -> Result<Option<String>, PreviewError> {
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf).map_err(|_| budget.error())? {
            Event::Start(e) | Event::Empty(e)
                if e.local_name().as_ref() == "Relationship"
                    && attr(&e, "Id", false)?.as_deref() == Some(rid) =>
            {
                return Ok(attr(&e, "Target", false)?.map(|t| part_path(&t)));
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
        buf.clear();
    }
}

/// A relationship target resolved against `xl/` (absolute targets start at the package root).
fn part_path(target: &str) -> String {
    let joined = match target.strip_prefix('/') {
        Some(abs) => abs.to_string(),
        None => format!("xl/{target}"),
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// The shared-string table, packed into one buffer: string `i` is `text[ends[i-1]..ends[i]]`.
#[derive(Default)]
struct SharedStrings {
    text: String,
    ends: Vec<usize>,
}

impl SharedStrings {
    fn push_end(&mut self) -> Result<(), PreviewError> {
        if self.ends.len() >= MAX_SHARED_STRINGS {
            return Err(PreviewError::TooLarge(
                "it holds more than 1,000,000 shared strings",
            ));
        }
        self.ends.push(self.text.len());
        Ok(())
    }

    fn get(&self, i: usize) -> Option<&str> {
        let end = *self.ends.get(i)?;
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        Some(&self.text[start..end])
    }

    /// `<si>` items: plain `<t>` or rich-text runs `<r><t>`; phonetic hints (`<rPh>`) are skipped.
    fn read(r: &mut PartReader<'_, '_>, budget: &Budget) -> Result<Self, PreviewError> {
        let mut s = Self::default();
        let mut buf = Vec::new();
        let (mut in_t, mut in_rph) = (false, false);
        loop {
            let ev = r.read_event_into(&mut buf).map_err(|_| budget.error())?;
            match &ev {
                Event::Start(e) => match e.local_name().as_ref() {
                    "t" => in_t = true,
                    "rPh" => in_rph = true,
                    _ => {}
                },
                Event::End(e) => match e.local_name().as_ref() {
                    "t" => in_t = false,
                    "rPh" => in_rph = false,
                    "si" => s.push_end()?,
                    _ => {}
                },
                Event::Empty(e) if e.local_name().as_ref() == "si" => s.push_end()?,
                Event::Eof => return Ok(s),
                _ if in_t && !in_rph => {
                    push_text(&ev, &mut s.text)?;
                    if s.text.len() > MAX_SHARED_STRINGS_BYTES {
                        return Err(PreviewError::TooLarge("its shared strings exceed 16 MiB"));
                    }
                }
                _ => {}
            }
            buf.clear();
        }
    }
}

/// 0-based column of a cell reference's letters (`A1` → 0, `AA7` → 26); `None` without letters.
fn column_index(cell_ref: &str) -> Option<usize> {
    let letters = cell_ref.bytes().take_while(u8::is_ascii_alphabetic);
    let mut col = 0usize;
    let mut any = false;
    for b in letters {
        any = true;
        col = col
            .checked_mul(26)?
            .checked_add(usize::from(b.to_ascii_uppercase() - b'A') + 1)?;
    }
    any.then(|| col - 1)
}

/// The cell being read: its column, type attribute (`t`) and raw value text.
struct XlsxCell {
    col: usize,
    kind: Option<String>,
    raw: String,
}

/// The rows of one worksheet part, streamed. Cells land at their referenced column, so sparse rows
/// keep their shape; a row is as wide as its last non-empty cell.
struct SheetRows<'a, 'd> {
    reader: PartReader<'a, 'd>,
    budget: &'a Budget,
    strings: &'a SharedStrings,
    buf: Vec<u8>,
    done: bool,
}

/// A finished cell's display text: shared strings resolved, booleans as `TRUE` / `FALSE`. A shared string is
/// copied only up to [`MAX_CELL_BYTES`]: one long string referenced by every cell must not be copied whole each time.
fn cell_value(strings: &SharedStrings, c: XlsxCell) -> Result<String, PreviewError> {
    Ok(match c.kind.as_deref() {
        Some("s") => {
            let i: usize = c.raw.trim().parse().map_err(|_| XLSX)?;
            cut(strings.get(i).ok_or(XLSX)?).to_string()
        }
        Some("b") => match c.raw.trim() {
            "1" => "TRUE".into(),
            "0" => "FALSE".into(),
            _ => c.raw,
        },
        // inlineStr, str (formula result), e (error), n / absent (number): the text as stored
        _ => c.raw,
    })
}

impl SheetRows<'_, '_> {
    fn next_row(&mut self) -> Result<Option<Vec<String>>, PreviewError> {
        let mut row: Option<Vec<String>> = None;
        let mut cell: Option<XlsxCell> = None;
        let mut next_col = 0usize;
        let (mut in_v, mut in_is, mut in_t, mut in_rph) = (false, false, false, false);
        loop {
            self.buf.clear();
            let ev = self
                .reader
                .read_event_into(&mut self.buf)
                .map_err(|_| self.budget.error())?;
            match &ev {
                Event::Start(e) | Event::Empty(e) => {
                    let empty = matches!(ev, Event::Empty(_));
                    match e.local_name().as_ref() {
                        "row" if empty => return Ok(Some(Vec::new())),
                        "row" => {
                            row = Some(Vec::new());
                            next_col = 0;
                        }
                        "c" if row.is_some() => {
                            let col = match attr(e, "r", false)? {
                                Some(r) => column_index(&r).unwrap_or(next_col),
                                None => next_col,
                            };
                            if col >= MAX_COLUMNS {
                                return Err(PreviewError::TooManyColumns);
                            }
                            next_col = col + 1;
                            if !empty {
                                cell = Some(XlsxCell {
                                    col,
                                    kind: attr(e, "t", false)?,
                                    raw: String::new(),
                                });
                            }
                        }
                        "v" if !empty => in_v = true,
                        "is" if !empty => in_is = true,
                        "t" if !empty => in_t = true,
                        "rPh" if !empty => in_rph = true,
                        _ => {}
                    }
                }
                Event::End(e) => match e.local_name().as_ref() {
                    "v" => in_v = false,
                    "is" => in_is = false,
                    "t" => in_t = false,
                    "rPh" => in_rph = false,
                    "c" => {
                        if let (Some(c), Some(r)) = (cell.take(), row.as_mut()) {
                            let col = c.col;
                            let v = cell_value(self.strings, c)?;
                            if !v.is_empty() {
                                if r.len() <= col {
                                    r.resize(col + 1, String::new());
                                }
                                r[col] = v;
                            }
                        }
                    }
                    "row" => return Ok(Some(row.take().unwrap_or_default())),
                    _ => {}
                },
                Event::Eof => return Ok(None),
                _ => {
                    if let Some(c) = cell.as_mut()
                        && (in_v || (in_is && in_t && !in_rph))
                    {
                        push_text(&ev, &mut c.raw)?;
                        // an inline value longer than a preview cell is cut as it streams in
                        if c.raw.len() > 4 * MAX_CELL_BYTES {
                            cut_owned(&mut c.raw);
                        }
                    }
                }
            }
        }
    }
}

impl Iterator for SheetRows<'_, '_> {
    type Item = Result<Vec<String>, PreviewError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.next_row() {
            Ok(Some(r)) => Some(Ok(r)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// Column preview of one worksheet of an XLSX workbook: `sheet` by name (ASCII case-insensitive),
/// else the first sheet. The first non-blank row is the header unless `header_row` is false, as for CSV.
pub fn preview_xlsx(
    bytes: &[u8],
    sheet: Option<&str>,
    header_row: bool,
) -> Result<ColumnPreview, PreviewError> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| XLSX)?;
    if zip.len() > MAX_XLSX_ENTRIES {
        return Err(PreviewError::TooLarge("it holds more than 10,000 parts"));
    }
    let budget = Budget::new(MAX_XLSX_EXPANDED_BYTES);

    let sheets = {
        let mut r = open_part(&mut zip, &budget, "xl/workbook.xml")?.ok_or(XLSX)?;
        workbook_sheets(&mut r, &budget)?
    };
    let (chosen, rid) = match sheet.map(str::trim).filter(|s| !s.is_empty()) {
        Some(want) => sheets
            .iter()
            .find(|(n, _)| n == want)
            .or_else(|| sheets.iter().find(|(n, _)| n.eq_ignore_ascii_case(want)))
            .ok_or_else(|| PreviewError::SheetNotFound(want.to_string()))?,
        None => sheets.first().ok_or(XLSX)?,
    }
    .clone();
    let target = {
        let mut r = open_part(&mut zip, &budget, "xl/_rels/workbook.xml.rels")?.ok_or(XLSX)?;
        relationship_target(&mut r, &budget, &rid)?.ok_or(XLSX)?
    };
    let strings = match open_part(&mut zip, &budget, "xl/sharedStrings.xml")? {
        Some(mut r) => SharedStrings::read(&mut r, &budget)?,
        None => SharedStrings::default(),
    };
    let reader = open_part(&mut zip, &budget, &target)?.ok_or(XLSX)?;
    let rows = SheetRows {
        reader,
        budget: &budget,
        strings: &strings,
        buf: Vec::new(),
        done: false,
    };
    let mut preview = finish(TableFormat::Xlsx, rows, header_row)?;
    preview.sheets = sheets.into_iter().map(|(n, _)| n).collect();
    preview.sheet = Some(chosen);
    Ok(preview)
}

// ---------------------------------------------------------------------------------------------
// Whole tables (B8 T14a / T15a discovery imports, R14): the header and EVERY data row, not a preview.

/// A whole table: the header (named as in a preview) and every non-blank data row, each padded or truncated to
/// the header's width, every cell cut to [`MAX_CELL_BYTES`].
pub type Rows = (Vec<String>, Vec<Vec<String>>);

/// The first non-blank row is the header; more than `max_rows` data rows → [`PreviewError::TooLarge`].
fn all_rows(
    rows: impl Iterator<Item = Result<Vec<String>, PreviewError>>,
    max_rows: usize,
) -> Result<Rows, PreviewError> {
    let blank = |r: &Vec<String>| r.iter().all(|c| c.trim().is_empty());
    let mut head: Option<Vec<String>> = None;
    let mut out: Vec<Vec<String>> = Vec::new();
    for r in rows {
        let mut r = r?;
        if blank(&r) {
            continue;
        }
        r.iter_mut().for_each(cut_owned);
        if head.is_none() {
            if r.len() > MAX_COLUMNS {
                return Err(PreviewError::TooManyColumns);
            }
            head = Some(r);
            continue;
        }
        if out.len() >= max_rows {
            return Err(PreviewError::TooLarge(
                "it has more rows than an import takes",
            ));
        }
        out.push(r);
    }
    let columns = header(head.ok_or(PreviewError::Empty)?);
    let width = columns.len();
    for r in &mut out {
        r.resize(width, String::new());
    }
    Ok((columns, out))
}

/// Every row of a CSV (delimiter sniffed as in [`preview_csv`]); the first non-blank row is the header.
pub fn rows_csv(bytes: &[u8], max_rows: usize) -> Result<Rows, PreviewError> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(sniff_delimiter(bytes))
        .from_reader(bytes);
    let rows = rdr.byte_records().map(|rec| {
        let rec = rec.map_err(|_| PreviewError::Unreadable("CSV file"))?;
        if rec.len() > MAX_COLUMNS {
            return Err(PreviewError::TooManyColumns);
        }
        Ok(rec
            .iter()
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect::<Vec<_>>())
    });
    all_rows(rows, max_rows)
}

/// Every row of the FIRST sheet of an XLSX workbook (cell values only; formulas are not evaluated, their stored
/// result is read), with the same caps as [`preview_xlsx`]; the first non-blank row is the header.
pub fn rows_xlsx(bytes: &[u8], max_rows: usize) -> Result<Rows, PreviewError> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| XLSX)?;
    if zip.len() > MAX_XLSX_ENTRIES {
        return Err(PreviewError::TooLarge("it holds more than 10,000 parts"));
    }
    let budget = Budget::new(MAX_XLSX_EXPANDED_BYTES);
    let sheets = {
        let mut r = open_part(&mut zip, &budget, "xl/workbook.xml")?.ok_or(XLSX)?;
        workbook_sheets(&mut r, &budget)?
    };
    let rid = sheets.first().ok_or(XLSX)?.1.clone();
    let target = {
        let mut r = open_part(&mut zip, &budget, "xl/_rels/workbook.xml.rels")?.ok_or(XLSX)?;
        relationship_target(&mut r, &budget, &rid)?.ok_or(XLSX)?
    };
    let strings = match open_part(&mut zip, &budget, "xl/sharedStrings.xml")? {
        Some(mut r) => SharedStrings::read(&mut r, &budget)?,
        None => SharedStrings::default(),
    };
    let reader = open_part(&mut zip, &budget, &target)?.ok_or(XLSX)?;
    let rows = SheetRows {
        reader,
        budget: &budget,
        strings: &strings,
        buf: Vec::new(),
        done: false,
    };
    all_rows(rows, max_rows)
}

#[cfg(test)]
mod tests {
    //! The private helpers; the public vectors (CSV, XLSX, caps, zip bombs) are in tests/b3/tabular.rs.
    use super::*;

    #[test]
    fn twenty_thousand_duplicate_headers_are_named_in_linear_time() {
        let cells = vec!["a".to_string(); 20_000];
        let t = std::time::Instant::now();
        let names = header(cells);
        let took = t.elapsed();
        assert!(took < std::time::Duration::from_secs(2), "took {took:?}");
        assert_eq!(names.len(), 20_000);
        assert_eq!(names[1], "a_2");
        assert_eq!(names[19_999], "a_20000");
        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), 20_000);
        // a suffix that is itself a header is skipped, as before
        assert_eq!(
            header(vec!["a".into(), "a_2".into(), "a".into()]),
            ["a", "a_2", "a_3"]
        );
    }

    #[test]
    fn single_column_input_defaults_to_comma() {
        assert_eq!(sniff_delimiter(b"name\nAnna\n"), b',');
        assert_eq!(sniff_delimiter(b"a;b,c;d\n"), b';');
        assert_eq!(sniff_delimiter(b"a\tb\n"), b'\t');
    }

    #[test]
    fn cell_value_copies_at_most_the_cell_cap_of_a_shared_string() {
        let strings = SharedStrings {
            text: "x".repeat(4 * 1024 * 1024),
            ends: vec![4 * 1024 * 1024],
        };
        let c = XlsxCell {
            col: 0,
            kind: Some("s".into()),
            raw: "0".into(),
        };
        let v = cell_value(&strings, c).unwrap();
        assert_eq!(v.len(), MAX_CELL_BYTES, "copied {} bytes", v.len());
    }

    #[test]
    fn column_letters_and_part_paths() {
        assert_eq!(column_index("A1"), Some(0));
        assert_eq!(column_index("z9"), Some(25));
        assert_eq!(column_index("AA10"), Some(26));
        assert_eq!(column_index("XFD1"), Some(16_383));
        assert_eq!(column_index("12"), None);
        assert_eq!(
            part_path("worksheets/sheet1.xml"),
            "xl/worksheets/sheet1.xml"
        );
        assert_eq!(part_path("/xl/worksheets/s.xml"), "xl/worksheets/s.xml");
        assert_eq!(part_path("../xl/./worksheets/s.xml"), "xl/worksheets/s.xml");
    }
}
