//! Test-data generation: fills a table with plausible rows chosen from each column's type and
//! name. Auto-numbered columns are left to the server, foreign keys draw from existing parent
//! rows, unique numeric keys continue after the current maximum. Seeded, so runs are repeatable.

use std::collections::HashMap;

use crate::engine::{ScriptOptions, Session};
use crate::error::{DbError, DbResult};
use crate::model::{ColumnInfo, Engine, ObjectRef, Outcome, TableStructure};
use crate::sql::Dialect;

pub const MAX_ROWS: usize = 1_000_000;
/// Parent rows read to draw foreign-key values from.
const FK_POOL: usize = 1000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
    fn pick<'a>(&mut self, items: &'a [&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

const FIRST: [&str; 12] =
    ["Anna", "Ben", "Clara", "David", "Elena", "Felix", "Greta", "Hugo", "Ines", "Jonas", "Klara", "Liam"];
const LAST: [&str; 12] =
    ["Meyer", "Novak", "Rossi", "Silva", "Kim", "Olsen", "Dubois", "Garcia", "Ivanov", "Jensen", "Kowal", "Berg"];
const CITY: [&str; 8] = ["Oslo", "Rome", "Lyon", "Porto", "Graz", "Riga", "Bern", "Gent"];
const WORDS: [&str; 16] = [
    "alpha", "bravo", "cedar", "delta", "ember", "forge", "grain", "harbor", "iris", "jade", "kernel", "lumen",
    "mosaic", "nectar", "orbit", "prism",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gen {
    Int {
        lo: i64,
        hi: i64,
    },
    Decimal,
    Bool,
    Date,
    DateTime,
    Time,
    Uuid,
    Json,
    Text,
    /// Binary / spatial: not generated.
    Unsupported,
}

fn base_type(t: &str) -> String {
    t.to_ascii_lowercase().split('(').next().unwrap_or("").trim().to_string()
}

fn gen_for(engine: Engine, c: &ColumnInfo) -> Gen {
    let t = base_type(&c.data_type);
    let has = |ks: &[&str]| ks.iter().any(|k| t.contains(k));
    if has(&[
        "blob",
        "bytea",
        "binary",
        "image",
        "geometry",
        "geography",
        "point",
        "hierarchyid",
        "rowversion",
        "timestamp",
    ]) && !(t.contains("timestamp") && engine != Engine::SqlServer)
    {
        return Gen::Unsupported;
    }
    if t == "tinyint" && c.data_type.contains("(1)") || has(&["bool", "bit"]) {
        return Gen::Bool;
    }
    if has(&["tinyint"]) {
        return Gen::Int { lo: 0, hi: 100 };
    }
    if has(&["smallint"]) {
        return Gen::Int { lo: 0, hi: 30_000 };
    }
    if has(&["bigint", "int8"]) {
        return Gen::Int { lo: 0, hi: 1_000_000_000 };
    }
    if has(&["int", "serial"]) && !t.contains("interval") {
        return Gen::Int { lo: 0, hi: 1_000_000 };
    }
    if has(&["numeric", "decimal", "float", "double", "real", "money", "number"]) {
        return Gen::Decimal;
    }
    if has(&["uuid", "uniqueidentifier"]) {
        return Gen::Uuid;
    }
    if has(&["json"]) {
        return Gen::Json;
    }
    if t == "date" {
        return Gen::Date;
    }
    if has(&["timestamp", "datetime"]) {
        return Gen::DateTime;
    }
    if t == "time" || t == "timetz" {
        return Gen::Time;
    }
    Gen::Text
}

/// Length limit of `varchar(30)` / `char(5)` / `nvarchar(50)`; `None` for unbounded / `max`.
fn max_len(data_type: &str) -> Option<usize> {
    let open = data_type.find('(')?;
    let inner = &data_type[open + 1..data_type.find(')')?];
    inner.split(',').next()?.trim().parse().ok()
}

/// Values of an `enum('a','b')` column.
fn enum_values(data_type: &str) -> Option<Vec<String>> {
    let lower = data_type.to_ascii_lowercase();
    if !lower.starts_with("enum(") {
        return None;
    }
    let inner = &data_type[5..data_type.rfind(')')?];
    Some(inner.split(',').map(|v| v.trim().trim_matches('\'').to_string()).collect())
}

fn text_value(name: &str, r: &mut Rng, seq: u64) -> String {
    let n = name.to_ascii_lowercase();
    let has = |ks: &[&str]| ks.iter().any(|k| n.contains(k));
    if has(&["email", "mail"]) {
        format!("{}.{}{}@example.test", r.pick(&FIRST).to_lowercase(), r.pick(&LAST).to_lowercase(), seq)
    } else if has(&["first"]) && has(&["name"]) {
        r.pick(&FIRST).to_string()
    } else if has(&["last", "surname", "family"]) {
        r.pick(&LAST).to_string()
    } else if has(&["name"]) {
        format!("{} {}", r.pick(&FIRST), r.pick(&LAST))
    } else if has(&["city", "town"]) {
        r.pick(&CITY).to_string()
    } else if has(&["phone", "tel", "mobile"]) {
        format!("+1-555-{:04}", r.below(10_000))
    } else if has(&["country"]) {
        r.pick(&["Norway", "Italy", "France", "Portugal", "Austria", "Latvia"]).to_string()
    } else if has(&["url", "website"]) {
        format!("https://example.test/{}", r.pick(&WORDS))
    } else if has(&["desc", "comment", "note", "body", "text", "title", "summary"]) {
        let count = 3 + r.below(6);
        (0..count).map(|_| r.pick(&WORDS)).collect::<Vec<_>>().join(" ")
    } else {
        format!("{}-{seq}", r.pick(&WORDS))
    }
}

fn date_of(r: &mut Rng) -> (i64, i64, i64) {
    (r.range(2015, 2025), r.range(1, 12), r.range(1, 28))
}

fn value(g: Gen, c: &ColumnInfo, r: &mut Rng, seq: u64) -> Option<String> {
    if let Some(vals) = enum_values(&c.data_type) {
        return Some(vals[r.below(vals.len() as u64) as usize].clone());
    }
    Some(match g {
        Gen::Int { lo, hi } => r.range(lo, hi).to_string(),
        Gen::Decimal => format!("{}.{:02}", r.range(0, 9999), r.below(100)),
        Gen::Bool => r.below(2).to_string(),
        Gen::Date => {
            let (y, m, d) = date_of(r);
            format!("{y:04}-{m:02}-{d:02}")
        }
        Gen::DateTime => {
            let (y, m, d) = date_of(r);
            format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", r.below(24), r.below(60), r.below(60))
        }
        Gen::Time => format!("{:02}:{:02}:{:02}", r.below(24), r.below(60), r.below(60)),
        Gen::Uuid => format!(
            "{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
            r.next() as u32,
            r.below(65_536),
            r.below(4096),
            r.below(4096),
            r.next() & 0xffff_ffff_ffff
        ),
        Gen::Json => format!("{{\"n\": {}}}", r.below(1000)),
        Gen::Text => {
            let mut t = text_value(&c.name, r, seq);
            if let Some(n) = max_len(&c.data_type) {
                t = t.chars().take(n).collect();
            }
            t
        }
        Gen::Unsupported => return None,
    })
}

fn is_auto(c: &ColumnInfo) -> bool {
    let extra = c.extra.as_deref().unwrap_or("").to_ascii_lowercase();
    let default = c.default.as_deref().unwrap_or("").to_ascii_lowercase();
    extra.contains("auto_increment")
        || extra.contains("identity")
        || extra.contains("generated")
        || extra.contains("computed")
        || default.contains("nextval")
        || c.data_type.to_ascii_lowercase().contains("serial")
}

/// Column names and their generated rows.
pub type Generated = (Vec<String>, Vec<Vec<Option<String>>>);

/// Rows for `structure`: `(column names, rows)`. `fk_pools` maps a foreign-key column to parent values,
/// `starts` maps unique numeric columns to their first value.
pub fn generate(
    engine: Engine,
    structure: &TableStructure,
    rows: usize,
    seed: u64,
    fk_pools: &HashMap<String, Vec<String>>,
    starts: &HashMap<String, i64>,
    // Rows already in the table: sequence numbers continue after them, so repeated runs stay unique.
    existing: u64,
) -> DbResult<Generated> {
    if rows == 0 || rows > MAX_ROWS {
        return Err(DbError::Invalid(format!("choose between 1 and {MAX_ROWS} rows")));
    }
    let unique: Vec<&str> = structure
        .indexes
        .iter()
        .filter(|i| i.unique && i.columns.len() == 1)
        .map(|i| i.columns[0].as_str())
        .chain(if structure.primary_key().len() == 1 { structure.primary_key() } else { Vec::new() })
        .collect();
    let cols: Vec<&ColumnInfo> = structure.columns.iter().filter(|c| !is_auto(c)).collect();
    if cols.is_empty() {
        return Err(DbError::Invalid("every column is generated by the server; nothing to fill".into()));
    }
    let gens: Vec<Gen> = cols.iter().map(|c| gen_for(engine, c)).collect();
    for (c, g) in cols.iter().zip(&gens) {
        if *g == Gen::Unsupported && !c.nullable && c.default.is_none() && !fk_pools.contains_key(&c.name) {
            return Err(DbError::Invalid(format!(
                "cannot generate values for the required column {} ({}); give it a default or make it nullable",
                c.name, c.data_type
            )));
        }
    }
    let mut rng = Rng(seed | 1);
    let mut out = Vec::with_capacity(rows);
    for i in 0..rows {
        let seq = existing + i as u64 + 1;
        let row: Vec<Option<String>> = cols
            .iter()
            .zip(&gens)
            .map(|(c, g)| {
                if let Some(pool) = fk_pools.get(&c.name).filter(|p| !p.is_empty()) {
                    return Some(pool[rng.below(pool.len() as u64) as usize].clone());
                }
                let is_unique = unique.contains(&c.name.as_str());
                if let (Some(start), true) = (starts.get(&c.name), is_unique) {
                    return Some((start + i as i64).to_string());
                }
                if *g == Gen::Unsupported {
                    return None;
                }
                // Optional columns are sometimes empty, unique and key columns never.
                if c.nullable && !is_unique && rng.below(20) == 0 {
                    return None;
                }
                let mut v = value(*g, c, &mut rng, seq);
                if is_unique && *g == Gen::Text {
                    // Guarantee uniqueness by suffixing the sequence number inside the length limit.
                    let tail = format!("-{seq}");
                    let limit = max_len(&c.data_type).unwrap_or(usize::MAX);
                    let head: String = v.unwrap_or_default().chars().take(limit.saturating_sub(tail.len())).collect();
                    v = Some(format!("{head}{tail}"));
                }
                v
            })
            .collect();
        out.push(row);
    }
    Ok((cols.iter().map(|c| c.name.clone()).collect(), out))
}

async fn scalar(s: &mut Session, sql: &str) -> DbResult<Option<String>> {
    let out = s.run_script(sql, &ScriptOptions::default()).await;
    if let Some(e) = out.errors.first() {
        return Err(DbError::Invalid(e.error.to_string()));
    }
    Ok(match out.results.into_iter().next().map(|r| r.outcome) {
        Some(Outcome::Rows(q)) => q.rows.first().and_then(|r| r.first().cloned().flatten()),
        _ => None,
    })
}

/// Inserts `rows` generated rows in one transaction; returns how many were inserted.
pub async fn fill_table(s: &mut Session, obj: &ObjectRef, rows: usize, seed: u64) -> DbResult<u64> {
    let engine = s.engine();
    if !engine.is_sql() {
        return Err(DbError::Invalid("data generation works on SQL tables".into()));
    }
    let d = Dialect(engine);
    let structure = s.table_structure(obj).await?;
    let table = d.qualified(obj)?;

    // Parents for single-column foreign keys.
    let mut pools: HashMap<String, Vec<String>> = HashMap::new();
    for fk in structure.foreign_keys.iter().filter(|f| f.columns.len() == 1 && f.ref_columns.len() == 1) {
        let parent_name =
            fk.ref_table.rsplit('.').next().unwrap_or(&fk.ref_table).trim_matches(['"', '`', '[', ']']).to_string();
        let parent = ObjectRef { name: parent_name, ..obj.clone() };
        let (pt, pc) = (d.qualified(&parent)?, d.quote_ident(&fk.ref_columns[0])?);
        let sql = match engine {
            Engine::SqlServer => format!("SELECT TOP ({FK_POOL}) {pc} FROM {pt}"),
            _ => format!("SELECT {pc} FROM {pt} LIMIT {FK_POOL}"),
        };
        let out = s.run_script(&sql, &ScriptOptions::default()).await;
        if let Some(e) = out.errors.first() {
            return Err(DbError::Invalid(format!("cannot read the parent rows of {}: {}", fk.name, e.error)));
        }
        let values: Vec<String> = match out.results.into_iter().next().map(|r| r.outcome) {
            Some(Outcome::Rows(q)) => q.rows.into_iter().filter_map(|mut r| r.pop().flatten()).collect(),
            _ => Vec::new(),
        };
        if values.is_empty() && structure.column(&fk.columns[0]).is_some_and(|c| !c.nullable) {
            return Err(DbError::Invalid(format!(
                "the table {} that {} refers to is empty; fill it first",
                fk.ref_table, fk.name
            )));
        }
        pools.insert(fk.columns[0].clone(), values);
    }

    // Unique numeric columns continue after the largest existing value.
    let mut starts: HashMap<String, i64> = HashMap::new();
    for c in &structure.columns {
        let unique = structure.primary_key() == [c.name.as_str()]
            || structure.indexes.iter().any(|i| i.unique && i.columns == [c.name.clone()]);
        if unique && matches!(gen_for(engine, c), Gen::Int { .. }) && !is_auto(c) && !pools.contains_key(&c.name) {
            let q = d.quote_ident(&c.name)?;
            let max = scalar(s, &format!("SELECT MAX({q}) FROM {table}")).await?;
            starts.insert(c.name.clone(), max.and_then(|m| m.parse::<i64>().ok()).unwrap_or(0) + 1);
        }
    }

    let existing = scalar(s, &format!("SELECT COUNT(*) FROM {table}")).await?.and_then(|c| c.parse().ok()).unwrap_or(0);
    let (columns, data) = generate(engine, &structure, rows, seed, &pools, &starts, existing)?;
    let it = data.into_iter().map(Ok);
    crate::import::load_rows(s, obj, &structure, &columns, it).await.map(|r| r.rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ForeignKeyInfo, IndexInfo};

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            nullable,
            default: None,
            primary_key: false,
            comment: None,
            extra: None,
        }
    }

    fn table() -> TableStructure {
        let mut id = col("id", "int", false);
        id.primary_key = true;
        TableStructure {
            columns: vec![
                id,
                col("email", "varchar(30)", false),
                col("full_name", "varchar(12)", true),
                col("age", "smallint", true),
                col("born", "date", true),
                col("active", "tinyint(1)", false),
                col("kind", "enum('a','b','c')", false),
                col("avatar", "blob", true),
            ],
            indexes: vec![IndexInfo { name: "ux".into(), columns: vec!["email".into()], unique: true, primary: false }],
            foreign_keys: Vec::new(),
        }
    }

    #[test]
    fn rows_follow_types_names_limits_and_uniqueness() {
        let starts = HashMap::from([("id".to_string(), 100)]);
        let (cols, rows) = generate(Engine::MySql, &table(), 300, 7, &HashMap::new(), &starts, 0).unwrap();
        assert_eq!(cols, ["id", "email", "full_name", "age", "born", "active", "kind", "avatar"]);
        assert_eq!(rows[0][0].as_deref(), Some("100"));
        assert_eq!(rows[299][0].as_deref(), Some("399"));
        let emails: std::collections::HashSet<_> = rows.iter().map(|r| r[1].clone().unwrap()).collect();
        assert_eq!(emails.len(), 300, "unique column stays unique");
        assert!(rows.iter().all(|r| r[1].as_deref().unwrap().len() <= 30));
        assert!(rows.iter().filter_map(|r| r[2].as_deref()).all(|n| n.chars().count() <= 12));
        assert!(rows.iter().any(|r| r[2].is_none()), "nullable columns get some NULLs");
        assert!(rows.iter().all(|r| r[1].is_some() && r[5].is_some()), "required columns never NULL");
        assert!(
            rows.iter()
                .filter_map(|r| r[3].as_deref())
                .all(|a| a.parse::<i64>().is_ok_and(|v| (0..=30_000).contains(&v)))
        );
        assert!(rows.iter().filter_map(|r| r[4].as_deref()).all(|d| d.len() == 10 && d.as_bytes()[4] == b'-'));
        assert!(rows.iter().all(|r| matches!(r[5].as_deref(), Some("0" | "1"))));
        assert!(rows.iter().all(|r| matches!(r[6].as_deref(), Some("a" | "b" | "c"))));
        assert!(rows.iter().all(|r| r[7].is_none()), "binary columns are left empty");
        // Same seed, same data; another seed differs.
        let again = generate(Engine::MySql, &table(), 300, 7, &HashMap::new(), &starts, 0).unwrap().1;
        assert_eq!(again, rows);
        assert_ne!(generate(Engine::MySql, &table(), 300, 8, &HashMap::new(), &starts, 0).unwrap().1, rows);
    }

    #[test]
    fn foreign_keys_draw_from_parents_and_auto_columns_are_skipped() {
        let mut t = table();
        t.columns[0].extra = Some("auto_increment".into());
        t.columns.push(col("owner", "int", false));
        t.foreign_keys.push(ForeignKeyInfo {
            name: "fk".into(),
            columns: vec!["owner".into()],
            ref_table: "owners".into(),
            ref_columns: vec!["id".into()],
            on_update: String::new(),
            on_delete: String::new(),
        });
        let pools = HashMap::from([("owner".to_string(), vec!["7".to_string(), "9".to_string()])]);
        let (cols, rows) = generate(Engine::MySql, &t, 50, 1, &pools, &HashMap::new(), 0).unwrap();
        assert!(!cols.contains(&"id".to_string()));
        let owner = cols.iter().position(|c| c == "owner").unwrap();
        assert!(rows.iter().all(|r| matches!(r[owner].as_deref(), Some("7" | "9"))));
    }

    #[test]
    fn refuses_impossible_requests() {
        assert!(generate(Engine::Postgres, &table(), 0, 1, &HashMap::new(), &HashMap::new(), 0).is_err());
        let mut t = table();
        t.columns[7].nullable = false;
        let err = generate(Engine::Postgres, &t, 5, 1, &HashMap::new(), &HashMap::new(), 0).unwrap_err();
        assert!(err.to_string().contains("avatar"), "{err}");
        assert_eq!(max_len("varchar(30)"), Some(30));
        assert_eq!(max_len("nvarchar(max)"), None);
        assert_eq!(max_len("numeric(10,2)"), Some(10));
    }
}
