//! Streaming export writers. Rows are written as they arrive, so exports never
//! need the whole result in memory.

use std::io::{self, Write};
use std::path::Path;

use crate::engine::Session;
use crate::error::{DbError, DbResult};
use crate::model::{ObjectRef, PageQuery, SortDir};
use crate::sql::dml;

use crate::model::{Cell, ColumnMeta};
use crate::sql::Dialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Tsv,
    Json,
    SqlInsert,
    Html,
    Xml,
    /// Written by `export_table` only (a workbook is not a byte stream).
    Xlsx,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 7] = [
        ExportFormat::Csv,
        ExportFormat::Tsv,
        ExportFormat::Json,
        ExportFormat::SqlInsert,
        ExportFormat::Html,
        ExportFormat::Xml,
        ExportFormat::Xlsx,
    ];
    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Tsv => "TSV",
            ExportFormat::Json => "JSON",
            ExportFormat::SqlInsert => "SQL INSERT",
            ExportFormat::Html => "HTML",
            ExportFormat::Xml => "XML",
            ExportFormat::Xlsx => "Excel (.xlsx)",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Tsv => "tsv",
            ExportFormat::Json => "json",
            ExportFormat::SqlInsert => "sql",
            ExportFormat::Html => "html",
            ExportFormat::Xml => "xml",
            ExportFormat::Xlsx => "xlsx",
        }
    }
}

/// Escapes text for XML/HTML content and attributes; characters XML 1.0 forbids become U+FFFD.
pub(crate) fn escape_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

enum Sink<W: Write> {
    Html(W),
    Xml(W),
    Delimited(Box<csv::Writer<W>>),
    Json(W),
    Sql { out: W, dialect: Dialect, prefix: String },
}

pub struct ExportWriter<W: Write> {
    sink: Sink<W>,
    columns: Vec<String>,
    rows: u64,
}

fn csv_err(e: csv::Error) -> io::Error {
    io::Error::other(e.to_string())
}

impl<W: Write> ExportWriter<W> {
    /// `sql_target` (already quoted table name) and `dialect` are used only by `SqlInsert`.
    pub fn new(
        format: ExportFormat,
        mut out: W,
        columns: &[ColumnMeta],
        sql_target: &str,
        dialect: Option<Dialect>,
    ) -> io::Result<Self> {
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let sink = match format {
            ExportFormat::Csv | ExportFormat::Tsv => {
                let delimiter = if format == ExportFormat::Csv { b',' } else { b'\t' };
                let mut w = csv::WriterBuilder::new().delimiter(delimiter).from_writer(out);
                w.write_record(&names).map_err(csv_err)?;
                Sink::Delimited(Box::new(w))
            }
            ExportFormat::Json => {
                out.write_all(b"[")?;
                Sink::Json(out)
            }
            ExportFormat::Html => {
                write!(
                    out,
                    "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>{}</title>\n<style>body{{font:13px -apple-system,sans-serif}}table{{border-collapse:collapse}}th,td{{border:1px solid #ccc;padding:3px 8px;text-align:left}}th{{background:#f3f3f3}}.null{{color:#999}}</style></head>\n<body><table>\n<thead><tr>",
                    escape_markup(sql_target.trim_matches(['"', '`']))
                )?;
                for n in &names {
                    write!(out, "<th>{}</th>", escape_markup(n))?;
                }
                out.write_all(b"</tr></thead>\n<tbody>\n")?;
                Sink::Html(out)
            }
            ExportFormat::Xlsx => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "xlsx cannot be streamed; use export_table"));
            }
            ExportFormat::Xml => {
                out.write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rows>\n")?;
                Sink::Xml(out)
            }
            ExportFormat::SqlInsert => {
                let dialect =
                    dialect.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "SQL export needs a dialect"))?;
                let cols = names
                    .iter()
                    .map(|c| dialect.quote_ident(c))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| io::Error::other(e.to_string()))?;
                let prefix = format!("INSERT INTO {sql_target} ({}) VALUES (", cols.join(", "));
                Sink::Sql { out, dialect, prefix }
            }
        };
        Ok(Self { sink, columns: names, rows: 0 })
    }

    pub fn write_row(&mut self, row: &[Cell]) -> io::Result<()> {
        match &mut self.sink {
            Sink::Delimited(w) => w.write_record(row.iter().map(|c| c.as_deref().unwrap_or(""))).map_err(csv_err)?,
            Sink::Json(out) => {
                let obj: serde_json::Map<String, serde_json::Value> = self
                    .columns
                    .iter()
                    .zip(row)
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v.as_ref().map_or(serde_json::Value::Null, |s| serde_json::Value::String(s.clone())),
                        )
                    })
                    .collect();
                out.write_all(if self.rows > 0 { b",\n  " } else { b"\n  " })?;
                serde_json::to_writer(&mut *out, &obj).map_err(io::Error::other)?;
            }
            Sink::Html(out) => {
                out.write_all(b"<tr>")?;
                for v in row {
                    match v {
                        Some(t) => write!(out, "<td>{}</td>", escape_markup(t))?,
                        None => out.write_all(b"<td class=\"null\">NULL</td>")?,
                    }
                }
                out.write_all(b"</tr>\n")?;
            }
            Sink::Xml(out) => {
                out.write_all(b"  <row>")?;
                for (name, v) in self.columns.iter().zip(row) {
                    let n = escape_markup(name);
                    match v {
                        Some(t) => write!(out, "<col name=\"{n}\">{}</col>", escape_markup(t))?,
                        None => write!(out, "<col name=\"{n}\" null=\"true\"/>")?,
                    }
                }
                out.write_all(b"</row>\n")?;
            }
            Sink::Sql { out, dialect, prefix } => {
                let vals: Vec<String> = row
                    .iter()
                    .map(|v| v.as_deref().map_or_else(|| "NULL".to_string(), |s| dialect.string_literal(s)))
                    .collect();
                writeln!(out, "{prefix}{});", vals.join(", "))?;
            }
        }
        self.rows += 1;
        Ok(())
    }

    /// Flushes and returns the number of rows written.
    pub fn finish(self) -> io::Result<u64> {
        match self.sink {
            Sink::Delimited(w) => w.into_inner().map_err(|e| io::Error::other(e.to_string()))?.flush()?,
            Sink::Json(mut out) => {
                out.write_all(if self.rows > 0 { b"\n]\n" } else { b"]\n" })?;
                out.flush()?;
            }
            Sink::Html(mut out) => {
                out.write_all(b"</tbody>\n</table></body></html>\n")?;
                out.flush()?;
            }
            Sink::Xml(mut out) => {
                out.write_all(b"</rows>\n")?;
                out.flush()?;
            }
            Sink::Sql { mut out, .. } => out.flush()?,
        }
        Ok(self.rows)
    }
}

