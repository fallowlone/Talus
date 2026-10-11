//! Data dictionary: schema documentation (tables, columns, keys, indexes, foreign keys, views)
//! as one self-contained HTML page or a Markdown file.

use std::fmt::Write;
use std::path::Path;

use crate::engine::Session;
use crate::error::DbResult;
use crate::export::escape_markup as esc;
use crate::model::{ObjectKind, ObjectRef, TableStructure};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictFormat {
    Html,
    Markdown,
}

impl DictFormat {
    pub fn extension(self) -> &'static str {
        match self {
            DictFormat::Html => "html",
            DictFormat::Markdown => "md",
        }
    }
}

/// `|` and newlines would break a Markdown table cell.
fn md(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn anchor(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect()
}

pub fn render(
    format: DictFormat,
    title: &str,
    tables: &[(ObjectRef, TableStructure)],
    views: &[ObjectRef],
    generated: &str,
) -> String {
    let mut o = String::new();
    match format {
        DictFormat::Markdown => {
            let _ = writeln!(o, "# {}\n\n_Generated {generated}_\n", md(title));
            let _ = writeln!(o, "{} table(s), {} view(s)\n", tables.len(), views.len());
            for (obj, st) in tables {
                let _ = writeln!(o, "## {}\n", obj.display_name());
                let _ = writeln!(o, "| Column | Type | Null | Default | Key | Comment |\n|---|---|---|---|---|---|");
                let fk_cols: Vec<&str> =
                    st.foreign_keys.iter().flat_map(|f| f.columns.iter().map(String::as_str)).collect();
                for c in &st.columns {
                    let key = if c.primary_key {
                        "PK"
                    } else if fk_cols.contains(&c.name.as_str()) {
                        "FK"
                    } else {
                        ""
                    };
                    let _ = writeln!(
                        o,
                        "| {} | {} | {} | {} | {key} | {} |",
                        md(&c.name),
                        md(&c.data_type),
                        if c.nullable { "yes" } else { "no" },
                        md(c.default.as_deref().unwrap_or("")),
                        md(c.comment.as_deref().unwrap_or(""))
                    );
                }
                if !st.indexes.is_empty() {
                    let _ = writeln!(o, "\n**Indexes**\n");
                    for i in &st.indexes {
                        let _ = writeln!(
                            o,
                            "- `{}` ({}){}{}",
                            i.name,
                            i.columns.join(", "),
                            if i.unique { " unique" } else { "" },
                            if i.primary { " primary" } else { "" }
                        );
                    }
                }
                if !st.foreign_keys.is_empty() {
                    let _ = writeln!(o, "\n**Foreign keys**\n");
                    for f in &st.foreign_keys {
                        let _ = writeln!(
                            o,
                            "- `{}`: ({}) → {} ({}) on update {}, on delete {}",
                            f.name,
                            f.columns.join(", "),
                            f.ref_table,
                            f.ref_columns.join(", "),
                            f.on_update,
                            f.on_delete
                        );
                    }
                }
                o.push('\n');
            }
            if !views.is_empty() {
                let _ = writeln!(o, "## Views\n");
                for v in views {
                    let _ = writeln!(o, "- {}", v.display_name());
                }
            }
        }
        DictFormat::Html => {
            let _ = write!(
                o,
                "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>{t}</title>\n<style>\
                 body{{font:14px -apple-system,Helvetica,sans-serif;margin:2rem auto;max-width:960px;color:#171717}}\
                 h1{{font-size:1.6rem}}h2{{margin-top:2.2rem;border-bottom:1px solid #ddd;padding-bottom:.3rem}}\
                 table{{border-collapse:collapse;width:100%;margin:.6rem 0}}th,td{{border:1px solid #ddd;padding:4px 8px;text-align:left}}\
                 th{{background:#f4f4f4}}code{{background:#f4f4f4;padding:0 3px}}.k{{font-weight:600}}nav a{{margin-right:.8rem}}\
                 </style></head><body>\n<h1>{t}</h1>\n<p>Generated {g} · {n} table(s), {v} view(s)</p>\n<nav>",
                t = esc(title),
                g = esc(generated),
                n = tables.len(),
                v = views.len()
            );
            for (obj, _) in tables {
                let _ = write!(o, "<a href=\"#{}\">{}</a>", anchor(&obj.display_name()), esc(&obj.name));
            }
            o.push_str("</nav>\n");
            for (obj, st) in tables {
                let _ = writeln!(o, "<h2 id=\"{}\">{}</h2>", anchor(&obj.display_name()), esc(&obj.display_name()));
                o.push_str("<table><thead><tr><th>Column</th><th>Type</th><th>Null</th><th>Default</th><th>Key</th><th>Comment</th></tr></thead><tbody>\n");
                let fk_cols: Vec<&str> =
                    st.foreign_keys.iter().flat_map(|f| f.columns.iter().map(String::as_str)).collect();
                for c in &st.columns {
                    let key = if c.primary_key {
                        "PK"
                    } else if fk_cols.contains(&c.name.as_str()) {
                        "FK"
                    } else {
                        ""
                    };
                    let _ = writeln!(
                        o,
                        "<tr><td class=\"k\">{}</td><td>{}</td><td>{}</td><td>{}</td><td>{key}</td><td>{}</td></tr>",
                        esc(&c.name),
                        esc(&c.data_type),
                        if c.nullable { "yes" } else { "no" },
                        esc(c.default.as_deref().unwrap_or("")),
                        esc(c.comment.as_deref().unwrap_or(""))
                    );
                }
                o.push_str("</tbody></table>\n");
                if !st.indexes.is_empty() {
                    o.push_str("<p><b>Indexes</b></p><ul>");
                    for i in &st.indexes {
                        let _ = write!(
                            o,
                            "<li><code>{}</code> ({}){}{}</li>",
                            esc(&i.name),
                            esc(&i.columns.join(", ")),
                            if i.unique { " unique" } else { "" },
                            if i.primary { " primary" } else { "" }
                        );
                    }
                    o.push_str("</ul>\n");
                }
                if !st.foreign_keys.is_empty() {
                    o.push_str("<p><b>Foreign keys</b></p><ul>");
                    for f in &st.foreign_keys {
                        let _ = write!(
                            o,
                            "<li><code>{}</code>: ({}) → {} ({}) on update {}, on delete {}</li>",
                            esc(&f.name),
                            esc(&f.columns.join(", ")),
                            esc(&f.ref_table),
                            esc(&f.ref_columns.join(", ")),
                            esc(&f.on_update),
                            esc(&f.on_delete)
                        );
                    }
                    o.push_str("</ul>\n");
                }
            }
            if !views.is_empty() {
                o.push_str("<h2>Views</h2><ul>");
                for v in views {
                    let _ = write!(o, "<li>{}</li>", esc(&v.display_name()));
                }
                o.push_str("</ul>\n");
            }
            o.push_str("</body></html>\n");
        }
    }
    o
}

/// Writes the dictionary of `database` / `schema` to `path`; returns the number of tables.
pub async fn write_dictionary(
    s: &mut Session,
    database: &str,
    schema: Option<&str>,
    format: DictFormat,
    path: &Path,
) -> DbResult<usize> {
    let tables = s.schema_model(database, schema).await?;
    let views: Vec<ObjectRef> =
        s.list_objects(database, schema).await?.into_iter().filter(|o| o.kind == ObjectKind::View).collect();
    let title = format!("Data dictionary — {}", schema.unwrap_or(database));
    let when = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    std::fs::write(path, render(format, &title, &tables, &views, &when))?;
    Ok(tables.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnInfo, ForeignKeyInfo, IndexInfo};

    fn model() -> Vec<(ObjectRef, TableStructure)> {
        let col = |n: &str, pk: bool, comment: Option<&str>| ColumnInfo {
            name: n.into(),
            data_type: "int".into(),
            nullable: !pk,
            default: Some("0".into()),
            primary_key: pk,
            comment: comment.map(String::from),
            extra: None,
        };
        vec![(
            ObjectRef {
                database: "d".into(),
                schema: Some("s".into()),
                name: "or<ders".into(),
                kind: ObjectKind::Table,
            },
            TableStructure {
                columns: vec![col("id", true, None), col("cid", false, Some("a|b <c>"))],
                indexes: vec![IndexInfo {
                    name: "ix".into(),
                    columns: vec!["cid".into()],
                    unique: true,
                    primary: false,
                }],
                foreign_keys: vec![ForeignKeyInfo {
                    name: "fk".into(),
                    columns: vec!["cid".into()],
                    ref_table: "s.customers".into(),
                    ref_columns: vec!["id".into()],
                    on_update: "NO ACTION".into(),
                    on_delete: "CASCADE".into(),
                }],
            },
        )]
    }

    #[test]
    fn markdown_lists_keys_and_escapes_cells() {
        let md = render(DictFormat::Markdown, "T", &model(), &[], "now");
        assert!(md.contains("## s.or<ders") && md.contains("| id | int | no | 0 | PK |  |"), "{md}");
        assert!(md.contains("| cid | int | yes | 0 | FK | a\\|b <c> |"), "{md}");
        assert!(
            md.contains("- `ix` (cid) unique")
                && md.contains("→ s.customers (id) on update NO ACTION, on delete CASCADE")
        );
    }

    #[test]
    fn html_escapes_names_and_links_the_toc() {
        let views = vec![ObjectRef { database: "d".into(), schema: None, name: "v<1>".into(), kind: ObjectKind::View }];
        let h = render(DictFormat::Html, "A & B", &model(), &views, "now");
        assert!(h.contains("<title>A &amp; B</title>") && h.contains("or&lt;ders"));
        assert!(h.contains("a|b &lt;c&gt;") && h.contains("href=\"#s-or-ders\"") && h.contains("id=\"s-or-ders\""));
        assert!(h.contains("<li>v&lt;1&gt;</li>") && !h.contains("<c>"));
    }
}
