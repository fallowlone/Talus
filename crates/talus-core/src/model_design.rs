//! Data modelling: a database-independent model of tables, columns, keys and indexes that can be
//! reverse-engineered from a live schema and forward-engineered into DDL for any SQL engine.
//! A model is conceptual, logical or physical (`ModelKind`); only a physical model produces SQL.
//! Converting between kinds keeps every detail, the kind only decides what is shown and allowed.
//! Column types are logical (`Integer`, `Varchar(40)`, …) and mapped per engine; a type the
//! mapper does not know is kept verbatim as `Native`.

use serde::{Deserialize, Serialize};

use crate::dump::topo_order;
use crate::error::{DbError, DbResult};
use crate::model::{ColumnInfo, Engine, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectRef, TableStructure};
use crate::sql::Dialect;
use crate::sql::constraints::{FkDef, IndexDef, create_index, fk_clause};
use crate::sql::ddl::{ColumnDef, create_table_with};
use crate::sql::sync::{SyncPlan, structure_sync};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LType {
    SmallInt,
    Integer,
    BigInt,
    Decimal {
        precision: u8,
        scale: u8,
    },
    Float,
    Boolean,
    Text,
    Varchar(u32),
    Char(u32),
    Date,
    Time,
    DateTime,
    Uuid,
    Json,
    Blob,
    /// SQL type text that the mapper cannot translate; used as is on every engine.
    Native(String),
}

impl LType {
    /// Types offered by the editor (Native is typed by hand).
    pub fn choices() -> Vec<LType> {
        vec![
            LType::SmallInt,
            LType::Integer,
            LType::BigInt,
            LType::Decimal { precision: 10, scale: 2 },
            LType::Float,
            LType::Boolean,
            LType::Text,
            LType::Varchar(255),
            LType::Char(1),
            LType::Date,
            LType::Time,
            LType::DateTime,
            LType::Uuid,
            LType::Json,
            LType::Blob,
        ]
    }

    pub fn label(&self) -> String {
        match self {
            LType::SmallInt => "smallint".into(),
            LType::Integer => "integer".into(),
            LType::BigInt => "bigint".into(),
            LType::Decimal { precision, scale } => format!("decimal({precision},{scale})"),
            LType::Float => "float".into(),
            LType::Boolean => "boolean".into(),
            LType::Text => "text".into(),
            LType::Varchar(n) => format!("varchar({n})"),
            LType::Char(n) => format!("char({n})"),
            LType::Date => "date".into(),
            LType::Time => "time".into(),
            LType::DateTime => "datetime".into(),
            LType::Uuid => "uuid".into(),
            LType::Json => "json".into(),
            LType::Blob => "blob".into(),
            LType::Native(t) => t.clone(),
        }
    }

