//! Server accounts: list, grants, create and drop (PostgreSQL roles, MySQL users).
//! SQL that carries a password comes with a redacted twin for display and logs.

use super::dialect::Dialect;
use crate::engine::{ScriptOptions, Session};
use crate::error::{DbError, DbResult};
use crate::model::{Engine, Outcome};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub name: String,
    /// MySQL account host part.
    pub host: Option<String>,
    /// Human-readable flags: `superuser`, `login`, `locked`, …
    pub attrs: Vec<String>,
}

impl UserInfo {
    pub fn display(&self) -> String {
        match &self.host {
            Some(h) => format!("{}@{h}", self.name),
            None => self.name.clone(),
        }
    }
}

async fn rows(s: &mut Session, sql: &str) -> DbResult<Vec<Vec<Option<String>>>> {
    let out = s.run_script(sql, &ScriptOptions::default()).await;
    if let Some(e) = out.errors.first() {
        return Err(DbError::Invalid(e.error.to_string()));
    }
    match out.results.into_iter().next().map(|r| r.outcome) {
        Some(Outcome::Rows(q)) => Ok(q.rows),
        _ => Ok(Vec::new()),
    }
}

fn cell(r: &[Option<String>], i: usize) -> String {
    r.get(i).cloned().flatten().unwrap_or_default()
}

fn unsupported() -> DbError {
    DbError::Invalid("this engine has no SQL user accounts".into())
}

pub async fn list_users(s: &mut Session) -> DbResult<Vec<UserInfo>> {
    match s.engine() {
        Engine::Sqlite | Engine::Redis | Engine::MongoDb => Err(unsupported()),
        Engine::Postgres => {
            let sql = "SELECT rolname, rolsuper::text, rolcreatedb::text, rolcreaterole::text, rolcanlogin::text, rolconnlimit::text \
                       FROM pg_roles WHERE rolname !~ '^pg_' ORDER BY rolname";
            Ok(rows(s, sql)
                .await?
                .iter()
                .map(|r| {
                    let flag = |i: usize, name: &str| (cell(r, i) == "true").then(|| name.to_string());
                    let mut attrs: Vec<String> =
                        [flag(1, "superuser"), flag(2, "createdb"), flag(3, "createrole"), flag(4, "login")]
                            .into_iter()
                            .flatten()
                            .collect();
                    if cell(r, 5) != "-1" {
                        attrs.push(format!("connlimit {}", cell(r, 5)));
                    }
                    UserInfo { name: cell(r, 0), host: None, attrs }
                })
                .collect())
        }
        Engine::SqlServer => {
            let sql = "SELECT name, is_disabled, type_desc FROM sys.server_principals \
                       WHERE type IN ('S', 'U', 'G') AND name NOT LIKE '##%' ORDER BY name";
            Ok(rows(s, sql)
                .await?
                .iter()
                .map(|r| {
                    let mut attrs = vec![cell(r, 2).to_lowercase().replace('_', " ")];
                    if matches!(cell(r, 1).as_str(), "1" | "true") {
                        attrs.push("disabled".to_string());
                    }
                    UserInfo { name: cell(r, 0), host: None, attrs }
                })
                .collect())
        }
        Engine::MySql => {
            let sql = "SELECT User, Host, account_locked, password_expired FROM mysql.user ORDER BY User, Host";
            Ok(rows(s, sql)
                .await?
                .iter()
                .map(|r| {
                    let mut attrs = Vec::new();
                    if cell(r, 2) == "Y" {
                        attrs.push("locked".to_string());
                    }
                    if cell(r, 3) == "Y" {
                        attrs.push("password expired".to_string());
                    }
                    UserInfo { name: cell(r, 0), host: Some(cell(r, 1)), attrs }
                })
                .collect())
        }
    }
}