const EXPORT_CHUNK: u64 = 5000;

/// Rows of a whole table in key order (or all columns), delivered chunk by chunk.
enum Event<'a> {
    Columns(&'a [ColumnMeta]),
    Row(&'a [Cell]),
}

async fn stream_rows(s: &mut Session, obj: &ObjectRef, mut on: impl FnMut(Event<'_>) -> DbResult<()>) -> DbResult<()> {
    let st = s.table_structure(obj).await?;
    // Deterministic order makes chunked paging complete: key columns, else every column.
    let sort: Vec<(String, SortDir)> = if dml::key_columns(s.engine(), &st).is_some() {
        Vec::new()
    } else {
        st.columns.iter().map(|c| (c.name.clone(), SortDir::Asc)).collect()
    };
    let mut q = PageQuery { table: obj.clone(), filters: Vec::new(), sort, offset: 0, limit: EXPORT_CHUNK };
    let mut keep: Option<Vec<usize>> = None;
    loop {
        let page = s.fetch_page(&q, &st).await?;
        let keep = keep.get_or_insert_with(|| {
            page.columns.iter().enumerate().filter(|(_, c)| c.name != dml::ROWID_ALIAS).map(|(i, _)| i).collect()
        });
        if q.offset == 0 {
            let cols: Vec<ColumnMeta> = keep.iter().map(|&i| page.columns[i].clone()).collect();
            on(Event::Columns(&cols))?;
        }
        for row in &page.rows {
            let projected: Vec<Cell> = keep.iter().map(|&i| row[i].clone()).collect();
            on(Event::Row(&projected))?;
        }
        if (page.rows.len() as u64) < EXPORT_CHUNK {
            return Ok(());
        }
        q.offset += EXPORT_CHUNK;
    }
}

/// Streams a whole table/view to `path` in chunks, so the table never has to fit in memory.
/// Returns the number of rows written.
pub async fn export_table(s: &mut Session, obj: &ObjectRef, format: ExportFormat, path: &Path) -> DbResult<u64> {
    if format == ExportFormat::Xlsx {
        return export_xlsx(s, obj, path).await;
    }
    let target = Dialect(s.engine()).qualified(obj)?;
    let file = std::fs::File::create(path)?;
    export_table_to(s, obj, format, std::io::BufWriter::new(file), &target).await
}

/// Like `export_table` but writes into any sink; `sql_target` is the (quoted) table name used
/// by `SqlInsert` statements.
pub async fn export_table_to<W: Write>(
    s: &mut Session,
    obj: &ObjectRef,
    format: ExportFormat,
    out: W,
    sql_target: &str,
) -> DbResult<u64> {
    let dialect = Some(Dialect(s.engine()));
    let mut out = Some(out);
    let mut writer: Option<ExportWriter<W>> = None;
    stream_rows(s, obj, |ev| {
        match ev {
            Event::Columns(cols) => {
                let sink = out.take().ok_or_else(|| DbError::Io("export sink missing".into()))?;
                writer = Some(ExportWriter::new(format, sink, cols, sql_target, dialect)?);
            }
            Event::Row(row) => {
                if let Some(w) = writer.as_mut() {
                    w.write_row(row)?;
                }
            }
        }
        Ok(())
    })
    .await?;
    Ok(match writer {
        Some(w) => w.finish()?,
        None => 0,
    })
}

/// Excel's hard row limit (one header row used).
const XLSX_MAX_DATA_ROWS: u64 = 1_048_575;

fn sheet_name(name: &str) -> String {
    let n: String = name.chars().filter(|c| !"[]:*?/\\".contains(*c)).take(31).collect();
    if n.trim().is_empty() { "Sheet1".into() } else { n }
}

async fn export_xlsx(s: &mut Session, obj: &ObjectRef, path: &Path) -> DbResult<u64> {
    use rust_xlsxwriter::{Format, Workbook};
    let xl = |e: rust_xlsxwriter::XlsxError| DbError::Io(format!("xlsx: {e}"));
    let mut book = Workbook::new();
    let sheet = book.add_worksheet_with_constant_memory();
    sheet.set_name(sheet_name(&obj.name)).map_err(xl)?;
    let bold = Format::new().set_bold();
    let mut rows: u64 = 0;
    stream_rows(s, obj, |ev| {
        match ev {
            Event::Columns(cols) => {
                for (i, c) in cols.iter().enumerate() {
                    sheet.write_string_with_format(0, i as u16, &c.name, &bold).map_err(xl)?;
                }
            }
            Event::Row(row) => {
                if rows >= XLSX_MAX_DATA_ROWS {
                    return Err(DbError::Invalid(format!(
                        "Excel holds at most {XLSX_MAX_DATA_ROWS} data rows per sheet"
                    )));
                }
                rows += 1;
                for (i, v) in row.iter().enumerate() {
                    let Some(text) = v else { continue };
                    // Numbers only when they survive the round trip (keeps zip codes, big ids, 1.50).
                    match text.parse::<f64>().ok().filter(|f| f.is_finite() && f.to_string() == *text) {
                        Some(f) => sheet.write_number(rows as u32, i as u16, f),
                        None => sheet.write_string(rows as u32, i as u16, text),
                    }
                    .map_err(xl)?;
                }
            }
        }
        Ok(())
    })
    .await?;
    book.save(path).map_err(xl)?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Engine;

    fn cols() -> Vec<ColumnMeta> {
        ["id", "name"]
            .iter()
            .map(|n| ColumnMeta { name: n.to_string(), type_name: String::new(), binary: false })
            .collect()
    }

    fn run(format: ExportFormat) -> String {
        let mut buf = Vec::new();
        let mut w = ExportWriter::new(format, &mut buf, &cols(), "\"t\"", Some(Dialect(Engine::Postgres))).unwrap();
        w.write_row(&[Some("1".into()), Some("a,\"b\"".into())]).unwrap();
        w.write_row(&[Some("2".into()), None]).unwrap();
        assert_eq!(w.finish().unwrap(), 2);
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn csv_quotes_and_nulls() {
        assert_eq!(run(ExportFormat::Csv), "id,name\n1,\"a,\"\"b\"\"\"\n2,\n");
    }

    #[test]
    fn json_is_valid_with_nulls() {
        let v: serde_json::Value = serde_json::from_str(&run(ExportFormat::Json)).unwrap();
        assert_eq!(v[0]["name"], "a,\"b\"");
        assert!(v[1]["name"].is_null());
    }

    #[test]
    fn sql_insert_escapes() {
        let s = run(ExportFormat::SqlInsert);
        assert_eq!(s.lines().next().unwrap(), "INSERT INTO \"t\" (\"id\", \"name\") VALUES ('1', 'a,\"b\"');");
        assert!(s.contains("VALUES ('2', NULL);"));
    }

    #[test]
    fn html_and_xml_escape_markup_and_mark_nulls() {
        let mut buf = Vec::new();
        let mut w = ExportWriter::new(ExportFormat::Html, &mut buf, &cols(), "\"t\"", None).unwrap();
        w.write_row(&[Some("1".into()), Some("<b>&\"x\"</b>".into())]).unwrap();
        w.write_row(&[Some("2".into()), None]).unwrap();
        w.finish().unwrap();
        let h = String::from_utf8(buf).unwrap();
        assert!(h.contains("<td>&lt;b&gt;&amp;&quot;x&quot;&lt;/b&gt;</td>"), "{h}");
        assert!(h.contains("<td class=\"null\">NULL</td>") && h.contains("<th>name</th>") && h.ends_with("</html>\n"));

        let mut buf = Vec::new();
        let mut w = ExportWriter::new(ExportFormat::Xml, &mut buf, &cols(), "", None).unwrap();
        w.write_row(&[Some("1".into()), Some("a\u{1}b<".into())]).unwrap();
        w.write_row(&[Some("2".into()), None]).unwrap();
        w.finish().unwrap();
        let x = String::from_utf8(buf).unwrap();
        assert!(x.contains("<col name=\"name\">a\u{FFFD}b&lt;</col>"), "{x}");
        assert!(x.contains("<col name=\"name\" null=\"true\"/>") && x.trim_end().ends_with("</rows>"));
    }

    #[test]
    fn empty_json_array() {
        let mut buf = Vec::new();
        ExportWriter::new(ExportFormat::Json, &mut buf, &cols(), "", None).unwrap().finish().unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&buf).unwrap(), serde_json::json!([]));
    }
}
