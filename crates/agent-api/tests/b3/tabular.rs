//! B3 T7: the column-preview parser (crate::tabular) on its public API, with care-bus-agents@96c608c's test vectors
//! carried over (`crates/carebus-api/src/tabular.rs` tests): CSV header/sample/count rules, the no-header-row
//! reading, XLSX shared/inline strings, sparse cells, sheet choice (and the sheet list agent adds), and every cap
//! (columns, cell bytes, shared strings, parts, the 50 MiB expansion budget against a zip bomb). Pure functions: no
//! database, no server.
use std::io::{Cursor, Read, Write};

use agent_api::tabular::{
    MAX_CELL_BYTES, MAX_COLUMNS, MAX_SAMPLE_ROWS, MAX_SHARED_STRINGS, MAX_SHARED_STRINGS_BYTES,
    MAX_XLSX_ENTRIES, PreviewError, TableFormat, preview_csv, preview_xlsx,
};

const XLSX: PreviewError = PreviewError::Unreadable("XLSX file");

/// A zip of `(part name, contents)`, deflated, built in memory.
fn zip_of(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in parts {
        w.start_file(*name, opts).unwrap();
        w.write_all(body).unwrap();
    }
    w.finish().unwrap().into_inner()
}

const NS: &str = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

/// A minimal workbook: `sheets` are `(name, <sheetData> inner xml)`, `shared` the `<si>` texts.
fn workbook(sheets: &[(&str, &str)], shared: Option<&[&str]>) -> Vec<u8> {
    let mut wb = format!("<?xml version=\"1.0\"?><workbook {NS}><sheets>");
    let mut rels = String::from(
        r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    );
    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    for (i, (name, data)) in sheets.iter().enumerate() {
        let n = i + 1;
        wb.push_str(&format!(
            r#"<sheet name="{name}" sheetId="{n}" r:id="rId{n}"/>"#
        ));
        // the second sheet uses an absolute target, as some writers do
        let target = if n == 2 {
            format!("/xl/worksheets/sheet{n}.xml")
        } else {
            format!("worksheets/sheet{n}.xml")
        };
        rels.push_str(&format!(
            r#"<Relationship Id="rId{n}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="{target}"/>"#
        ));
        parts.push((
            format!("xl/worksheets/sheet{n}.xml"),
            format!(
                "<?xml version=\"1.0\"?><worksheet {NS}><sheetData>{data}</sheetData></worksheet>"
            )
            .into_bytes(),
        ));
    }
    wb.push_str("</sheets></workbook>");
    rels.push_str("</Relationships>");
    parts.push(("xl/workbook.xml".into(), wb.into_bytes()));
    parts.push(("xl/_rels/workbook.xml.rels".into(), rels.into_bytes()));
    if let Some(shared) = shared {
        let mut sst = format!("<?xml version=\"1.0\"?><sst {NS}>");
        for s in shared {
            sst.push_str(&format!("<si>{s}</si>"));
        }
        sst.push_str("</sst>");
        parts.push(("xl/sharedStrings.xml".into(), sst.into_bytes()));
    }
    let refs: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    zip_of(&refs)
}

#[test]
fn csv_header_samples_and_count() {
    let p = preview_csv(
        b"\xEF\xBB\xBFklant_id;naam;email\nC-1;Anna;a@x.nl\n\nC-2;Bram;b@x.nl\nC-3;Cas\n",
        true,
    )
    .unwrap();
    assert_eq!(p.format, TableFormat::Csv);
    assert_eq!(p.columns, ["klant_id", "naam", "email"]);
    assert_eq!(p.row_count, 3, "blank lines are not rows");
    assert_eq!(
        p.sample_rows[2],
        ["C-3", "Cas", ""],
        "short rows are padded to the header"
    );
}

