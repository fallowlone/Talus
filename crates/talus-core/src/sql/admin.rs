//! Server administration SQL: database create/drop and the process monitor.

use super::dialect::Dialect;
use crate::error::{DbError, DbResult};
use crate::model::Engine;

pub fn create_database(engine: Engine, name: &str) -> DbResult<String> {
    match engine {
        Engine::Sqlite => {
            Err(DbError::Invalid("SQLite databases are files: create a new connection with \"New File…\"".into()))
        }
        Engine::Oracle => Err(DbError::Invalid(
            "an Oracle database is created by the administrator; create a schema with CREATE USER (Tools → Users)"
                .into(),
        )),
        Engine::Snowflake => Ok(format!("CREATE DATABASE {}", Dialect(engine).quote_ident(name.trim())?)),
        _ => Ok(format!("CREATE DATABASE {}", Dialect(engine).quote_ident(name.trim())?)),
    }
}

pub fn drop_database(engine: Engine, name: &str) -> DbResult<String> {
    match engine {
        Engine::Sqlite => Err(DbError::Invalid("SQLite databases are files; delete the file instead".into())),
        Engine::Oracle => Err(DbError::Invalid("drop an Oracle schema with DROP USER (Tools → Users)".into())),
        Engine::SqlServer => {
            // Open connections would make DROP fail; ROLLBACK IMMEDIATE disconnects them first.
            let q = Dialect(engine).quote_ident(name)?;
            Ok(format!("ALTER DATABASE {q} SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE {q}"))
        }
        Engine::Snowflake => Ok(format!("DROP DATABASE {}", Dialect(engine).quote_ident(name)?)),
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
        Engine::Oracle => Some(
            "SELECT s.sid || ',' || s.serial# AS \"id\", s.username AS \"user\", SYS_CONTEXT('USERENV', 'CON_NAME') AS \"database\", \
             s.machine AS \"client\", s.status AS \"state\", s.last_call_et AS \"seconds\", q.sql_text AS \"query\" \
             FROM v$session s LEFT JOIN v$sql q ON q.sql_id = s.sql_id AND q.child_number = 0 \
             WHERE s.type = 'USER' AND s.sid <> SYS_CONTEXT('USERENV', 'SID') ORDER BY s.last_call_et DESC",
        ),
        // The monitor shows running queries (Snowflake has no exposed session list); cancelling one
        // aborts the query, it does not end the session.
        Engine::Snowflake => Some(
            "SELECT QUERY_ID AS \"id\", USER_NAME AS \"user\", DATABASE_NAME AS \"database\", WAREHOUSE_NAME AS \"client\", \
             EXECUTION_STATUS AS \"state\", DATEDIFF(SECOND, START_TIME, CURRENT_TIMESTAMP()) AS \"seconds\", \
             QUERY_TEXT AS \"query\" FROM TABLE(INFORMATION_SCHEMA.QUERY_HISTORY()) \
             WHERE EXECUTION_STATUS = 'RUNNING' ORDER BY START_TIME",
        ),
        Engine::Sqlite | Engine::Redis | Engine::MongoDb => None,
    }
}

/// Cancels the running statement (`terminate == false`) or ends the session of process `id`.
pub fn kill(engine: Engine, id: &str, terminate: bool) -> DbResult<String> {
    if engine == Engine::Oracle {
        // Oracle sessions are identified by `sid,serial#`.
        let bad = || DbError::Invalid(format!("invalid session id: {id}"));
        let (sid, serial) = id.split_once(',').ok_or_else(bad)?;
        let (sid, serial): (u64, u64) =
            (sid.trim().parse().map_err(|_| bad())?, serial.trim().parse().map_err(|_| bad())?);
        return Ok(if terminate {
            format!("ALTER SYSTEM KILL SESSION '{sid},{serial}' IMMEDIATE")
        } else {
            format!("ALTER SYSTEM CANCEL SQL '{sid},{serial}'")
        });
    }
    if engine == Engine::Snowflake {
        // Query ids are opaque tokens; quote them as a string after a character check.
        let id = id.trim();
        if id.is_empty()
            || id.len() > 64
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        {
            return Err(DbError::Invalid(format!("invalid query id: {id}")));
        }
        if terminate {
            return Err(DbError::Invalid(
                "Snowflake cannot end another session from here; cancel its running queries instead".into(),
            ));
        }
        return Ok(format!("SELECT SYSTEM$CANCEL_QUERY('{id}')"));
    }
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
        (Engine::Oracle | Engine::Snowflake, _) => unreachable!("handled above"),
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
        assert_eq!(kill(Engine::Oracle, "51,7", true).unwrap(), "ALTER SYSTEM KILL SESSION '51,7' IMMEDIATE");
        assert_eq!(kill(Engine::Oracle, "51, 7", false).unwrap(), "ALTER SYSTEM CANCEL SQL '51,7'");
        assert!(kill(Engine::Oracle, "51;drop", true).is_err());
        assert!(kill(Engine::SqlServer, "51", false).is_err());
        assert!(drop_database(Engine::SqlServer, "d").unwrap().ends_with("DROP DATABASE [d]"));
        assert_eq!(kill(Engine::Postgres, "7", true).unwrap(), "SELECT pg_terminate_backend(7)");
        assert!(kill(Engine::Postgres, "1; drop table x", true).is_err());
        assert!(process_list(Engine::Sqlite).is_none());
    }
}
