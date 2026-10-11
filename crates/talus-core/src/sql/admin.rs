//! Server administration SQL: database create/drop and the process monitor.

use super::dialect::Dialect;
use crate::error::{DbError, DbResult};
use crate::model::Engine;

pub fn create_database(engine: Engine, name: &str) -> DbResult<String> {
    match engine {
        Engine::Sqlite => {
            Err(DbError::Invalid("SQLite databases are files: create a new connection with \"New File…\"".into()))
        }
        _ => Ok(format!("CREATE DATABASE {}", Dialect(engine).quote_ident(name.trim())?)),
    }
}

pub fn drop_database(engine: Engine, name: &str) -> DbResult<String> {
    match engine {
        Engine::Sqlite => Err(DbError::Invalid("SQLite databases are files; delete the file instead".into())),
        Engine::SqlServer => {
            // Open connections would make DROP fail; ROLLBACK IMMEDIATE disconnects them first.
            let q = Dialect(engine).quote_ident(name)?;
            Ok(format!("ALTER DATABASE {q} SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE {q}"))
        }
        _ => Ok(format!("DROP DATABASE {}", Dialect(engine).quote_ident(name)?)),
    }
}

/// Process list query: columns id, user, database, client, state, seconds, query.
pub fn process_list(engine: Engine) -> Option<&'static str> {
    match engine {
        Engine::Postgres => Some(
            "SELECT pid AS id, usename AS user, datname AS database, client_addr::text AS client, state, \
             COALESCE(EXTRACT(EPOCH FROM now() - query_start)::bigint, 0) AS seconds, query \
             FROM pg_stat_activity WHERE pid <> pg_backend_pid() AND backend_type = 'client backend' ORDER BY seconds DESC",
        ),
        Engine::MySql => Some(
            "SELECT ID AS id, USER AS user, DB AS `database`, HOST AS client, COMMAND AS state, TIME AS seconds, INFO AS query \
             FROM information_schema.PROCESSLIST WHERE ID <> CONNECTION_ID() ORDER BY TIME DESC",
        ),
        Engine::SqlServer => Some(
            "SELECT s.session_id AS id, s.login_name AS [user], DB_NAME(s.database_id) AS [database], s.host_name AS client, \
             ISNULL(r.status, s.status) AS state, ISNULL(DATEDIFF(SECOND, r.start_time, GETDATE()), 0) AS seconds, t.text AS query \
             FROM sys.dm_exec_sessions s LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id \
             OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t \
             WHERE s.is_user_process = 1 AND s.session_id <> @@SPID ORDER BY seconds DESC",
        ),
        Engine::Sqlite | Engine::Redis | Engine::MongoDb => None,
    }
}

/// Cancels the running statement (`terminate == false`) or ends the session of process `id`.
pub fn kill(engine: Engine, id: &str, terminate: bool) -> DbResult<String> {
    let id: u64 = id.trim().parse().map_err(|_| DbError::Invalid(format!("invalid process id: {id}")))?;
    match (engine, terminate) {
        (Engine::Postgres, false) => Ok(format!("SELECT pg_cancel_backend({id})")),
        (Engine::Postgres, true) => Ok(format!("SELECT pg_terminate_backend({id})")),
        (Engine::MySql, false) => Ok(format!("KILL QUERY {id}")),
        (Engine::MySql, true) => Ok(format!("KILL {id}")),
        (Engine::SqlServer, false) => Err(DbError::Invalid(
            "SQL Server cannot cancel one statement of another session; end the session instead".into(),
        )),
        (Engine::SqlServer, true) => Ok(format!("KILL {id}")),
        (Engine::Sqlite | Engine::Redis | Engine::MongoDb, _) => {
            Err(DbError::Invalid("SQLite has no server processes".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_sql() {
        assert_eq!(create_database(Engine::Postgres, " sales ").unwrap(), "CREATE DATABASE \"sales\"");
        assert_eq!(drop_database(Engine::MySql, "a`b").unwrap(), "DROP DATABASE `a``b`");
        assert!(create_database(Engine::Sqlite, "x").is_err());
        assert!(create_database(Engine::Postgres, "  ").is_err());
        assert_eq!(kill(Engine::MySql, "42", false).unwrap(), "KILL QUERY 42");
        assert_eq!(kill(Engine::SqlServer, "51", true).unwrap(), "KILL 51");
        assert!(kill(Engine::SqlServer, "51", false).is_err());
        assert!(drop_database(Engine::SqlServer, "d").unwrap().ends_with("DROP DATABASE [d]"));
        assert_eq!(kill(Engine::Postgres, "7", true).unwrap(), "SELECT pg_terminate_backend(7)");
        assert!(kill(Engine::Postgres, "1; drop table x", true).is_err());
        assert!(process_list(Engine::Sqlite).is_none());
    }
}
