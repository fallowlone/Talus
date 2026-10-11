//! UI-side state: navigation tree (lazily loaded) and selection.

use talus_core::model::{ObjectKind, ObjectRef, TableStructure};
use uuid::Uuid;

use crate::worker::JobId;

#[derive(Debug, Clone, Default)]
pub enum Loadable<T> {
    #[default]
    NotLoaded,
    Loading(JobId),
    Loaded(T),
    Failed(String),
}

impl<T> Loadable<T> {
    pub fn loaded(&self) -> Option<&T> {
        match self {
            Loadable::Loaded(v) => Some(v),
            _ => None,
        }
    }
    pub fn loaded_mut(&mut self) -> Option<&mut T> {
        match self {
            Loadable::Loaded(v) => Some(v),
            _ => None,
        }
    }
    pub fn is_loading(&self) -> bool {
        matches!(self, Loadable::Loading(_))
    }
    pub fn needs_load(&self) -> bool {
        matches!(self, Loadable::NotLoaded)
    }
}

#[derive(Debug, Clone, Default)]
pub enum ConnStatus {
    #[default]
    Closed,
    Connecting(JobId),
    Open {
        version: String,
    },
    Failed(String),
}

impl ConnStatus {
    pub fn is_open(&self) -> bool {
        matches!(self, ConnStatus::Open { .. })
    }
}

#[derive(Debug, Clone)]
pub struct ObjNode {
    pub obj: ObjectRef,
    pub expanded: bool,
    pub structure: Loadable<TableStructure>,
}

impl ObjNode {
    pub fn new(obj: ObjectRef) -> Self {
        Self { obj, expanded: false, structure: Loadable::NotLoaded }
    }
}

#[derive(Debug, Clone)]
pub struct SchemaNode {
    pub name: String,
    pub expanded: bool,
    pub objects: Loadable<Vec<ObjNode>>,
}

#[derive(Debug, Clone)]
pub struct DbNode {
    pub name: String,
    pub expanded: bool,
    pub schemas: Loadable<Vec<SchemaNode>>,
    pub objects: Loadable<Vec<ObjNode>>,
}

impl DbNode {
    pub fn new(name: String) -> Self {
        Self { name, expanded: false, schemas: Loadable::NotLoaded, objects: Loadable::NotLoaded }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConnState {
    pub status: ConnStatus,
    pub expanded: bool,
    pub databases: Vec<DbNode>,
}

impl ConnState {
    pub fn db_mut(&mut self, name: &str) -> Option<&mut DbNode> {
        self.databases.iter_mut().find(|d| d.name == name)
    }

    /// Object list for a database (MySQL/SQLite) or a schema (PostgreSQL).
    pub fn objects_mut(&mut self, db: &str, schema: Option<&str>) -> Option<&mut Loadable<Vec<ObjNode>>> {
        let d = self.db_mut(db)?;
        match schema {
            None => Some(&mut d.objects),
            Some(s) => d.schemas.loaded_mut()?.iter_mut().find(|x| x.name == s).map(|x| &mut x.objects),
        }
    }

    pub fn objects(&self, db: &str, schema: Option<&str>) -> Option<&Loadable<Vec<ObjNode>>> {
        let d = self.databases.iter().find(|d| d.name == db)?;
        match schema {
            None => Some(&d.objects),
            Some(s) => d.schemas.loaded()?.iter().find(|x| x.name == s).map(|x| &x.objects),
        }
    }

    pub fn obj_node_mut(&mut self, obj: &ObjectRef) -> Option<&mut ObjNode> {
        self.objects_mut(&obj.database, obj.schema.as_deref())?.loaded_mut()?.iter_mut().find(|n| n.obj == *obj)
    }

    pub fn structure(&self, obj: &ObjectRef) -> Option<&TableStructure> {
        self.objects(&obj.database, obj.schema.as_deref())?.loaded()?.iter().find(|n| n.obj == *obj)?.structure.loaded()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavSel {
    Conn(Uuid),
    Db(Uuid, String),
    Schema(Uuid, String, String),
    Obj(Uuid, ObjectRef),
}

impl NavSel {
    pub fn conn(&self) -> Uuid {
        match self {
            NavSel::Conn(c) | NavSel::Db(c, _) | NavSel::Schema(c, _, _) | NavSel::Obj(c, _) => *c,
        }
    }
    pub fn database(&self) -> Option<&str> {
        match self {
            NavSel::Conn(_) => None,
            NavSel::Db(_, d) | NavSel::Schema(_, d, _) => Some(d),
            NavSel::Obj(_, o) => Some(&o.database),
        }
    }
    /// (database, schema) whose object list the Objects tab should show.
    pub fn object_scope(&self) -> Option<(String, Option<String>)> {
        match self {
            NavSel::Conn(_) => None,
            NavSel::Db(_, d) => Some((d.clone(), None)),
            NavSel::Schema(_, d, s) => Some((d.clone(), Some(s.clone()))),
            NavSel::Obj(_, o) => Some((o.database.clone(), o.schema.clone())),
        }
    }
}

pub fn kind_icon(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => egui_phosphor::regular::TABLE,
        ObjectKind::View => egui_phosphor::regular::EYE,
    }
}
