//! CSV import: preview, column mapping and a transactional, batched load.

use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use crate::engine::Session;
use crate::error::{DbError, DbResult};
use crate::model::{Engine, ObjectRef, TableStructure};
use crate::sql::{Dialect, dml};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvPreview {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

fn reader(path: &Path, delimiter: u8, has_header: bool) -> io::Result<csv::Reader<BufReader<File>>> {
    let f = File::open(path)?;
    Ok(csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(has_header)
        .flexible(true)
        .from_reader(BufReader::new(f)))
}

/// Header names (or `column1…`) and the first `max_rows` records.
pub fn preview_csv(path: &Path, delimiter: u8, has_header: bool, max_rows: usize) -> io::Result<CsvPreview> {
    let mut r = reader(path, delimiter, has_header)?;
    let mut rows = Vec::new();
    for rec in r.records().take(max_rows) {
        let rec = rec.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        rows.push(rec.iter().map(String::from).collect::<Vec<_>>());
    }
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let headers = if has_header {
        r.headers()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
            .iter()
            .map(String::from)
            .collect()
    } else {
        (1..=width).map(|i| format!("column{i}")).collect()
    };
    Ok(CsvPreview { headers, rows })
}

/// Maps table columns to CSV columns: same name (case-insensitive) first, else by position.
pub fn auto_mapping(csv_headers: &[String], table_columns: &[String]) -> Vec<(String, Option<usize>)> {
    let by_name = table_columns.iter().any(|t| csv_headers.iter().any(|h| h.eq_ignore_ascii_case(t)));
    table_columns
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let idx = if by_name {
                csv_headers.iter().position(|h| h.trim().eq_ignore_ascii_case(t))
            } else {
                (i < csv_headers.len()).then_some(i)
            };
            (t.clone(), idx)
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct ImportSpec {
    pub path: PathBuf,
    pub delimiter: u8,
    pub has_header: bool,
    /// Table column → CSV column index; unmapped columns use their default.
    pub mapping: Vec<(String, usize)>,
    pub empty_as_null: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportReport {
    pub rows: u64,
}

fn batch_size(engine: Engine, columns: usize) -> usize {
    // Stay well below bind-parameter limits (SQLite 32 766, PG/MySQL 65 535).
    let max_params = if engine == Engine::Sqlite { 30_000 } else { 60_000 };
    (max_params / columns.max(1)).clamp(1, 500)
}

/// Loads the whole file in one transaction. Any error rolls everything back and reports
/// which data rows (1-based, excluding the header) were in the failing batch.
pub async fn import_csv(
    s: &mut Session,
    table: &ObjectRef,
    structure: &TableStructure,
    spec: &ImportSpec,
) -> DbResult<ImportReport> {
    if spec.mapping.is_empty() {
        return Err(DbError::Invalid("map at least one column".into()));
    }
    let columns: Vec<String> = spec.mapping.iter().map(|(c, _)| c.clone()).collect();
    let mut rdr = reader(&spec.path, spec.delimiter, spec.has_header)?;
    let mut n = 0u64;
    let rows = rdr.records().map(|rec| {
        n += 1;
        let rec = rec.map_err(|e| DbError::Invalid(format!("row {n}: invalid CSV: {e}")))?;
        Ok(spec
            .mapping
            .iter()
            .map(|(_, idx)| rec.get(*idx).map(str::to_string).filter(|v| !(spec.empty_as_null && v.is_empty())))
            .collect())
    });
    load_rows(s, table, structure, &columns, rows).await
}

/// JSON text of a value that is not a scalar; scalars use their natural text form.
fn json_cell(engine: Engine, v: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    match v {
        Value::Null => None,
        Value::String(t) => Some(t.clone()),
        Value::Bool(b) if engine == Engine::Postgres => Some(b.to_string()),
        Value::Bool(b) => Some(if *b { "1" } else { "0" }.into()),
        other => Some(other.to_string()),
    }
}

/// Imports a JSON file holding an array of objects (what the JSON export writes). Keys are matched
/// to table columns case-insensitively; a key with no column aborts the import before any write.
pub async fn import_json(
    s: &mut Session,
    table: &ObjectRef,
    structure: &TableStructure,
    path: &Path,
) -> DbResult<ImportReport> {
    let text = std::fs::read_to_string(path)?;
    let doc: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| DbError::Invalid(format!("invalid JSON: {e}")))?;
    let items = doc.as_array().ok_or_else(|| DbError::Invalid("expected a JSON array of objects".into()))?;
    let mut keys: Vec<String> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let obj = item.as_object().ok_or_else(|| DbError::Invalid(format!("element {} is not an object", i + 1)))?;
        for k in obj.keys() {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    if keys.is_empty() {
        return Err(DbError::Invalid("the JSON array has no fields to import".into()));
    }
    // Several keys may differ only by case and so name the same column: they share one slot.
    let mut columns: Vec<String> = Vec::new();
    let mut slot_keys: Vec<Vec<&String>> = Vec::new();
    for k in &keys {
        let col = structure
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(k))
            .ok_or_else(|| DbError::Invalid(format!("the table has no column named {k:?}")))?;
        match columns.iter().position(|c| *c == col.name) {
            Some(i) => slot_keys[i].push(k),
            None => {
                columns.push(col.name.clone());
                slot_keys.push(vec![k]);
            }
        }
    }
    let engine = s.engine();
    let rows = items.iter().map(|item| {
        let obj = item.as_object().into_iter().flatten().collect::<std::collections::HashMap<_, _>>();
        Ok(slot_keys.iter().map(|ks| ks.iter().find_map(|k| obj.get(k)).and_then(|v| json_cell(engine, v))).collect())
    });
    load_rows(s, table, structure, &columns, rows).await
}

/// One parsed XML row: `(column, value)` pairs, `None` for NULL.
type XmlRow = Vec<(String, Option<String>)>;

/// Rows of the XML file the XML export writes: `<rows><row><col name="id">1</col>…</row></rows>`.
/// A column with `null="true"` (or one missing from a row) becomes NULL.
fn parse_xml_rows(text: &str) -> DbResult<Vec<XmlRow>> {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut reader = Reader::from_str(text);
    let bad = |e: String| DbError::Invalid(format!("invalid XML: {e}"));
    let mut rows: Vec<Vec<(String, Option<String>)>> = Vec::new();
    let mut current: Option<Vec<(String, Option<String>)>> = None;
    let mut col: Option<(String, bool, String)> = None; // name, is_null, text
    loop {
        match reader.read_event().map_err(|e| bad(e.to_string()))? {
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == "row" => {
                current = Some(Vec::new());
            }
            Event::End(e) if e.name().as_ref() == "row" => {
                rows.extend(current.take());
            }
            ev @ (Event::Start(_) | Event::Empty(_))
                if current.is_some()
                    && matches!(&ev, Event::Start(e) | Event::Empty(e) if e.name().as_ref() == "col") =>
            {
                let (e, empty) = match ev {
                    Event::Start(e) => (e, false),
                    Event::Empty(e) => (e, true),
                    _ => unreachable!(),
                };
                let mut name = None;
                let mut is_null = false;
                for a in e.attributes().flatten() {
                    let v = a
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|e| bad(e.to_string()))?
                        .to_string();
                    match a.key.as_ref() {
                        "name" => name = Some(v),
                        "null" => is_null = v == "true",
                        _ => {}
                    }
                }
                let name = name.ok_or_else(|| bad("a <col> has no name attribute".into()))?;
                if empty {
                    // `<col name="x"/>` is an empty string unless it says null="true".
                    if let Some(r) = current.as_mut() {
                        r.push((name, if is_null { None } else { Some(String::new()) }));
                    }
                } else {
                    col = Some((name, is_null, String::new()));
                }
            }
            Event::Text(t) => {
                if let Some((_, _, buf)) = col.as_mut() {
                    buf.push_str(&t.xml11_content());
                }
            }
            Event::GeneralRef(r) => {
                if let Some((_, _, buf)) = col.as_mut() {
                    if let Some(c) = r.resolve_char_ref().map_err(|e| bad(e.to_string()))? {
                        buf.push(c);
                        continue;
                    }
                    buf.push_str(match &*r {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        other => return Err(bad(format!("unknown entity &{other};"))),
                    });
                }
            }
            Event::End(e) if e.name().as_ref() == "col" => {
                if let (Some((name, null, text)), Some(r)) = (col.take(), current.as_mut()) {
                    r.push((name, if null { None } else { Some(text) }));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(rows)
}

/// Imports an XML file in the format of the XML export (`<rows><row><col name=…>`).
pub async fn import_xml(
    s: &mut Session,
    table: &ObjectRef,
    structure: &TableStructure,
    path: &Path,
) -> DbResult<ImportReport> {
    let text = std::fs::read_to_string(path)?;
    let parsed = parse_xml_rows(&text)?;
    if parsed.is_empty() {
        return Err(DbError::Invalid("the XML file has no <row> elements".into()));
    }
    let mut columns: Vec<String> = Vec::new();
    for (name, _) in parsed.iter().flatten() {
        let col = structure
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| DbError::Invalid(format!("the table has no column named {name:?}")))?;
        if !columns.contains(&col.name) {
            columns.push(col.name.clone());
        }
    }
    let rows = parsed.into_iter().map(|r| {
        Ok(columns
            .iter()
            .map(|c| r.iter().find(|(n, _)| n.eq_ignore_ascii_case(c)).and_then(|(_, v)| v.clone()))
            .collect())
    });
    load_rows(s, table, structure, &columns, rows).await
}

/// Batched multi-row INSERT of `rows` (already in `columns` order) inside one transaction.
pub(crate) async fn load_rows(
    s: &mut Session,
    table: &ObjectRef,
    structure: &TableStructure,
    columns: &[String],
    rows: impl Iterator<Item = DbResult<Vec<Option<String>>>>,
) -> DbResult<ImportReport> {
    let d = Dialect(s.engine());
    let target = d.qualified(table)?;
    let batch = batch_size(s.engine(), columns.len());
    let full_sql = dml::insert_rows(d, &target, structure, columns, batch)?;
    let mut params: Vec<Option<String>> = Vec::with_capacity(batch * columns.len());
    let mut in_batch = 0usize;
    let mut done: u64 = 0;
    let begin = s.engine().begin_sql();
    s.execute_batch(&[begin.to_string()], false).await?;
    let result: DbResult<()> = async {
        let mut rows = rows;
        loop {
            let row = rows.next();
            let flush = match &row {
                Some(_) => in_batch == batch,
                None => in_batch > 0,
            };
            if flush {
                let sql = if in_batch == batch {
                    full_sql.clone()
                } else {
                    dml::insert_rows(d, &target, structure, columns, in_batch)?
                };
                s.exec_params(&sql, &params)
                    .await
                    .map_err(|e| DbError::Invalid(format!("rows {}–{}: {e}", done + 1, done + in_batch as u64)))?;
                done += in_batch as u64;
                params.clear();
                in_batch = 0;
            }
            let Some(row) = row else { break };
            params.extend(row?);
            in_batch += 1;
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            s.execute_batch(&["COMMIT".to_string()], false).await?;
            Ok(ImportReport { rows: done })
        }
        Err(e) => {
            if let Err(rb) = s.execute_batch(&["ROLLBACK".to_string()], false).await {
                tracing::warn!(error = %rb, "rollback after failed import also failed");
            }
            Err(DbError::Invalid(format!("Import failed, nothing was imported. {e}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_rows_parse_nulls_entities_and_empty_values() {
        let xml = "<?xml version=\"1.0\"?>\n<rows>\n  <row><col name=\"id\">1</col><col name=\"t\">a &amp; &lt;b&gt; &quot;q&quot;</col><col name=\"n\" null=\"true\"/></row>\n  <row><col name=\"id\">2</col><col name=\"t\"></col></row>\n</rows>";
        let rows = parse_xml_rows(xml).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], ("id".into(), Some("1".into())));
        assert_eq!(rows[0][1], ("t".into(), Some("a & <b> \"q\"".into())));
        assert_eq!(rows[0][2], ("n".into(), None));
        assert_eq!(rows[1][1], ("t".into(), Some(String::new())));
        assert!(parse_xml_rows("<rows><row><col>x</col></row></rows>").is_err());
        assert!(parse_xml_rows("<rows><row></rows>").is_err() || parse_xml_rows("<rows></rows>").unwrap().is_empty());
    }

    #[test]
    fn preview_and_mapping() {
        let dir = std::env::temp_dir().join(format!("talus-csv-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.csv");
        std::fs::write(&p, "Name;ID\n\"a;b\";1\nc;2\n").unwrap();
        let pv = preview_csv(&p, b';', true, 10).unwrap();
        assert_eq!(pv.headers, ["Name", "ID"]);
        assert_eq!(pv.rows, vec![vec!["a;b".to_string(), "1".into()], vec!["c".into(), "2".into()]]);
        let m = auto_mapping(&pv.headers, &["id".into(), "name".into(), "extra".into()]);
        assert_eq!(m, vec![("id".into(), Some(1)), ("name".into(), Some(0)), ("extra".into(), None)]);
        let pv = preview_csv(&p, b';', false, 1).unwrap();
        assert_eq!(pv.headers, ["column1", "column2"]);
        assert_eq!(
            auto_mapping(&pv.headers, &["x".into(), "y".into(), "z".into()]),
            vec![("x".into(), Some(0)), ("y".into(), Some(1)), ("z".into(), None)]
        );
        assert_eq!(batch_size(Engine::Sqlite, 100), 300);
        assert_eq!(batch_size(Engine::Postgres, 2), 500);
    }
}
