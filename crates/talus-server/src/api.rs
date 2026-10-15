//! HTTP surface. Handlers only translate; authorization lives in `Store`.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::store::{ApiError, Role, Store};

type Shared = State<Arc<Store>>;
type Reply<T> = Result<Json<T>, ApiError>;

const MAX_BODY_BYTES: usize = 64 * 1024;

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({ "error": self.message });
        if let Some(current) = self.current {
            body["current"] = current;
        }
        (self.status, Json(body)).into_response()
    }
}

pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/v1/login", post(login))
        .route("/v1/logout", post(logout))
        .route("/v1/workspaces", get(list_workspaces).post(create_workspace))
        .route("/v1/workspaces/{ws}", get(workspace).delete(delete_workspace))
        .route("/v1/workspaces/{ws}/members/{user}", put(set_member).delete(remove_member))
        .route("/v1/workspaces/{ws}/connections", get(list_connections))
        .route("/v1/workspaces/{ws}/connections/{id}", put(put_connection).delete(delete_connection))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(store)
}

/// Serves the API on `listener` until the process ends.
pub async fn serve(listener: tokio::net::TcpListener, store: Arc<Store>) -> std::io::Result<()> {
    axum::serve(listener, router(store)).await
}

fn bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid token"))
}

fn who(store: &Store, headers: &HeaderMap) -> Result<String, ApiError> {
    store.authenticate(bearer(headers)?)
}

#[derive(Deserialize)]
struct Login {
    user: String,
    password: String,
}

async fn login(State(store): Shared, Json(body): Json<Login>) -> Reply<Value> {
    store.check_lockout(&body.user)?;
    let creds = store.credentials(&body.user);
    let password = body.password;
    // PBKDF2 is deliberately slow; keep it off the async workers.
    let ok = tokio::task::spawn_blocking(move || creds.check(&password))
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))?;
    if !ok {
        store.record_failure(&body.user);
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "invalid user name or password"));
    }
    store.clear_failures(&body.user);
    let token = store.issue_token(&body.user)?;
    Ok(Json(json!({ "token": token })))
}

async fn logout(State(store): Shared, headers: HeaderMap) -> Reply<Value> {
    store.logout(bearer(&headers)?)?;
    Ok(Json(json!({})))
}

async fn list_workspaces(State(store): Shared, headers: HeaderMap) -> Reply<Value> {
    let user = who(&store, &headers)?;
    Ok(Json(json!({ "workspaces": store.list_workspaces(&user)? })))
}

#[derive(Deserialize)]
struct NewWorkspace {
    name: String,
}

async fn create_workspace(State(store): Shared, headers: HeaderMap, Json(b): Json<NewWorkspace>) -> Reply<Value> {
    let user = who(&store, &headers)?;
    Ok(Json(json!(store.create_workspace(&user, &b.name)?)))
}

async fn workspace(State(store): Shared, headers: HeaderMap, Path(ws): Path<String>) -> Reply<Value> {
    let user = who(&store, &headers)?;
    Ok(Json(json!(store.workspace_info(&user, &ws)?)))
}

async fn delete_workspace(State(store): Shared, headers: HeaderMap, Path(ws): Path<String>) -> Reply<Value> {
    let user = who(&store, &headers)?;
    store.delete_workspace(&user, &ws)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct MemberRole {
    role: Role,
}

async fn set_member(
    State(store): Shared,
    headers: HeaderMap,
    Path((ws, member)): Path<(String, String)>,
    Json(b): Json<MemberRole>,
) -> Reply<Value> {
    let user = who(&store, &headers)?;
    store.set_member(&user, &ws, &member, b.role)?;
    Ok(Json(json!({})))
}

async fn remove_member(
    State(store): Shared,
    headers: HeaderMap,
    Path((ws, member)): Path<(String, String)>,
) -> Reply<Value> {
    let user = who(&store, &headers)?;
    store.remove_member(&user, &ws, &member)?;
    Ok(Json(json!({})))
}

async fn list_connections(State(store): Shared, headers: HeaderMap, Path(ws): Path<String>) -> Reply<Value> {
    let user = who(&store, &headers)?;
    Ok(Json(json!({ "connections": store.list_connections(&user, &ws)? })))
}

#[derive(Deserialize)]
struct PutConnection {
    base_rev: u64,
    data: Value,
}

async fn put_connection(
    State(store): Shared,
    headers: HeaderMap,
    Path((ws, id)): Path<(String, String)>,
    Json(b): Json<PutConnection>,
) -> Reply<Value> {
    let user = who(&store, &headers)?;
    let rev = store.put_connection(&user, &ws, &id, b.base_rev, b.data)?;
    Ok(Json(json!({ "rev": rev })))
}

#[derive(Deserialize)]
struct BaseRev {
    base_rev: u64,
}

async fn delete_connection(
    State(store): Shared,
    headers: HeaderMap,
    Path((ws, id)): Path<(String, String)>,
    Query(q): Query<BaseRev>,
) -> Reply<Value> {
    let user = who(&store, &headers)?;
    store.delete_connection(&user, &ws, &id, q.base_rev)?;
    Ok(Json(json!({})))
}