#[test]
fn without_a_header_row_the_first_row_is_data_and_columns_are_numbered() {
    let p = preview_csv(b"C-1;Anna;a@x.nl\n\nC-2;Bram\nC-3;Cas;c@x.nl;VIP\n", false).unwrap();
    assert_eq!(
        p.columns,
        ["column_1", "column_2", "column_3", "column_4"],
        "N is the widest sampled row"
    );
    assert_eq!(p.row_count, 3, "row 1 is counted as data");
    assert_eq!(p.sample_rows[0], ["C-1", "Anna", "a@x.nl", ""]);
    assert_eq!(p.sample_rows[1], ["C-2", "Bram", "", ""]);
    assert_eq!(p.sample_rows[2], ["C-3", "Cas", "c@x.nl", "VIP"]);
    // the default reading of the same file loses that row to the header
    let h = preview_csv(b"C-1;Anna;a@x.nl\nC-2;Bram\n", true).unwrap();
    assert_eq!((h.columns[0].as_str(), h.row_count), ("C-1", 1));
    assert_eq!(
        preview_csv(b"\n\n", false).unwrap_err(),
        PreviewError::Empty
    );
}

#[test]
fn without_a_header_row_the_width_is_the_widest_row_beyond_the_sample() {
    let p = preview_csv(b"a,b\nc,d\ne,f\ng,h\ni,j\nk,l\nm,n,o\n", false).unwrap();
    assert_eq!(p.columns, ["column_1", "column_2", "column_3"]);
    assert_eq!((p.row_count, p.sample_rows.len()), (7, MAX_SAMPLE_ROWS));
    assert_eq!(
        p.sample_rows[0],
        ["a", "b", ""],
        "sampled rows padded to the full width"
    );
}

#[test]
fn without_a_header_row_every_row_is_counted_but_five_are_sampled() {
    let csv: String = (0..9).map(|i| format!("{i}\n")).collect();
    let p = preview_csv(csv.as_bytes(), false).unwrap();
    assert_eq!(p.columns, ["column_1"]);
    assert_eq!((p.row_count, p.sample_rows.len()), (9, MAX_SAMPLE_ROWS));
    assert_eq!(p.sample_rows[0], ["0"]);
}

#[test]
fn blank_and_duplicate_headers_get_stable_names() {
    let p = preview_csv(b"id,,id,name\n1,2,3,4\n", true).unwrap();
    assert_eq!(p.columns, ["id", "column_2", "id_2", "name"]);
}

#[test]
fn a_header_wider_than_the_column_cap_is_refused() {
    // an 8 KB line of `a,a,a…` (4,096 columns)
    let wide = vec!["a"; 4_096].join(",");
    assert_eq!(
        preview_csv(format!("{wide}\n1\n").as_bytes(), true).unwrap_err(),
        PreviewError::TooManyColumns
    );
    // a narrow header and one data record past the cap: refused on the record's field count
    let far = vec!["1"; MAX_COLUMNS + 1].join(",");
    assert_eq!(
        preview_csv(format!("h\n{far}\n").as_bytes(), true).unwrap_err(),
        PreviewError::TooManyColumns
    );
    let at_cap = vec!["a"; MAX_COLUMNS].join(",");
    assert_eq!(
        preview_csv(format!("{at_cap}\n").as_bytes(), true)
            .unwrap()
            .columns
            .len(),
        MAX_COLUMNS
    );
}

#[test]
fn preview_cells_are_cut_to_the_cell_cap() {
    let long = "é".repeat(MAX_CELL_BYTES); // 2 bytes each: twice the cap
    let p = preview_csv(format!("h\n{long}\n").as_bytes(), true).unwrap();
    let cell = &p.sample_rows[0][0];
    assert!(cell.len() <= MAX_CELL_BYTES, "{} bytes", cell.len());
    assert!(cell.len() >= MAX_CELL_BYTES - 4);
}

#[test]
fn at_most_five_samples_but_every_row_counted() {
    let mut csv = String::from("n\n");
    for i in 0..12 {
        csv.push_str(&format!("{i}\n"));
    }
    let p = preview_csv(csv.as_bytes(), true).unwrap();
    assert_eq!(p.sample_rows.len(), 5);
    assert_eq!(p.row_count, 12);
}

#[test]
fn tabs_quotes_and_non_utf8_bytes() {
    let p = preview_csv(b"a\tb\n\"x\ty\"\t\xff\n", true).unwrap();
    assert_eq!(p.columns, ["a", "b"]);
    assert_eq!(p.sample_rows[0], ["x\ty", "\u{fffd}"]);
}

