//! User intents emitted by widgets and executed by `App` at the end of the frame.

use talus_core::model::ObjectRef;
use uuid::Uuid;

use crate::state::NavSel;

#[derive(Debug, Clone)]
pub enum Action {
    NewConnection,
    EditConnection(Uuid),
    DuplicateConnection(Uuid),
    AskDeleteConnection(Uuid),
    Open(Uuid),
    Close(Uuid),
    Refresh(NavSel),
    LoadSchemas {
        conn: Uuid,
        db: String,
    },
    LoadObjects {
        conn: Uuid,
        db: String,
        schema: Option<String>,
    },
    LoadStructure {
        conn: Uuid,
        obj: ObjectRef,
    },
    Select(NavSel),
    OpenData {
        conn: Uuid,
        obj: ObjectRef,
    },
    OpenStructure {
        conn: Uuid,
        obj: ObjectRef,
    },
    DesignTable {
        conn: Uuid,
        obj: ObjectRef,
    },
    NewTable {
        conn: Uuid,
        db: String,
        schema: Option<String>,
    },
    NewQuery {
        conn: Option<Uuid>,
        database: Option<String>,
        text: String,
    },
    Export {
        conn: Uuid,
        obj: ObjectRef,
    },
    Import {
        conn: Uuid,
        obj: ObjectRef,
    },
    AskDrop {
        conn: Uuid,
        obj: ObjectRef,
    },
    AskTruncate {
        conn: Uuid,
        obj: ObjectRef,
    },
    CopyText(String),
    OpenHistory,
    OpenFind,
    OpenSync,
    /// Column profile of a table.
    Profile {
        conn: Uuid,
        obj: ObjectRef,
    },
    /// Ask how many rows of test data to generate into a table.
    GenerateData {
        conn: Uuid,
        obj: ObjectRef,
    },
    /// Document the selected database/schema as HTML or Markdown.
    Dictionary {
        html: bool,
    },
    OpenJobs,
    OpenBuilder,
    OpenModel,
    OpenUsers(Uuid),
    /// Routines and triggers of the navigator-selected database/schema.
    OpenRoutines,
    /// Dump the navigator-selected database/schema to a .sql file.
    DumpSql {
        include_data: bool,
    },
    /// Pick a .sql file and run it against the selected database.
    RunSqlFile,
    OpenDiagram {
        conn: Uuid,
        db: String,
        schema: Option<String>,
    },
    OpenMonitor(Uuid),
    NewDatabase(Uuid),
    AskDropDatabase {
        conn: Uuid,
        db: String,
    },
    OpenSettings,
    OpenAbout,
    OpenFile,
    ActivateTab(u64),
    CloseTab(u64),
}