    /// The type as written for `engine`.
    pub fn sql(&self, engine: Engine) -> String {
        use Engine::*;
        match (self, engine) {
            // Oracle has no integer, boolean or time-of-day types: NUMBER precisions stand in for them
            // (`from_sql` reads them back the same way) and a DATE carries its time.
            (LType::SmallInt, Oracle) => "NUMBER(5)".into(),
            (LType::Integer, Oracle) => "NUMBER(10)".into(),
            (LType::BigInt, Oracle) => "NUMBER(19)".into(),
            (LType::Decimal { precision, scale }, Oracle) => format!("NUMBER({precision},{scale})"),
            (LType::Float, Oracle) => "BINARY_DOUBLE".into(),
            (LType::Boolean, Oracle) => "NUMBER(1)".into(),
            (LType::Text | LType::Json, Oracle) => "CLOB".into(),
            (LType::Varchar(n), Oracle) => format!("VARCHAR2({n})"),
            (LType::Char(n), Oracle) => format!("CHAR({n})"),
            (LType::Date | LType::Time, Oracle) => "DATE".into(),
            (LType::DateTime, Oracle) => "TIMESTAMP".into(),
            (LType::Uuid, Oracle) => "RAW(16)".into(),
            (LType::Blob, Oracle) => "BLOB".into(),
            // Snowflake precisions mirror Oracle's so `from_sql` reads them back the same way.
            (LType::SmallInt, Snowflake) => "NUMBER(5)".into(),
            (LType::Integer, Snowflake) => "NUMBER(10)".into(),
            (LType::BigInt, Snowflake) => "NUMBER(19)".into(),
            (LType::Decimal { precision, scale }, Snowflake) => format!("NUMBER({precision},{scale})"),
            (LType::Float, Snowflake) => "FLOAT".into(),
            (LType::Boolean, Snowflake) => "BOOLEAN".into(),
            (LType::Text, Snowflake) => "TEXT".into(),
            (LType::DateTime, Snowflake) => "TIMESTAMP_NTZ".into(),
            (LType::Uuid, Snowflake) => "VARCHAR(36)".into(),
            (LType::Json, Snowflake) => "VARIANT".into(),
            (LType::Blob, Snowflake) => "BINARY".into(),
            (LType::SmallInt, _) if engine == Sqlite => "INTEGER".into(),
            (LType::Integer | LType::BigInt, Sqlite) => "INTEGER".into(),
            (LType::SmallInt, _) => "smallint".into(),
            (LType::Integer, _) => if engine == Postgres { "integer" } else { "int" }.into(),
            (LType::BigInt, _) => "bigint".into(),
            (LType::Decimal { precision, scale }, Sqlite) => {
                let _ = (precision, scale);
                "NUMERIC".into()
            }
            (LType::Decimal { precision, scale }, _) => format!("decimal({precision},{scale})"),
            (LType::Float, Sqlite) => "REAL".into(),
            (LType::Float, Postgres) => "double precision".into(),
            (LType::Float, _) => "float".into(),
            (LType::Boolean, Postgres) => "boolean".into(),
            (LType::Boolean, MySql) => "tinyint(1)".into(),
            (LType::Boolean, SqlServer) => "bit".into(),
            (LType::Boolean, _) => "INTEGER".into(),
            (LType::Text, Sqlite) => "TEXT".into(),
            (LType::Text, Postgres | MySql) => "text".into(),
            (LType::Text, _) => "nvarchar(max)".into(),
            (LType::Varchar(n), SqlServer) => format!("nvarchar({n})"),
            (LType::Varchar(n), Sqlite) => format!("VARCHAR({n})"),
            (LType::Varchar(n), _) => format!("varchar({n})"),
            (LType::Char(n), SqlServer) => format!("nchar({n})"),
            (LType::Char(n), _) => format!("char({n})"),
            (LType::Date, Sqlite) => "DATE".into(),
            (LType::Date, _) => "date".into(),
            (LType::Time, Sqlite) => "TIME".into(),
            (LType::Time, _) => "time".into(),
            (LType::DateTime, Postgres) => "timestamp".into(),
            (LType::DateTime, MySql) => "datetime".into(),
            (LType::DateTime, SqlServer) => "datetime2".into(),
            (LType::DateTime, _) => "DATETIME".into(),
            (LType::Uuid, Postgres) => "uuid".into(),
            (LType::Uuid, MySql) => "char(36)".into(),
            (LType::Uuid, SqlServer) => "uniqueidentifier".into(),
            (LType::Uuid, _) => "TEXT".into(),
            (LType::Json, Postgres) => "jsonb".into(),
            (LType::Json, MySql) => "json".into(),
            (LType::Json, SqlServer) => "nvarchar(max)".into(),
            (LType::Json, _) => "TEXT".into(),
            (LType::Blob, Postgres) => "bytea".into(),
            (LType::Blob, MySql) => "blob".into(),
            (LType::Blob, SqlServer) => "varbinary(max)".into(),
            (LType::Blob, _) => "BLOB".into(),
            (LType::Native(t), _) => t.clone(),
        }
    }

    /// Best-effort translation of an engine's type text back into a logical type.
    pub fn from_sql(raw: &str) -> LType {
        // `varchar2(50 char)` / `varchar2(50 byte)`: the length semantics are not part of the logical type.
        let lower = raw.trim().to_ascii_lowercase().replace(" char)", ")").replace(" byte)", ")");
        let base = lower.split('(').next().unwrap_or("").trim().to_string();
        let args: Vec<u32> = lower
            .split_once('(')
            .and_then(|(_, r)| r.split(')').next())
            .map(|a| a.split(',').filter_map(|x| x.trim().parse().ok()).collect())
            .unwrap_or_default();
        let one = || args.first().copied();
        // Oracle NUMBER: a scale of 0 (or `*,0`) is a whole number, sized by its precision.
        if base == "number" {
            let whole = lower.contains("(*,0)") || (args.len() == 1) || (args.len() == 2 && args[1] == 0);
            if whole {
                return match if lower.contains("(*,0)") { Some(10) } else { one() } {
                    Some(1) => LType::Boolean,
                    Some(p) if p <= 5 => LType::SmallInt,
                    Some(p) if p <= 10 => LType::Integer,
                    Some(p) if p <= 19 => LType::BigInt,
                    Some(p) => LType::Decimal { precision: p.min(38) as u8, scale: 0 },
                    None => LType::Native(raw.trim().to_string()),
                };
            }
        }
        match base.as_str() {
            "smallint" | "int2" | "tinyint" if !(base == "tinyint" && one() == Some(1)) => LType::SmallInt,
            "tinyint" => LType::Boolean,
            "int" | "integer" | "int4" | "mediumint" => LType::Integer,
            "bigint" | "int8" => LType::BigInt,
            "decimal" | "numeric" | "number" | "money" if args.len() == 2 => {
                LType::Decimal { precision: args[0].min(38) as u8, scale: args[1].min(38) as u8 }
            }
            "decimal" | "numeric" => LType::Decimal { precision: 18, scale: 2 },
            "float" | "double" | "double precision" | "real" | "float4" | "float8" | "binary_double"
            | "binary_float" => LType::Float,
            "raw" if one() == Some(16) => LType::Uuid,
            "raw" | "long raw" => LType::Blob,
            "bool" | "boolean" | "bit" => LType::Boolean,
            "text" | "mediumtext" | "longtext" | "tinytext" | "clob" | "ntext" => LType::Text,
            "varchar" | "nvarchar" | "character varying" | "varchar2" | "nvarchar2" => {
                one().map_or(LType::Text, LType::Varchar)
            }
            "char" | "nchar" | "character" | "bpchar" => LType::Char(one().unwrap_or(1)),
            "date" => LType::Date,
            "time" | "timetz" | "time without time zone" => LType::Time,
            "datetime"
            | "datetime2"
            | "timestamp"
            | "timestamptz"
            | "smalldatetime"
            | "timestamp_ntz"
            | "timestamp_ltz"
            | "timestamp_tz"
            | "timestamp without time zone"
            | "timestamp with time zone" => LType::DateTime,
            "uuid" | "uniqueidentifier" => LType::Uuid,
            "json" | "jsonb" | "variant" | "object" | "array" => LType::Json,
            "blob" | "bytea" | "longblob" | "mediumblob" | "tinyblob" | "varbinary" | "binary" | "image" => LType::Blob,
            _ => LType::Native(raw.trim().to_string()),
        }
    }
}

