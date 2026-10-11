//! Engine-specific SQL spelling: identifier quoting, qualification and bind placeholders.

use crate::error::{DbError, DbResult};
use crate::model::{Engine, ObjectRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialect(pub Engine);

impl Dialect {
    /// Quotes an identifier, doubling embedded quote characters. NUL is rejected because
    /// no engine accepts it and drivers may truncate at it.
    pub fn quote_ident(self, ident: &str) -> DbResult<String> {
        if ident.contains('\0') {
            return Err(DbError::Invalid("identifier contains NUL character".into()));
        }
        if ident.is_empty() {
            return Err(DbError::Invalid("identifier is empty".into()));
        }
        Ok(match self.0 {
            Engine::MySql => format!("`{}`", ident.replace('`', "``")),
            Engine::SqlServer => format!("[{}]", ident.replace(']', "]]")),
            Engine::Sqlite | Engine::Postgres | Engine::Redis | Engine::MongoDb => {
                format!("\"{}\"", ident.replace('"', "\"\""))
            }
        })
    }

    pub fn qualified(self, obj: &ObjectRef) -> DbResult<String> {
        let qualifier = match self.0 {
            Engine::Postgres | Engine::SqlServer => obj.schema.as_deref(),
            Engine::MySql | Engine::Sqlite | Engine::Redis | Engine::MongoDb => {
                Some(obj.database.as_str()).filter(|d| !d.is_empty())
            }
        };
        let name = self.quote_ident(&obj.name)?;
        Ok(match qualifier {
            Some(q) => format!("{}.{}", self.quote_ident(q)?, name),
            None => name,
        })
    }

    /// Bind placeholder for the 1-based parameter `n`. Values are always sent as text;
    /// PostgreSQL needs an explicit cast to the column type (`type_name` comes from the catalog).
    pub fn placeholder(self, n: usize, type_name: Option<&str>) -> String {
        match (self.0, type_name) {
            (Engine::Postgres, Some(t)) => format!("${n}::text::{t}"),
            (Engine::Postgres, None) => format!("${n}"),
            (Engine::MySql, _) => "?".into(),
            (Engine::Sqlite | Engine::Redis | Engine::MongoDb, _) => format!("?{n}"),
            (Engine::SqlServer, _) => format!("@P{n}"),
        }
    }

    /// SQL string literal (used only for catalog lookups of identifiers in generated DDL text,
    /// never for user data, which is always bound).
    pub fn string_literal(self, s: &str) -> String {
        let escaped = s.replace('\'', "''");
        match self.0 {
            Engine::MySql => format!("'{}'", escaped.replace('\\', "\\\\")),
            // N-prefix keeps non-ASCII text intact whatever the column code page is.
            Engine::SqlServer => format!("N'{escaped}'"),
            _ => format!("'{escaped}'"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ObjectKind;

    #[test]
    fn quoting_escapes_and_rejects() {
        assert_eq!(Dialect(Engine::Postgres).quote_ident("a\"b").unwrap(), "\"a\"\"b\"");
        assert_eq!(Dialect(Engine::MySql).quote_ident("a`b").unwrap(), "`a``b`");
        assert_eq!(Dialect(Engine::SqlServer).quote_ident("a]b").unwrap(), "[a]]b]");
        assert!(Dialect(Engine::Sqlite).quote_ident("a\0").is_err());
        assert!(Dialect(Engine::Sqlite).quote_ident("").is_err());
    }

    #[test]
    fn qualification_per_engine() {
        let o =
            ObjectRef { database: "db".into(), schema: Some("s".into()), name: "t".into(), kind: ObjectKind::Table };
        assert_eq!(Dialect(Engine::Postgres).qualified(&o).unwrap(), "\"s\".\"t\"");
        assert_eq!(Dialect(Engine::MySql).qualified(&o).unwrap(), "`db`.`t`");
        assert_eq!(Dialect(Engine::Sqlite).qualified(&o).unwrap(), "\"db\".\"t\"");
        assert_eq!(Dialect(Engine::SqlServer).qualified(&o).unwrap(), "[s].[t]");
    }

    #[test]
    fn literals() {
        assert_eq!(Dialect(Engine::MySql).string_literal("a'b\\"), "'a''b\\\\'");
        assert_eq!(Dialect(Engine::Postgres).string_literal("a'b\\"), "'a''b\\'");
        assert_eq!(Dialect(Engine::SqlServer).string_literal("o'k"), "N'o''k'");
    }
}