/// GRANT statements that describe what the account may do.
pub async fn grants(s: &mut Session, u: &UserInfo) -> DbResult<Vec<String>> {
    let d = Dialect(s.engine());
    match s.engine() {
        Engine::Sqlite | Engine::Redis | Engine::MongoDb => Err(unsupported()),
        Engine::MySql => {
            let host = u.host.as_deref().unwrap_or("%");
            let sql = format!("SHOW GRANTS FOR {}@{}", d.string_literal(&u.name), d.string_literal(host));
            Ok(rows(s, &sql).await?.iter().map(|r| cell(r, 0)).collect())
        }
        Engine::SqlServer => {
            let n = d.string_literal(&u.name);
            let mut out = Vec::new();
            for sql in [
                format!(
                    "SELECT 'ALTER SERVER ROLE ' + QUOTENAME(r.name COLLATE DATABASE_DEFAULT) + ' ADD MEMBER ' + QUOTENAME(m.name COLLATE DATABASE_DEFAULT) \
                     FROM sys.server_role_members rm JOIN sys.server_principals r ON r.principal_id = rm.role_principal_id \
                     JOIN sys.server_principals m ON m.principal_id = rm.member_principal_id WHERE m.name = {n} ORDER BY 1"
                ),
                format!(
                    "SELECT REPLACE(p.state_desc COLLATE DATABASE_DEFAULT, '_', ' ') + ' ' + (p.permission_name COLLATE DATABASE_DEFAULT) + ' TO ' + QUOTENAME(sp.name COLLATE DATABASE_DEFAULT) \
                     FROM sys.server_permissions p JOIN sys.server_principals sp ON sp.principal_id = p.grantee_principal_id \
                     WHERE sp.name = {n} ORDER BY 1"
                ),
                format!(
                    "SELECT REPLACE(dp.state_desc COLLATE DATABASE_DEFAULT, '_', ' ') + ' ' + (dp.permission_name COLLATE DATABASE_DEFAULT) + ' ON ' + \
                       CASE dp.class WHEN 1 THEN QUOTENAME(OBJECT_SCHEMA_NAME(dp.major_id)) + '.' + QUOTENAME(OBJECT_NAME(dp.major_id)) \
                                     WHEN 3 THEN 'SCHEMA::' + QUOTENAME(SCHEMA_NAME(dp.major_id)) ELSE 'DATABASE::' + QUOTENAME(DB_NAME()) END \
                       + ' TO ' + QUOTENAME(u.name) \
                     FROM sys.database_permissions dp JOIN sys.database_principals u ON u.principal_id = dp.grantee_principal_id \
                     WHERE u.sid = SUSER_SID({n}) ORDER BY 1"
                ),
            ] {
                out.extend(rows(s, &sql).await?.iter().map(|r| cell(r, 0)));
            }
            Ok(out)
        }
        Engine::Postgres => {
            let n = d.string_literal(&u.name);
            let tables = format!(
                "SELECT 'GRANT ' || string_agg(privilege_type, ', ' ORDER BY privilege_type) || ' ON ' || table_schema || '.' || table_name || ' TO ' || grantee \
                 FROM information_schema.role_table_grants WHERE grantee = {n} GROUP BY table_schema, table_name, grantee ORDER BY table_schema, table_name"
            );
            let members = format!(
                "SELECT 'GRANT ' || r.rolname || ' TO ' || m.rolname FROM pg_auth_members am \
                 JOIN pg_roles r ON r.oid = am.roleid JOIN pg_roles m ON m.oid = am.member WHERE m.rolname = {n} ORDER BY 1"
            );
            let mut out: Vec<String> = rows(s, &members).await?.iter().map(|r| cell(r, 0)).collect();
            out.extend(rows(s, &tables).await?.iter().map(|r| cell(r, 0)));
            Ok(out)
        }
    }
}

/// `(sql to run, sql safe to show)`; the second hides the password.
pub fn create_user_sql(engine: Engine, name: &str, host: &str, password: &str) -> DbResult<(String, String)> {
    let name = name.trim();
    if name.is_empty() {
        return Err(DbError::Invalid("user name is required".into()));
    }
    if password.is_empty() {
        return Err(DbError::Invalid("a password is required".into()));
    }
    let d = Dialect(engine);
    let build = |pw: &str| -> DbResult<String> {
        Ok(match engine {
            Engine::Sqlite | Engine::Redis | Engine::MongoDb => return Err(unsupported()),
            Engine::Postgres => format!("CREATE ROLE {} LOGIN PASSWORD {pw}", d.quote_ident(name)?),
            Engine::SqlServer => format!("CREATE LOGIN {} WITH PASSWORD = {pw}", d.quote_ident(name)?),
            Engine::MySql => {
                let host = Some(host.trim()).filter(|h| !h.is_empty()).unwrap_or("%");
                format!("CREATE USER {}@{} IDENTIFIED BY {pw}", d.string_literal(name), d.string_literal(host))
            }
        })
    };
    Ok((build(&d.string_literal(password))?, build("'••••••••'")?))
}