#[test]
fn a_large_input_of_tiny_rows_is_counted_and_sampled_in_one_pass() {
    let mut csv = String::from("a,b\n");
    for _ in 0..200_000 {
        csv.push_str(",\n"); // blank rows
    }
    for i in 0..200_000 {
        csv.push_str(&format!("{i},x\n"));
    }
    let p = preview_csv(csv.as_bytes(), true).unwrap();
    assert_eq!(p.row_count, 200_000);
    assert_eq!(p.sample_rows.len(), 5);
    assert_eq!(p.sample_rows[0], ["0", "x"]);
}

#[test]
fn empty_input_is_an_error() {
    assert_eq!(preview_csv(b"\n\n", true).unwrap_err(), PreviewError::Empty);
}

#[test]
fn format_from_extension() {
    assert_eq!(TableFormat::from_file_name("a.CSV"), Some(TableFormat::Csv));
    assert_eq!(
        TableFormat::from_file_name("b.xlsx"),
        Some(TableFormat::Xlsx)
    );
    assert_eq!(TableFormat::from_file_name("c.xls"), None);
    assert_eq!(TableFormat::from_file_name("noext"), None);
}

#[test]
fn shared_and_inline_strings_numbers_and_booleans() {
    let data = concat!(
        r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c>"#,
        r#"<c r="C1" t="inlineStr"><is><t>active</t></is></c><c r="D1" t="s"><v>2</v></c></row>"#,
        r#"<row r="2"><c r="A2" t="s"><v>3</v></c><c r="B2"><v>42.5</v></c>"#,
        r#"<c r="C2" t="b"><v>1</v></c><c r="D2" t="inlineStr"><is><r><t>B &amp; </t></r><r><t xml:space="preserve">co&#233;</t></r></is></c></row>"#,
        r#"<row r="3"><c r="A3" t="str"><f>A2&amp;"x"</f><v>C-1x</v></c><c r="B3"><v>7</v></c>"#,
        r#"<c r="C3" t="b"><v>0</v></c><c r="D3" t="e"><v>#N/A</v></c></row>"#,
    );
    let shared: &[&str] = &[
        "<t>klant_id</t>",
        "<r><t>na</t></r><r><t>am</t></r><rPh><t>ignored</t></rPh>",
        "<t>notes</t>",
        "<t>C-1</t>",
    ];
    let p = preview_xlsx(&workbook(&[("Customers", data)], Some(shared)), None, true).unwrap();
    assert_eq!(p.format, TableFormat::Xlsx);
    assert_eq!(p.columns, ["klant_id", "naam", "active", "notes"]);
    assert_eq!(p.row_count, 2);
    assert_eq!(p.sample_rows[0], ["C-1", "42.5", "TRUE", "B & coé"]);
    assert_eq!(p.sample_rows[1], ["C-1x", "7", "FALSE", "#N/A"]);
}