fn normalize_default(raw: &str) -> String {
    let mut v = raw.trim();
    while v.len() >= 2 && v.starts_with('(') && v.ends_with(')') {
        v = v[1..v.len() - 1].trim();
    }
    // PostgreSQL casts (`'x'::character varying`) and SQL Server national literals (`N'x'`).
    if let Some(i) = v.find("::") {
        v = v[..i].trim();
    }
    if v.len() >= 2 && (v.starts_with("N'") || v.starts_with("n'")) {
        v = &v[1..];
    }
    v.to_string()
}

fn same_default(a: &str, b: &str) -> bool {
    let (a, b) = (normalize_default(a), normalize_default(b));
    let unquote = |s: &str| s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).map(str::to_string);
    let num = |s: &str| unquote(s).unwrap_or_else(|| s.to_string()).parse::<f64>().ok();
    a.eq_ignore_ascii_case(&b)
        || unquote(&a).is_some_and(|x| x == b)
        || unquote(&b).is_some_and(|x| x == a)
        || matches!((num(&a), num(&b)), (Some(x), Some(y)) if x == y)
}

impl MColumn {
    /// The column as `engine` stores it; auto-increment becomes the engine's marker.
    fn info(&self, engine: Engine) -> ColumnInfo {
        let extra = match (self.auto_increment, engine) {
            (true, Engine::MySql) => Some("auto_increment".to_string()),
            (true, Engine::Postgres | Engine::SqlServer | Engine::Oracle) => Some("IDENTITY".to_string()),
            (true, Engine::Snowflake) => Some("AUTOINCREMENT".to_string()),
            _ => None,
        };
        ColumnInfo {
            name: self.name.clone(),
            data_type: self.ty.sql(engine),
            nullable: self.nullable && !self.primary_key,
            default: self.default.clone(),
            primary_key: self.primary_key,
            comment: None,
            extra,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MColumn {
    pub name: String,
    pub ty: LType,
    pub nullable: bool,
    pub primary_key: bool,
    pub auto_increment: bool,
    /// SQL default expression as typed.
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MFk {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MIndex {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MTable {
    pub name: String,
    pub columns: Vec<MColumn>,
    #[serde(default)]
    pub foreign_keys: Vec<MFk>,
    #[serde(default)]
    pub indexes: Vec<MIndex>,
    /// Dimensional / Data Vault role; plain tables have none. Older model files have no role.
    #[serde(default)]
    pub role: TableRole,
    /// Canvas position.
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DesignModel {
    #[serde(default)]
    pub name: String,
    /// Older model files have no kind and are physical.
    #[serde(default)]
    pub kind: ModelKind,
    pub tables: Vec<MTable>,
}

/// Abstraction level of a model (conceptual / logical / physical, as in data-modelling tools).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelKind {
    /// Entities, their attributes' names and keys, and relationships.
    Conceptual,
    /// Adds logical types, nullability and indexes; still engine-independent.
    Logical,
    /// Engine-specific: native types, defaults, auto-increment; forward engineering and comparison.
    #[default]
    Physical,
}

impl ModelKind {
    pub const ALL: [ModelKind; 3] = [ModelKind::Conceptual, ModelKind::Logical, ModelKind::Physical];

    pub fn label(self) -> &'static str {
        match self {
            ModelKind::Conceptual => "Conceptual",
            ModelKind::Logical => "Logical",
            ModelKind::Physical => "Physical",
        }
    }

    /// What a box on the canvas is called at this level.
    pub fn object_noun(self) -> &'static str {
        if self == ModelKind::Physical { "Table" } else { "Entity" }
    }
}

impl MTable {
    pub fn new(name: &str, x: f32, y: f32) -> Self {
        Self {
            name: name.into(),
            columns: vec![MColumn {
                name: "id".into(),
                ty: LType::Integer,
                nullable: false,
                primary_key: true,
                auto_increment: true,
                default: None,
            }],
            foreign_keys: Vec::new(),
            indexes: Vec::new(),
            role: TableRole::None,
            x,
            y,
        }
    }

    /// The first primary-key column (a role template's hash or surrogate key).
    pub fn key_column(&self) -> Option<&MColumn> {
        self.columns.iter().find(|c| c.primary_key)
    }

    /// Data Vault hub: hash key, business key (unique), load date, record source.
    pub fn hub(name: &str, x: f32, y: f32) -> Self {
        let mut t = Self::new(name, x, y);
        t.role = TableRole::Hub;
        t.columns = vec![
            dv_col(&format!("{name}_hk"), LType::Char(32), true),
            dv_col(&format!("{name}_bk"), LType::Varchar(255), false),
            dv_col("load_date", LType::DateTime, false),
            dv_col("record_source", LType::Varchar(100), false),
        ];
        t.indexes = vec![MIndex { name: format!("ux_{name}_bk"), columns: vec![format!("{name}_bk")], unique: true }];
        t
    }

    /// Data Vault link between `parents` (hubs): its own hash key plus each parent's hash key.
    pub fn link(name: &str, parents: &[&MTable], x: f32, y: f32) -> Self {
        let mut t = Self::new(name, x, y);
        t.role = TableRole::Link;
        t.columns = vec![dv_col(&format!("{name}_hk"), LType::Char(32), true)];
        for p in parents {
            t.reference(p);
        }
        t.columns.push(dv_col("load_date", LType::DateTime, false));
        t.columns.push(dv_col("record_source", LType::Varchar(100), false));
        t
    }

    /// Data Vault satellite of `parent` (hub or link): parent key + load date as the key, hash diff.
    pub fn satellite(name: &str, parent: &MTable, x: f32, y: f32) -> Self {
        let mut t = Self::new(name, x, y);
        t.role = TableRole::Satellite;
        t.columns.clear();
        t.reference(parent);
        if let Some(c) = t.columns.first_mut() {
            c.primary_key = true;
        }
        t.columns.push(dv_col("load_date", LType::DateTime, true));
        t.columns.push(dv_col("hash_diff", LType::Char(32), false));
        t.columns.push(dv_col("record_source", LType::Varchar(100), false));
        t
    }

    /// Adds a column for `parent`'s key and a foreign key to it.
    fn reference(&mut self, parent: &MTable) {
        let Some(key) = parent.key_column() else { return };
        self.columns.push(MColumn { primary_key: false, auto_increment: false, nullable: false, ..key.clone() });
        self.foreign_keys.push(MFk {
            name: format!("fk_{}_{}", self.name, parent.name),
            columns: vec![key.name.clone()],
            ref_table: parent.name.clone(),
            ref_columns: vec![key.name.clone()],
            on_update: "NO ACTION".into(),
            on_delete: "NO ACTION".into(),
        });
    }
}

fn dv_col(name: &str, ty: LType, primary_key: bool) -> MColumn {
    MColumn { name: name.into(), ty, nullable: false, primary_key, auto_increment: false, default: None }
}

/// What a table is in a dimensional (star) or Data Vault model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TableRole {
    #[default]
    None,
    Fact,
    Dimension,
    Hub,
    Link,
    Satellite,
}

impl TableRole {
    pub const ALL: [TableRole; 6] =
        [TableRole::None, TableRole::Fact, TableRole::Dimension, TableRole::Hub, TableRole::Link, TableRole::Satellite];

    pub fn label(self) -> &'static str {
        match self {
            TableRole::None => "Table",
            TableRole::Fact => "Fact",
            TableRole::Dimension => "Dimension",
            TableRole::Hub => "Hub",
            TableRole::Link => "Link",
            TableRole::Satellite => "Satellite",
        }
    }
}

impl DesignModel {
    /// Problems that would stop forward engineering (or produce a broken schema).
    /// Rules of the dimensional and Data Vault roles (only for tables that have a role).
    fn role_problems(&self) -> Vec<String> {
        let role_of = |name: &str| self.tables.iter().find(|t| t.name.eq_ignore_ascii_case(name)).map(|t| t.role);
        let mut out = Vec::new();
        for t in &self.tables {
            let parents: Vec<TableRole> = t.foreign_keys.iter().filter_map(|f| role_of(&f.ref_table)).collect();
            let count = |r: TableRole| parents.iter().filter(|p| **p == r).count();
            match t.role {
                TableRole::None => {}
                TableRole::Fact if count(TableRole::Dimension) == 0 => {
                    out.push(format!("Fact {} references no dimension.", t.name));
                }
                TableRole::Dimension if count(TableRole::Fact) > 0 => {
                    out.push(format!("Dimension {} references a fact; facts reference dimensions.", t.name));
                }
                TableRole::Hub if !t.foreign_keys.is_empty() => {
                    out.push(format!("Hub {} has foreign keys; hubs stand alone (use a link).", t.name));
                }
                TableRole::Link if count(TableRole::Hub) < 2 => {
                    out.push(format!("Link {} must reference at least two hubs.", t.name));
                }
                TableRole::Satellite
                    if t.foreign_keys.len() != 1
                        || !matches!(parents.first(), Some(TableRole::Hub | TableRole::Link)) =>
                {
                    out.push(format!("Satellite {} must reference exactly one hub or link.", t.name));
                }
                _ => {}
            }
        }
        out
    }

    pub fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut names = std::collections::HashSet::new();
        for t in &self.tables {
            if t.name.trim().is_empty() {
                out.push("A table has no name.".into());
                continue;
            }
            if !names.insert(t.name.to_lowercase()) {
                out.push(format!("Table {} appears twice.", t.name));
            }
            if t.columns.is_empty() {
                out.push(format!("Table {} has no columns.", t.name));
            }
            let mut cols = std::collections::HashSet::new();
            for c in &t.columns {
                if c.name.trim().is_empty() {
                    out.push(format!("A column of {} has no name.", t.name));
                } else if !cols.insert(c.name.to_lowercase()) {
                    out.push(format!("Column {}.{} appears twice.", t.name, c.name));
                }
                if c.auto_increment && !c.primary_key {
                    out.push(format!("{}.{} is auto-incremented but not a primary key.", t.name, c.name));
                }
            }
            for fk in &t.foreign_keys {
                let Some(parent) = self.tables.iter().find(|p| p.name.eq_ignore_ascii_case(&fk.ref_table)) else {
                    out.push(format!("{}.{} refers to the missing table {}.", t.name, fk.name, fk.ref_table));
                    continue;
                };
                if fk.columns.is_empty() || fk.columns.len() != fk.ref_columns.len() {
                    out.push(format!("{}.{}: the column lists do not match.", t.name, fk.name));
                }
                for c in &fk.columns {
                    if !t.columns.iter().any(|x| x.name.eq_ignore_ascii_case(c)) {
                        out.push(format!("{}.{} uses the missing column {c}.", t.name, fk.name));
                    }
                }
                for c in &fk.ref_columns {
                    if !parent.columns.iter().any(|x| x.name.eq_ignore_ascii_case(c)) {
                        out.push(format!("{}.{} points to the missing column {}.{c}.", t.name, fk.name, parent.name));
                    }
                }
            }
            for ix in &t.indexes {
                for c in &ix.columns {
                    if !t.columns.iter().any(|x| x.name.eq_ignore_ascii_case(c)) {
                        out.push(format!("Index {}.{} uses the missing column {c}.", t.name, ix.name));
                    }
                }
            }
        }
        out.extend(self.role_problems());
        out
    }

    /// Reverse engineering: the tables of a live schema as a model, laid out in a grid.
    pub fn reverse(name: &str, tables: &[(ObjectRef, TableStructure)]) -> DesignModel {
        let mut m = DesignModel { name: name.into(), kind: ModelKind::Physical, tables: Vec::new() };
        for (i, (obj, st)) in tables.iter().enumerate() {
            let columns = st
                .columns
                .iter()
                .map(|c: &ColumnInfo| MColumn {
                    name: c.name.clone(),
                    ty: LType::from_sql(&c.data_type),
                    nullable: c.nullable,
                    primary_key: c.primary_key,
                    auto_increment: c.extra.as_deref().is_some_and(|e| {
                        let l = e.to_ascii_lowercase();
                        l.contains("auto_increment") || l.contains("identity")
                    }) || c
                        .default
                        .as_deref()
                        .is_some_and(|d| d.to_ascii_lowercase().contains("nextval")),
                    default: c.default.clone().filter(|d| !d.to_ascii_lowercase().contains("nextval")),
                })
                .collect();
            m.tables.push(MTable {
                name: obj.name.clone(),
                role: TableRole::None,
                columns,
                foreign_keys: st
                    .foreign_keys
                    .iter()
                    .map(|f| MFk {
                        name: f.name.clone(),
                        columns: f.columns.clone(),
                        ref_table: f
                            .ref_table
                            .rsplit('.')
                            .next()
                            .unwrap_or(&f.ref_table)
                            .trim_matches(['"', '`'])
                            .to_string(),
                        ref_columns: f.ref_columns.clone(),
                        on_update: f.on_update.clone(),
                        on_delete: f.on_delete.clone(),
                    })
                    .collect(),
                indexes: st
                    .indexes
                    .iter()
                    .filter(|ix| !ix.primary && !ix.name.starts_with("sqlite_autoindex_"))
                    .map(|ix| MIndex { name: ix.name.clone(), columns: ix.columns.clone(), unique: ix.unique })
                    .collect(),
                x: (i % 4) as f32 * 260.0,
                y: (i / 4) as f32 * 260.0,
            });
        }
        m
    }

    fn require_physical(&self, what: &str) -> DbResult<()> {
        if self.kind == ModelKind::Physical {
            return Ok(());
        }
        Err(DbError::Invalid(format!(
            "this is a {} model; convert it to a physical model to {what}",
            self.kind.label().to_lowercase()
        )))
    }

    /// The model as the physical structures of `engine` (the shape `sql::sync` compares).
    pub fn structures(&self, engine: Engine, database: &str, schema: Option<&str>) -> Vec<(ObjectRef, TableStructure)> {
        self.tables
            .iter()
            .map(|t| {
                let obj = ObjectRef {
                    database: database.to_string(),
                    schema: schema.map(String::from),
                    name: t.name.clone(),
                    kind: ObjectKind::Table,
                };
                let st = TableStructure {
                    columns: t.columns.iter().map(|c| c.info(engine)).collect(),
                    indexes: t
                        .indexes
                        .iter()
                        .map(|ix| IndexInfo {
                            name: ix.name.clone(),
                            columns: ix.columns.clone(),
                            unique: ix.unique,
                            primary: false,
                        })
                        .collect(),
                    foreign_keys: t
                        .foreign_keys
                        .iter()
                        .map(|f| ForeignKeyInfo {
                            name: f.name.clone(),
                            columns: f.columns.clone(),
                            ref_table: f.ref_table.clone(),
                            ref_columns: f.ref_columns.clone(),
                            on_update: f.on_update.clone(),
                            on_delete: f.on_delete.clone(),
                        })
                        .collect(),
                };
                (obj, st)
            })
            .collect()
    }

    /// Model against a live schema: the statements that make the database match the model.
    /// Both sides go through the logical types, so only real differences produce statements.
    pub fn diff(
        &self,
        engine: Engine,
        database: &str,
        schema: Option<&str>,
        live: &[(ObjectRef, TableStructure)],
        include_drops: bool,
    ) -> DbResult<SyncPlan> {
        self.require_physical("compare it with a database")?;
        let problems = self.validate();
        if !problems.is_empty() {
            return Err(DbError::Invalid(format!("the model has problems:\n- {}", problems.join("\n- "))));
        }
        let mut current = DesignModel::reverse("live", live);
        // Servers echo defaults in their own spelling (`0.00`, `((0))`, `'x'::text`): an equivalent
        // default is not a difference.
        for t in &mut current.tables {
            let Some(mine) = self.tables.iter().find(|m| m.name == t.name) else { continue };
            for c in &mut t.columns {
                if let Some(m) = mine.columns.iter().find(|m| m.name == c.name)
                    && let (Some(a), Some(b)) = (&m.default, &c.default)
                    && same_default(a, b)
                {
                    c.default = m.default.clone();
                }
            }
        }
        let current = current.structures(engine, database, schema);
        structure_sync(engine, &self.structures(engine, database, schema), &current, database, schema, include_drops)
    }

    /// Forward engineering: DDL for `engine` in dependency order. `database` / `schema` place the tables.
    pub fn forward(&self, engine: Engine, database: &str, schema: Option<&str>) -> DbResult<Vec<String>> {
        self.require_physical("generate SQL")?;
        let problems = self.validate();
        if !problems.is_empty() {
            return Err(DbError::Invalid(format!("the model has problems:\n- {}", problems.join("\n- "))));
        }
        let d = Dialect(engine);
        let obj = |name: &str| ObjectRef {
            database: database.to_string(),
            schema: schema.map(String::from),
            name: name.to_string(),
            kind: ObjectKind::Table,
        };
        let deps: Vec<Vec<usize>> = self
            .tables
            .iter()
            .enumerate()
            .map(|(i, t)| {
                t.foreign_keys
                    .iter()
                    .filter_map(|f| self.tables.iter().position(|p| p.name.eq_ignore_ascii_case(&f.ref_table)))
                    .filter(|&p| p != i)
                    .collect()
            })
            .collect();
        let order = topo_order(&deps);
        let created_at: std::collections::HashMap<usize, usize> =
            order.iter().enumerate().map(|(pos, &i)| (i, pos)).collect();
        let mut statements = Vec::new();
        let mut late: Vec<String> = Vec::new();
        for &i in &order {
            let t = &self.tables[i];
            let cols: Vec<ColumnDef> = t.columns.iter().map(|c| ColumnDef::from(&c.info(engine))).collect();
            let table = obj(&t.name);
            // A foreign key whose parent does not exist yet (cycle) is added afterwards, except on SQLite.
            let mut inline = Vec::new();
            for f in &t.foreign_keys {
                let def = FkDef {
                    original: None,
                    name: f.name.clone(),
                    columns: f.columns.clone(),
                    ref_table: match (&schema, engine) {
                        (Some(s), Engine::Postgres | Engine::SqlServer | Engine::Oracle | Engine::Snowflake) => {
                            format!("{s}.{}", f.ref_table)
                        }
                        (_, Engine::MySql) if !database.is_empty() => format!("{database}.{}", f.ref_table),
                        _ => f.ref_table.clone(),
                    },
                    ref_columns: f.ref_columns.clone(),
                    on_update: f.on_update.clone(),
                    on_delete: f.on_delete.clone(),
                };
                let parent_pos = self.tables.iter().position(|p| p.name.eq_ignore_ascii_case(&f.ref_table));
                let parent_first = parent_pos.is_some_and(|p| created_at[&p] < created_at[&i]) || parent_pos == Some(i);
                if parent_first || engine == Engine::Sqlite {
                    inline.push(def);
                } else {
                    late.push(format!("ALTER TABLE {} ADD {}", d.qualified(&table)?, fk_clause(d, &def)?));
                }
            }
            statements.push(create_table_with(d, &table, &cols, &inline)?);
            for ix in &t.indexes {
                let def =
                    IndexDef { original: None, name: ix.name.clone(), columns: ix.columns.clone(), unique: ix.unique };
                statements.push(create_index(d, &table, &def)?);
            }
        }
        statements.extend(late);
        Ok(statements)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equivalent_defaults_are_recognised() {
        for (a, b) in [("0", "0.00"), ("0", "((0))"), ("'x'", "'x'::character varying"), ("'x'", "N'x'"), ("1", "'1'")]
        {
            assert!(same_default(a, b), "{a} vs {b}");
        }
        assert!(!same_default("0", "1") && !same_default("'a'", "'b'"));
    }

    #[test]
    fn data_vault_and_star_roles_follow_their_rules() {
        let customer = MTable::hub("customer", 0.0, 0.0);
        let product = MTable::hub("product", 300.0, 0.0);
        let link = MTable::link("purchase", &[&customer, &product], 150.0, 250.0);
        let sat = MTable::satellite("customer_details", &customer, 0.0, 250.0);
        assert_eq!(sat.columns.iter().filter(|c| c.primary_key).count(), 2, "parent key + load date");
        let mut m = DesignModel {
            name: "vault".into(),
            kind: ModelKind::Physical,
            tables: vec![customer.clone(), product, link, sat],
        };
        assert!(m.validate().is_empty(), "{:?}", m.validate());
        let ddl = m.forward(Engine::Postgres, "db", Some("public")).unwrap().join(";\n");
        assert!(ddl.contains("\"customer_hk\" char(32)"), "{ddl}");
        assert!(ddl.contains("REFERENCES \"public\".\"customer\""), "{ddl}");
        // A link to one hub, a satellite without a parent, a fact without a dimension.
        m.tables.push(MTable::link("lonely", &[&customer], 0.0, 0.0));
        let mut orphan = MTable::satellite("orphan", &customer, 0.0, 0.0);
        orphan.foreign_keys.clear();
        m.tables.push(orphan);
        let mut fact = MTable::new("sales", 0.0, 0.0);
        fact.role = TableRole::Fact;
        m.tables.push(fact);
        let problems = m.validate();
        for want in ["Link lonely", "Satellite orphan", "Fact sales"] {
            assert!(problems.iter().any(|p| p.starts_with(want)), "{want}: {problems:?}");
        }
        // Model files from before roles load as plain tables.
        let json = serde_json::to_string(&sample()).unwrap().replace(",\"role\":\"None\"", "");
        assert!(!json.contains("role"));
        assert!(serde_json::from_str::<DesignModel>(&json).unwrap().tables.iter().all(|t| t.role == TableRole::None));
    }

    #[test]
    fn only_physical_models_produce_sql_and_old_files_are_physical() {
        let mut m = sample();
        let json = serde_json::to_string(&m).unwrap().replace("\"kind\":\"Physical\",", "");
        assert!(!json.contains("kind"));
        assert_eq!(serde_json::from_str::<DesignModel>(&json).unwrap().kind, ModelKind::Physical);
        assert!(m.forward(Engine::Postgres, "db", Some("public")).is_ok());
        m.kind = ModelKind::Logical;
        let e = m.forward(Engine::Postgres, "db", Some("public")).unwrap_err().to_string();
        assert!(e.contains("logical model"), "{e}");
        assert!(m.diff(Engine::Postgres, "db", Some("public"), &[], false).is_err());
        // Converting back is lossless.
        m.kind = ModelKind::Physical;
        assert_eq!(m, sample());
    }

    fn sample() -> DesignModel {
        let mut orders = MTable::new("orders", 0.0, 0.0);
        orders.columns.push(MColumn {
            name: "customer_id".into(),
            ty: LType::Integer,
            nullable: false,
            primary_key: false,
            auto_increment: false,
            default: None,
        });
        orders.columns.push(MColumn {
            name: "total".into(),
            ty: LType::Decimal { precision: 10, scale: 2 },
            nullable: true,
            primary_key: false,
            auto_increment: false,
            default: Some("0".into()),
        });
        orders.foreign_keys.push(MFk {
            name: "fk_orders_customer".into(),
            columns: vec!["customer_id".into()],
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "CASCADE".into(),
        });
        orders.indexes.push(MIndex {
            name: "ix_orders_customer".into(),
            columns: vec!["customer_id".into()],
            unique: false,
        });
        let mut customers = MTable::new("customers", 300.0, 0.0);
        customers.columns.push(MColumn {
            name: "email".into(),
            ty: LType::Varchar(80),
            nullable: false,
            primary_key: false,
            auto_increment: false,
            default: None,
        });
        // Children first on purpose: forward engineering must reorder.
        DesignModel { name: "shop".into(), kind: ModelKind::Physical, tables: vec![orders, customers] }
    }

    #[test]
    fn forward_orders_parents_first_and_maps_types_per_engine() {
        let m = sample();
        for (engine, schema, ident) in [
            (Engine::Postgres, Some("public"), "GENERATED BY DEFAULT AS IDENTITY"),
            (Engine::MySql, None, "auto_increment"),
            (Engine::SqlServer, Some("dbo"), "IDENTITY(1,1)"),
            (Engine::Sqlite, None, "INTEGER PRIMARY KEY"),
        ] {
            let sql = m.forward(engine, "main", schema).unwrap();
            assert!(sql[0].contains("customers") && sql[1].contains("orders"), "{engine:?}: {sql:?}");
            assert!(sql[0].contains(ident), "{engine:?}: {}", sql[0]);
            assert!(sql[1].contains("FOREIGN KEY") && sql[1].contains("ON DELETE CASCADE"), "{engine:?}: {}", sql[1]);
            assert!(sql.last().unwrap().contains("ix_orders_customer"), "{engine:?}");
        }
        let pg = m.forward(Engine::Postgres, "d", Some("public")).unwrap();
        assert!(
            pg[0].contains("\"email\" varchar(80) NOT NULL") && pg[1].contains("decimal(10,2) DEFAULT 0"),
            "{pg:?}"
        );
        let ms = m.forward(Engine::SqlServer, "d", Some("dbo")).unwrap();
        assert!(ms[0].contains("[email] nvarchar(80) NOT NULL"), "{ms:?}");
    }

    #[test]
    fn cycles_add_the_foreign_key_afterwards_except_on_sqlite() {
        let mut m = sample();
        m.tables[1].columns.push(MColumn {
            name: "last_order".into(),
            ty: LType::Integer,
            nullable: true,
            primary_key: false,
            auto_increment: false,
            default: None,
        });
        m.tables[1].foreign_keys.push(MFk {
            name: "fk_last".into(),
            columns: vec!["last_order".into()],
            ref_table: "orders".into(),
            ref_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "SET NULL".into(),
        });
        let sql = m.forward(Engine::Postgres, "d", Some("public")).unwrap();
        assert_eq!(
            sql.iter().filter(|s| s.starts_with("ALTER TABLE") && s.contains("ADD CONSTRAINT")).count(),
            1,
            "{sql:?}"
        );
        let sqlite = m.forward(Engine::Sqlite, "main", None).unwrap();
        assert!(!sqlite.iter().any(|s| s.starts_with("ALTER TABLE")), "{sqlite:?}");
    }

    #[test]
    fn validation_reports_every_kind_of_problem() {
        let mut m = sample();
        assert!(m.validate().is_empty());
        m.tables[0].foreign_keys[0].ref_table = "nope".into();
        m.tables[1].columns[0].primary_key = false;
        m.tables[1].columns[0].auto_increment = true;
        m.tables.push(MTable::new("ORDERS", 0.0, 0.0));
        m.tables[0].indexes[0].columns = vec!["ghost".into()];
        let p = m.validate().join("\n");
        for needle in
            ["missing table nope", "auto-incremented but not a primary key", "appears twice", "missing column ghost"]
        {
            assert!(p.contains(needle), "{needle} not in:\n{p}");
        }
        assert!(m.forward(Engine::Postgres, "d", Some("public")).is_err());
    }

    #[test]
    fn types_round_trip_through_sql_text() {
        for t in LType::choices() {
            for engine in [Engine::Postgres, Engine::MySql, Engine::SqlServer, Engine::Sqlite] {
                let back = LType::from_sql(&t.sql(engine));
                let lossy = matches!(
                    (&t, engine),
                    (LType::SmallInt | LType::BigInt, Engine::Sqlite)
                        | (LType::Text, Engine::SqlServer)
                        | (LType::Json, Engine::SqlServer | Engine::Sqlite)
                        | (LType::Uuid, Engine::MySql | Engine::Sqlite)
                        | (LType::Boolean, Engine::Sqlite)
                        | (LType::Float | LType::Decimal { .. } | LType::Time, Engine::Sqlite)
                );
                if !lossy {
                    assert_eq!(back, t, "{engine:?}: {}", t.sql(engine));
                }
            }
        }
        assert_eq!(LType::from_sql("money"), LType::Native("money".into()));
        assert_eq!(LType::from_sql("character varying(30)"), LType::Varchar(30));
        assert_eq!(LType::from_sql("tinyint(1)"), LType::Boolean);
        assert_eq!(LType::from_sql("int(11)"), LType::Integer);
    }

    #[test]
    fn models_survive_json() {
        let m = sample();
        let back: DesignModel = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
    }
}