/// What a privilege applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantTarget {
    Table(crate::model::ObjectRef),
    /// PostgreSQL / SQL Server schema (MySQL: the database itself).
    Schema(String),
    /// The whole database (PostgreSQL / SQL Server database-level rights).
    Database(String),
}

/// Privileges the editor offers for a target, per engine. Anything else is refused.
pub fn privileges(engine: Engine, target: &GrantTarget) -> &'static [&'static str] {
    match (engine, target) {
        (Engine::Postgres, GrantTarget::Table(_)) => {
            &["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"]
        }
        (Engine::Postgres, GrantTarget::Schema(_)) => &["USAGE", "CREATE"],
        (Engine::Postgres, GrantTarget::Database(_)) => &["CONNECT", "CREATE", "TEMPORARY"],
        (Engine::MySql, GrantTarget::Table(_)) => {
            &["SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "INDEX", "ALTER", "DROP", "TRIGGER"]
        }
        (Engine::MySql, _) => &[
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "CREATE",
            "DROP",
            "ALTER",
            "INDEX",
            "REFERENCES",
            "TRIGGER",
            "CREATE VIEW",
            "SHOW VIEW",
            "EXECUTE",
            "EVENT",
            "LOCK TABLES",
        ],
        (Engine::SqlServer, GrantTarget::Table(_)) => {
            &["SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "ALTER", "CONTROL"]
        }
        (Engine::SqlServer, GrantTarget::Schema(_)) => {
            &["SELECT", "INSERT", "UPDATE", "DELETE", "EXECUTE", "ALTER", "CONTROL"]
        }
        (Engine::SqlServer, GrantTarget::Database(_)) => &[
            "CONNECT",
            "CREATE TABLE",
            "CREATE VIEW",
            "CREATE PROCEDURE",
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "EXECUTE",
            "CONTROL",
        ],
        _ => &[],
    }
}

/// `GRANT … ON … TO user` / `REVOKE … ON … FROM user`. Privileges are checked against `privileges`.
pub fn grant_sql(
    engine: Engine,
    user: &UserInfo,
    privs: &[String],
    target: &GrantTarget,
    grant: bool,
) -> DbResult<String> {
    let allowed = privileges(engine, target);
    if allowed.is_empty() {
        return Err(unsupported());
    }
    if privs.is_empty() {
        return Err(DbError::Invalid("choose at least one privilege".into()));
    }
    if let Some(bad) = privs.iter().find(|p| !allowed.contains(&p.as_str())) {
        return Err(DbError::Invalid(format!("{bad:?} is not a privilege that can be set here")));
    }
    let d = Dialect(engine);
    let on = match (engine, target) {
        (Engine::Postgres, GrantTarget::Table(o)) => format!("TABLE {}", d.qualified(o)?),
        (Engine::Postgres, GrantTarget::Schema(s)) => format!("SCHEMA {}", d.quote_ident(s)?),
        (Engine::Postgres, GrantTarget::Database(db)) => format!("DATABASE {}", d.quote_ident(db)?),
        (Engine::MySql, GrantTarget::Table(o)) => d.qualified(o)?,
        (Engine::MySql, GrantTarget::Schema(s) | GrantTarget::Database(s)) => format!("{}.*", d.quote_ident(s)?),
        (Engine::SqlServer, GrantTarget::Table(o)) => d.qualified(o)?,
        (Engine::SqlServer, GrantTarget::Schema(s)) => format!("SCHEMA::{}", d.quote_ident(s)?),
        (Engine::SqlServer, GrantTarget::Database(_)) => String::new(),
        _ => return Err(unsupported()),
    };
    let who = match engine {
        Engine::Postgres => d.quote_ident(&user.name)?,
        Engine::SqlServer => d.quote_ident(&user.name)?,
        _ => format!("{}@{}", d.string_literal(&user.name), d.string_literal(user.host.as_deref().unwrap_or("%"))),
    };
    let list = privs.join(", ");
    let on = if on.is_empty() { String::new() } else { format!(" ON {on}") };
    Ok(if grant { format!("GRANT {list}{on} TO {who}") } else { format!("REVOKE {list}{on} FROM {who}") })
}

pub fn drop_user_sql(engine: Engine, u: &UserInfo) -> DbResult<String> {
    let d = Dialect(engine);
    match engine {
        Engine::Sqlite | Engine::Redis | Engine::MongoDb => Err(unsupported()),
        Engine::Postgres => Ok(format!("DROP ROLE {}", d.quote_ident(&u.name)?)),
        Engine::SqlServer => Ok(format!("DROP LOGIN {}", d.quote_ident(&u.name)?)),
        Engine::MySql => Ok(format!(
            "DROP USER {}@{}",
            d.string_literal(&u.name),
            d.string_literal(u.host.as_deref().unwrap_or("%"))
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_sql_quotes_and_redacts() {
        let (real, shown) = create_user_sql(Engine::Postgres, "app\"x", "", "p'w").unwrap();
        assert_eq!(real, "CREATE ROLE \"app\"\"x\" LOGIN PASSWORD 'p''w'");
        assert!(!shown.contains("p'w") && shown.contains("••••"));
        let (real, _) = create_user_sql(Engine::MySql, "bob", "10.%", "x\\y").unwrap();
        assert_eq!(real, "CREATE USER 'bob'@'10.%' IDENTIFIED BY 'x\\\\y'");
        let (real, _) = create_user_sql(Engine::MySql, "bob", " ", "pw").unwrap();
        assert!(real.contains("'bob'@'%'"));
        assert!(create_user_sql(Engine::Postgres, " ", "", "pw").is_err());
        assert!(create_user_sql(Engine::Postgres, "a", "", "").is_err());
        assert!(create_user_sql(Engine::Sqlite, "a", "", "pw").is_err());
    }

    #[test]
    fn grant_sql_is_quoted_and_whitelisted() {
        let pg = UserInfo { name: "a\"b".into(), host: None, attrs: vec![] };
        let t = crate::model::ObjectRef {
            database: "d".into(),
            schema: Some("s".into()),
            name: "t".into(),
            kind: crate::model::ObjectKind::Table,
        };
        let sel = vec!["SELECT".to_string(), "INSERT".to_string()];
        assert_eq!(
            grant_sql(Engine::Postgres, &pg, &sel, &GrantTarget::Table(t.clone()), true).unwrap(),
            "GRANT SELECT, INSERT ON TABLE \"s\".\"t\" TO \"a\"\"b\""
        );
        assert_eq!(
            grant_sql(Engine::Postgres, &pg, &["USAGE".into()], &GrantTarget::Schema("s".into()), false).unwrap(),
            "REVOKE USAGE ON SCHEMA \"s\" FROM \"a\"\"b\""
        );
        let my = UserInfo { name: "bo'b".into(), host: Some("%".into()), attrs: vec![] };
        assert_eq!(
            grant_sql(Engine::MySql, &my, &sel, &GrantTarget::Database("d".into()), true).unwrap(),
            "GRANT SELECT, INSERT ON `d`.* TO 'bo''b'@'%'"
        );
        let ms = UserInfo { name: "u".into(), host: None, attrs: vec![] };
        assert_eq!(
            grant_sql(Engine::SqlServer, &ms, &["CONNECT".into()], &GrantTarget::Database("d".into()), true).unwrap(),
            "GRANT CONNECT TO [u]"
        );
        assert!(
            grant_sql(Engine::Postgres, &pg, &["SELECT; DROP TABLE x".into()], &GrantTarget::Table(t.clone()), true)
                .is_err()
        );
        assert!(grant_sql(Engine::Postgres, &pg, &[], &GrantTarget::Table(t.clone()), true).is_err());
        assert!(grant_sql(Engine::Sqlite, &pg, &sel, &GrantTarget::Table(t), true).is_err());
    }

    #[test]
    fn drop_sql() {
        let u = UserInfo { name: "bo'b".into(), host: Some("%".into()), attrs: vec![] };
        assert_eq!(drop_user_sql(Engine::MySql, &u).unwrap(), "DROP USER 'bo''b'@'%'");
        let r = UserInfo { name: "r".into(), host: None, attrs: vec![] };
        assert_eq!(drop_user_sql(Engine::Postgres, &r).unwrap(), "DROP ROLE \"r\"");
        assert_eq!(r.display(), "r");
        assert_eq!(u.display(), "bo'b@%");
    }
}