#[test]
fn sparse_cells_land_in_their_lettered_column() {
    let mut header = String::from(r#"<row r="1">"#);
    for i in 0..28 {
        // columns A..AB, the last two without an r attribute (they follow the previous cell)
        let r = if i < 26 {
            format!(r#" r="{}1""#, (b'A' + i as u8) as char)
        } else {
            String::new()
        };
        header.push_str(&format!(r#"<c{r} t="inlineStr"><is><t>h{i}</t></is></c>"#));
    }
    header.push_str("</row>");
    let data = format!(
        r#"{header}<row r="2"/><row r="4"><c r="B4"><v>2</v></c><c r="D4"/><c r="AA4"><v>27</v></c></row>"#
    );
    let p = preview_xlsx(&workbook(&[("S", &data)], None), None, true).unwrap();
    assert_eq!(p.columns.len(), 28);
    assert_eq!(p.columns[26], "h26");
    assert_eq!(p.row_count, 1, "the empty row 2 is not a row");
    let row = &p.sample_rows[0];
    assert_eq!(row.len(), 28, "padded to the header");
    assert_eq!((row[0].as_str(), row[1].as_str()), ("", "2"));
    assert_eq!(row[3], "", "an empty cell element stays empty");
    assert_eq!(row[26], "27", "AA is the 27th column");
}

#[test]
fn a_named_sheet_is_chosen_and_the_first_is_the_default() {
    let one = r#"<row><c t="inlineStr"><is><t>first</t></is></c></row>"#;
    let two = concat!(
        r#"<row><c t="inlineStr"><is><t>second</t></is></c></row>"#,
        r#"<row><c><v>1</v></c></row><row><c><v>2</v></c></row>"#,
    );
    let wb = workbook(&[("Intro", one), ("Klanten 2026", two)], None);
    assert_eq!(preview_xlsx(&wb, None, true).unwrap().columns, ["first"]);
    assert_eq!(
        preview_xlsx(&wb, Some("  "), true).unwrap().columns,
        ["first"]
    );
    let p = preview_xlsx(&wb, Some("Klanten 2026"), true).unwrap();
    assert_eq!(
        (p.columns.clone(), p.row_count),
        (vec!["second".to_string()], 2)
    );
    // agent's addition: every sheet in workbook order, and the one read (UI `ColumnPreview.sheets` / `.sheet`)
    assert_eq!(p.sheets, ["Intro", "Klanten 2026"]);
    assert_eq!(p.sheet.as_deref(), Some("Klanten 2026"));
    let first = preview_xlsx(&wb, None, true).unwrap();
    assert_eq!(first.sheet.as_deref(), Some("Intro"));
    // a CSV has no sheets
    let csv = preview_csv(b"a\n1\n", true).unwrap();
    assert!(csv.sheets.is_empty() && csv.sheet.is_none());
    assert_eq!(
        preview_xlsx(&wb, Some("klanten 2026"), true)
            .unwrap()
            .columns,
        ["second"]
    );
}

#[test]
fn an_xlsx_without_a_header_row_keeps_its_first_row_as_data() {
    let data = concat!(
        r#"<row><c t="inlineStr"><is><t>10482</t></is></c><c t="inlineStr"><is><t>Anna</t></is></c></row>"#,
        r#"<row><c><v>10483</v></c></row>"#,
    );
    let wb = workbook(&[("Klanten", data)], None);
    let p = preview_xlsx(&wb, None, false).unwrap();
    assert_eq!(p.columns, ["column_1", "column_2"]);
    assert_eq!(p.row_count, 2);
    assert_eq!(p.sample_rows, [["10482", "Anna"], ["10483", ""]]);
}

#[test]
fn a_missing_sheet_is_named_in_the_error() {
    let wb = workbook(&[("Intro", "<row><c><v>1</v></c></row>")], None);
    let e = preview_xlsx(&wb, Some("Orders"), true).unwrap_err();
    assert_eq!(e, PreviewError::SheetNotFound("Orders".into()));
    assert_eq!(e.to_string(), "the workbook has no sheet named \"Orders\"");
}

#[test]
fn samples_are_capped_and_every_row_counted() {
    let mut data = String::from(r#"<row><c t="inlineStr"><is><t>n</t></is></c></row>"#);
    for i in 0..12 {
        data.push_str(&format!("<row><c><v>{i}</v></c></row>"));
    }
    let p = preview_xlsx(&workbook(&[("S", &data)], None), None, true).unwrap();
    assert_eq!((p.sample_rows.len(), p.row_count), (MAX_SAMPLE_ROWS, 12));
}

#[test]
fn a_shared_string_is_cut_per_cell_not_copied_whole() {
    let big = "x".repeat(64 * 1024);
    let shared = [format!("<t>{big}</t>")];
    let shared: Vec<&str> = shared.iter().map(String::as_str).collect();
    let row: String = (0..200).map(|_| r#"<c t="s"><v>0</v></c>"#).collect();
    let data = format!("<row>{row}</row><row>{row}</row>");
    let p = preview_xlsx(&workbook(&[("S", &data)], Some(&shared)), None, true).unwrap();
    assert_eq!(p.columns.len(), 200);
    assert!(
        p.columns.iter().all(|c| c.len() <= MAX_CELL_BYTES + 8),
        "header cut"
    );
    assert!(p.sample_rows[0].iter().all(|c| c.len() == MAX_CELL_BYTES));
}

#[test]
fn an_oversized_shared_strings_part_is_refused() {
    let big = "x".repeat(MAX_SHARED_STRINGS_BYTES + 1);
    let shared = [format!("<t>{big}</t>")];
    let shared: Vec<&str> = shared.iter().map(String::as_str).collect();
    let wb = workbook(
        &[("S", r#"<row><c t="s"><v>0</v></c></row>"#)],
        Some(&shared),
    );
    assert_eq!(
        preview_xlsx(&wb, None, true).unwrap_err(),
        PreviewError::TooLarge("its shared strings exceed 16 MiB")
    );
}

#[test]
fn too_many_shared_string_entries_are_refused() {
    let sst = format!(
        "<?xml version=\"1.0\"?><sst {NS}>{}</sst>",
        "<si/>".repeat(MAX_SHARED_STRINGS + 1)
    );
    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    let wb = workbook(&[("S", r#"<row><c><v>1</v></c></row>"#)], None);
    let mut zip = zip::ZipArchive::new(Cursor::new(wb.as_slice())).unwrap();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let mut b = Vec::new();
        f.read_to_end(&mut b).unwrap();
        parts.push((f.name().unwrap().to_string(), b));
    }
    parts.push(("xl/sharedStrings.xml".into(), sst.into_bytes()));
    let refs: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    assert_eq!(
        preview_xlsx(&zip_of(&refs), None, true).unwrap_err(),
        PreviewError::TooLarge("it holds more than 1,000,000 shared strings")
    );
}

#[test]
fn a_cell_beyond_the_column_cap_is_refused_without_widening_the_row() {
    // ALL is column 1,000 (index 999): the last allowed; ALM (index 1,000) is past the cap; XFD is Excel's last.
    let ok = r#"<row><c r="A1"><v>1</v></c><c r="ALL1"><v>2</v></c></row>"#;
    let p = preview_xlsx(&workbook(&[("S", ok)], None), None, true).unwrap();
    assert_eq!(p.columns.len(), MAX_COLUMNS);
    // a narrow header and the far cell in a DATA row: only the sheet reader can refuse it
    for far in ["ALM", "XFD"] {
        let data = format!(
            r#"<row><c r="A1"><v>h</v></c></row><row><c r="A2"><v>1</v></c><c r="{far}2"><v>2</v></c></row>"#
        );
        assert_eq!(
            preview_xlsx(&workbook(&[("S", &data)], None), None, true).unwrap_err(),
            PreviewError::TooManyColumns,
            "{far}"
        );
    }
}

#[test]
fn a_zip_bomb_hits_the_expansion_cap() {
    // ~51 MiB of whitespace inside the sheet deflates to a few dozen KiB.
    let pad = " ".repeat(51 * 1024 * 1024);
    let data = format!(r#"<row><c><v>1</v></c></row>{pad}"#);
    let wb = workbook(&[("S", &data)], None);
    assert!(wb.len() < 1024 * 1024, "compressed to {} bytes", wb.len());
    assert_eq!(
        preview_xlsx(&wb, None, true).unwrap_err(),
        PreviewError::TooLarge("it expands beyond 50 MiB")
    );
}

#[test]
fn too_many_parts_are_refused_before_reading() {
    let names: Vec<String> = (0..=MAX_XLSX_ENTRIES).map(|i| format!("p{i}")).collect();
    let parts: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
    assert_eq!(
        preview_xlsx(&zip_of(&parts), None, true).unwrap_err(),
        PreviewError::TooLarge("it holds more than 10,000 parts")
    );
}

#[test]
fn non_zip_and_incomplete_workbooks_are_unreadable() {
    assert_eq!(preview_xlsx(b"a,b\n1,2\n", None, true).unwrap_err(), XLSX);
    assert_eq!(preview_xlsx(b"PK\x03\x04", None, true).unwrap_err(), XLSX);
    let no_workbook = zip_of(&[("xl/worksheets/sheet1.xml", b"<worksheet/>")]);
    assert_eq!(preview_xlsx(&no_workbook, None, true).unwrap_err(), XLSX);
    let header_only_blank = workbook(&[("S", "<row/>")], None);
    assert_eq!(
        preview_xlsx(&header_only_blank, None, true).unwrap_err(),
        PreviewError::Empty
    );
    let bad_shared_index = workbook(&[("S", r#"<row><c t="s"><v>9</v></c></row>"#)], Some(&[]));
    assert_eq!(
        preview_xlsx(&bad_shared_index, None, true).unwrap_err(),
        XLSX
    );
}
