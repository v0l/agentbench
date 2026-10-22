use crate::projects;
use crate::runner::Runner;
use crate::store::StoreError;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use foreman_core::{
    Answer, ApiError, Id, IssuePatch, IssueState, NewComment, NewIssue, NewProject, NewSession,
    NewWorktree, Overview, Place, SendMessage, branch_slug, issue_prompt,
};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

const LONG_POLL: Duration = Duration::from_secs(25);
const TITLE_MAX: usize = 40;

#[derive(Clone)]
pub struct Api {
    pub runner: Runner,
    pub token: String,
    pub model: Option<crate::agent::native::Shared>,
}

pub struct Failure(StatusCode, String);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { error: self.1 })).into_response()
    }
}

impl From<StoreError> for Failure {
    fn from(e: StoreError) -> Self {
        let status = if e.0.ends_with("not found") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_REQUEST
        };
        Failure(status, e.0)
    }
}

fn bad(text: impl Into<String>) -> Failure {
    Failure(StatusCode::BAD_REQUEST, text.into())
}

type Reply<T> = Result<Json<T>, Failure>;

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn auth(State(api): State<Api>, headers: HeaderMap, req: Request, next: Next) -> Response {
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !constant_eq(given.trim().as_bytes(), api.token.as_bytes()) {
        return Failure(StatusCode::UNAUTHORIZED, "missing or wrong token".into()).into_response();
    }
    next.run(req).await
}

pub fn router(api: Api) -> Router {
    let protected = Router::new()
        .route("/api/overview", get(overview))
        .route("/api/changes", get(changes))
        .route("/api/agents", get(agents))
        .route("/api/settings/model", get(get_model).put(put_model))
        .route("/api/settings/models", post(list_models))
        .route("/api/projects", get(list_projects).post(add_project))
        .route("/api/projects/{id}", delete(delete_project))
        .route("/api/projects/{id}/sync", post(sync_project))
        .route("/api/projects/{id}/worktrees", get(list_worktrees).post(add_worktree))
        .route("/api/worktrees/{id}", delete(remove_worktree))
        .route("/api/issues", get(issues).post(add_issue))
        .route("/api/issues/{id}", get(issue).patch(update_issue).delete(delete_issue))
        .route("/api/issues/{id}/comments", get(comments).post(add_comment))
        .route("/api/issues/{id}/start", post(start_issue))
        .route("/api/sessions", get(sessions).post(add_session))
        .route("/api/sessions/{id}", get(session).delete(delete_session))
        .route("/api/sessions/{id}/messages", get(messages).post(send))
        .route("/api/sessions/{id}/interrupt", post(interrupt))
        .route("/api/sessions/{id}/answer", post(answer))
        .layer(middleware::from_fn_with_state(api.clone(), auth));
    Router::new()
        .route("/health", get(|| async { Json(json!({ "ok": true })) }))
        .merge(protected)
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(api)
}

fn changed(api: &Api) {
    api.runner.changed.notify_waiters();
}

async fn overview(State(api): State<Api>) -> Reply<Overview> {
    let store = api.runner.store();
    Ok(Json(Overview {
        version: env!("CARGO_PKG_VERSION").to_string(),
        agents: api.runner.agents().iter().map(|a| a.info()).collect(),
        projects: store.projects()?,
        worktrees: store.worktrees(None)?,
        issues: store.issues()?,
        sessions: store.sessions()?,
        cursor: store.cursor()?,
    }))
}

#[derive(Deserialize)]
struct Since {
    #[serde(default)]
    since: Id,
    #[serde(default)]
    wait: Option<u64>,
}

async fn changes(State(api): State<Api>, Query(q): Query<Since>) -> Reply<serde_json::Value> {
    let store = api.runner.store();
    let wait = Duration::from_secs(q.wait.unwrap_or(0)).min(LONG_POLL);
    if !wait.is_zero() && store.cursor()? <= q.since {
        let notified = api.runner.changed.notified();
        if store.cursor()? <= q.since {
            let _ = tokio::time::timeout(wait, notified).await;
        }
    }
    let changes = store.changes_since(q.since)?;
    let cursor = changes.last().map(|c| c.0).unwrap_or(q.since);
    let changes: Vec<_> = changes
        .into_iter()
        .map(|(id, kind, ref_id)| json!({ "id": id, "kind": kind, "ref": ref_id }))
        .collect();
    Ok(Json(json!({ "cursor": cursor, "changes": changes })))
}

async fn agents(State(api): State<Api>) -> Reply<Vec<foreman_core::AgentInfo>> {
    Ok(Json(api.runner.agents().iter().map(|a| a.info()).collect()))
}

fn masked(m: &crate::agent::native::Model) -> foreman_core::ModelSettings {
    foreman_core::ModelSettings {
        url: m.url.clone(),
        model: m.model.clone(),
        key: String::new(),
        has_key: !m.key.trim().is_empty(),
    }
}

async fn get_model(State(api): State<Api>) -> Reply<foreman_core::ModelSettings> {
    let shared = api.model.as_ref().ok_or_else(|| bad("this server has no foreman agent"))?;
    Ok(Json(masked(&shared.get())))
}

async fn put_model(
    State(api): State<Api>,
    Json(new): Json<foreman_core::ModelSettings>,
) -> Reply<foreman_core::ModelSettings> {
    let shared = api.model.as_ref().ok_or_else(|| bad("this server has no foreman agent"))?;
    let mut m = shared.get();
    m.url = new.url.trim().to_string();
    m.model = new.model.trim().to_string();
    if !new.key.trim().is_empty() {
        m.key = new.key.trim().to_string();
    } else if !new.has_key {
        m.key.clear();
    }
    if m.url.is_empty() {
        return Err(bad("the server address is empty"));
    }
    let text = serde_json::to_string(&m).map_err(|e| bad(e.to_string()))?;
    api.runner.store().set_setting("model", &text)?;
    shared.set(m.clone());
    changed(&api);
    Ok(Json(masked(&m)))
}

async fn list_models(
    State(api): State<Api>,
    Json(probe): Json<foreman_core::ModelSettings>,
) -> Reply<Vec<String>> {
    let saved = api.model.as_ref().map(|m| m.get()).unwrap_or_default();
    let key = if probe.key.trim().is_empty() && probe.has_key { saved.key } else { probe.key };
    let url = probe.url.trim().trim_end_matches('/').to_string();
    let list = tokio::task::spawn_blocking(move || crate::agent::native::models(&url, &key))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(bad)?;
    Ok(Json(list))
}

async fn list_projects(State(api): State<Api>) -> Reply<Vec<foreman_core::Project>> {
    Ok(Json(api.runner.store().projects()?))
}

async fn add_project(
    State(api): State<Api>,
    Json(new): Json<NewProject>,
) -> Reply<foreman_core::Project> {
    let store = api.runner.store().clone();
    let project = tokio::task::spawn_blocking(move || {
        projects::add(&store, std::path::Path::new(new.path.trim()), new.name.as_deref())
    })
    .await
    .map_err(|e| bad(e.to_string()))??;
    changed(&api);
    Ok(Json(project))
}

async fn delete_project(State(api): State<Api>, Path(id): Path<Id>) -> Result<StatusCode, Failure> {
    for s in api.runner.store().sessions()?.iter().filter(|s| s.project_id == id) {
        api.runner.interrupt(s.id);
    }
    api.runner.store().delete_project(id)?;
    changed(&api);
    Ok(StatusCode::NO_CONTENT)
}

async fn sync_project(
    State(api): State<Api>,
    Path(id): Path<Id>,
) -> Reply<Vec<foreman_core::Worktree>> {
    let store = api.runner.store().clone();
    let moved = tokio::task::spawn_blocking(move || projects::sync(&store, id, None))
        .await
        .map_err(|e| bad(e.to_string()))??;
    if moved {
        changed(&api);
    }
    Ok(Json(api.runner.store().worktrees(Some(id))?))
}

async fn list_worktrees(
    State(api): State<Api>,
    Path(id): Path<Id>,
) -> Reply<Vec<foreman_core::Worktree>> {
    api.runner.store().project(id)?;
    Ok(Json(api.runner.store().worktrees(Some(id))?))
}

async fn add_worktree(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(new): Json<NewWorktree>,
) -> Reply<foreman_core::Worktree> {
    let store = api.runner.store().clone();
    let w = tokio::task::spawn_blocking(move || {
        projects::add_worktree(&store, id, &new.branch, new.base.as_deref())
    })
    .await
    .map_err(|e| bad(e.to_string()))??;
    changed(&api);
    Ok(Json(w))
}

#[derive(Deserialize)]
struct Force {
    #[serde(default)]
    force: bool,
}

async fn remove_worktree(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Query(q): Query<Force>,
) -> Result<StatusCode, Failure> {
    let busy = api
        .runner
        .store()
        .sessions()?
        .into_iter()
        .any(|s| s.worktree_id == id && api.runner.is_running(s.id));
    if busy {
        return Err(Failure(StatusCode::CONFLICT, "a session is working in this worktree".into()));
    }
    let store = api.runner.store().clone();
    tokio::task::spawn_blocking(move || projects::remove_worktree(&store, id, q.force))
        .await
        .map_err(|e| bad(e.to_string()))??;
    changed(&api);
    Ok(StatusCode::NO_CONTENT)
}

async fn issues(State(api): State<Api>) -> Reply<Vec<foreman_core::Issue>> {
    Ok(Json(api.runner.store().issues()?))
}

async fn issue(State(api): State<Api>, Path(id): Path<Id>) -> Reply<foreman_core::Issue> {
    Ok(Json(api.runner.store().issue(id)?))
}

async fn add_issue(
    State(api): State<Api>,
    Json(new): Json<NewIssue>,
) -> Reply<foreman_core::Issue> {
    let issue = api.runner.store().add_issue(&new)?;
    changed(&api);
    Ok(Json(issue))
}

async fn update_issue(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(patch): Json<IssuePatch>,
) -> Reply<foreman_core::Issue> {
    let issue = api.runner.store().update_issue(id, &patch)?;
    changed(&api);
    Ok(Json(issue))
}

async fn delete_issue(State(api): State<Api>, Path(id): Path<Id>) -> Result<StatusCode, Failure> {
    api.runner.store().delete_issue(id)?;
    changed(&api);
    Ok(StatusCode::NO_CONTENT)
}

async fn comments(State(api): State<Api>, Path(id): Path<Id>) -> Reply<Vec<foreman_core::Comment>> {
    api.runner.store().issue(id)?;
    Ok(Json(api.runner.store().comments(id)?))
}

async fn add_comment(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(new): Json<NewComment>,
) -> Reply<foreman_core::Comment> {
    let c = api.runner.store().add_comment(id, &new)?;
    changed(&api);
    Ok(Json(c))
}

async fn sessions(State(api): State<Api>) -> Reply<Vec<foreman_core::Session>> {
    Ok(Json(api.runner.store().sessions()?))
}

async fn session(State(api): State<Api>, Path(id): Path<Id>) -> Reply<foreman_core::Session> {
    Ok(Json(api.runner.store().session(id)?))
}

async fn spawn(api: &Api, new: NewSession) -> Result<foreman_core::Session, Failure> {
    let store = api.runner.store().clone();
    let agent =
        api.runner.agent(&new.agent).ok_or_else(|| bad(format!("no agent named {}", new.agent)))?;
    if !agent.available() {
        return Err(bad(format!("{} is not ready: {}", agent.label(), agent.about())));
    }
    let project = store.project(new.project_id)?;
    let issue = new.issue_id.map(|id| store.issue(id)).transpose()?;
    if let Some(i) = issue.as_ref().filter(|i| i.project_id != project.id) {
        return Err(bad(format!("issue #{} belongs to another project", i.id)));
    }
    let worktree = match new.place.clone() {
        Place::Main => store.main_worktree(project.id)?,
        Place::Existing { worktree_id } => {
            let w = store.worktree(worktree_id)?;
            if w.project_id != project.id {
                return Err(bad("that worktree belongs to another project"));
            }
            w
        }
        Place::New { branch } => {
            let branch = match branch.trim() {
                "" => issue
                    .as_ref()
                    .map(|i| format!("issue-{}-{}", i.id, branch_slug(&i.title)))
                    .ok_or_else(|| bad("name the branch for the new worktree"))?,
                b => b.to_string(),
            };
            let s = store.clone();
            let pid = project.id;
            tokio::task::spawn_blocking(move || projects::add_worktree(&s, pid, &branch, None))
                .await
                .map_err(|e| bad(e.to_string()))??
        }
    };
    if worktree.missing || !std::path::Path::new(&worktree.path).is_dir() {
        return Err(bad(format!("{} no longer exists", worktree.path)));
    }
    let title = new
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| issue.as_ref().map(|i| format!("#{} {}", i.id, i.title)))
        .or_else(|| new.prompt.as_deref().map(|p| foreman_core::short_title(p, TITLE_MAX)))
        .unwrap_or_else(|| format!("{} on {}", agent.label(), worktree.label()));
    let model = new.model.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let session =
        store.add_session(worktree.id, agent.name(), title.trim(), model, new.issue_id)?;
    let prompt = new.prompt.clone().filter(|p| !p.trim().is_empty()).or_else(|| {
        issue.as_ref().map(|i| issue_prompt(i, &store.comments(i.id).unwrap_or_default()))
    });
    if let Some(prompt) = prompt {
        api.runner.send(&session, &prompt)?;
    }
    changed(api);
    Ok(store.session(session.id)?)
}

async fn add_session(
    State(api): State<Api>,
    Json(new): Json<NewSession>,
) -> Reply<foreman_core::Session> {
    Ok(Json(spawn(&api, new).await?))
}

async fn start_issue(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(mut new): Json<NewSession>,
) -> Reply<foreman_core::Session> {
    let issue = api.runner.store().issue(id)?;
    new.issue_id = Some(id);
    new.project_id = issue.project_id;
    let session = spawn(&api, new).await?;
    if issue.state != IssueState::Doing {
        api.runner.store().update_issue(
            id,
            &IssuePatch { state: Some(IssueState::Doing), ..Default::default() },
        )?;
        changed(&api);
    }
    Ok(Json(session))
}

async fn delete_session(State(api): State<Api>, Path(id): Path<Id>) -> Result<StatusCode, Failure> {
    api.runner.interrupt(id);
    api.runner.store().delete_session(id)?;
    changed(&api);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct After {
    #[serde(default)]
    after: Id,
}

async fn messages(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Query(q): Query<After>,
) -> Reply<Vec<foreman_core::Message>> {
    api.runner.store().session(id)?;
    Ok(Json(api.runner.store().messages(id, q.after)?))
}

async fn send(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(msg): Json<SendMessage>,
) -> Result<StatusCode, Failure> {
    let session = api.runner.store().session(id)?;
    if !std::path::Path::new(&session.cwd).is_dir() {
        return Err(bad(format!("{} no longer exists", session.cwd)));
    }
    api.runner.send(&session, &msg.text).map_err(|e| Failure(StatusCode::CONFLICT, e.0))?;
    Ok(StatusCode::ACCEPTED)
}

async fn answer(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(a): Json<Answer>,
) -> Result<StatusCode, Failure> {
    api.runner.store().session(id)?;
    if a.answer.trim().is_empty() {
        return Err(bad("an answer is needed"));
    }
    api.runner
        .answer(id, &a.question, a.answer.trim())
        .map_err(|e| Failure(StatusCode::CONFLICT, e.0))?;
    Ok(StatusCode::ACCEPTED)
}

async fn interrupt(State(api): State<Api>, Path(id): Path<Id>) -> Reply<serde_json::Value> {
    api.runner.store().session(id)?;
    Ok(Json(json!({ "interrupted": api.runner.interrupt(id) })))
}

pub fn watch_worktrees(api: Api, every: Duration) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let store = api.runner.store().clone();
            if let Ok(Ok(true)) =
                tokio::task::spawn_blocking(move || projects::sync_all(&store)).await
            {
                changed(&api);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::JsonCommand;
    use crate::git::testing::repo;
    use crate::store::Store;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    struct Fixture {
        app: Router,
        api: Api,
        _dir: tempfile::TempDir,
        repo: std::path::PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("demo");
        repo(&repo_path);
        let store = Store::memory().unwrap();
        let echo = JsonCommand {
            name: "echo".into(),
            label: Some("Echo".into()),
            program: "sh".into(),
            args: vec![
                "-c".into(),
                r#"read p; echo "{\"text\":\"echo: $p in $(basename $PWD)\"}""#.into(),
            ],
            resume_args: vec![],
            model_args: vec![],
            prompt_on_stdin: true,
            session: None,
            text: Some("/text".into()),
            error: None,
            input_tokens: None,
            output_tokens: None,
            plain: false,
        };
        let api = Api {
            runner: Runner::new(store, vec![Box::new(echo)]),
            token: "secret".into(),
            model: None,
        };
        Fixture { app: router(api.clone()), api, _dir: dir, repo: repo_path }
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
        token: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = axum::http::Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => {
                req.header("content-type", "application/json").body(Body::from(b.to_string()))
            }
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    const T: Option<&str> = Some("secret");

    async fn project(f: &Fixture) -> Id {
        let (s, p) = call(
            &f.app,
            "POST",
            "/api/projects",
            Some(json!({"path": f.repo.to_string_lossy()})),
            T,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{p}");
        p["id"].as_i64().unwrap()
    }

    async fn settle(api: &Api, sid: Id) {
        for _ in 0..200 {
            if !api.runner.is_running(sid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("session {sid} never settled");
    }

    #[tokio::test]
    async fn the_foreman_model_is_set_live_and_its_key_never_read_back() {
        let f = fixture();
        let shared = crate::agent::native::Shared::new(crate::agent::native::Model::default());
        let api = Api { model: Some(shared.clone()), ..f.api.clone() };
        let app = router(api);
        let (s, m) = call(&app, "GET", "/api/settings/model", None, T).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(m["has_key"], false);
        let (s, m) = call(
            &app,
            "PUT",
            "/api/settings/model",
            Some(json!({"url": "http://10.0.0.5:8001/v1", "model": "qwen", "key": "sk-1", "has_key": false})),
            T,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{m}");
        assert_eq!(m["key"], "");
        assert_eq!(m["has_key"], true);
        assert_eq!(shared.get().key, "sk-1");
        assert_eq!(shared.get().url, "http://10.0.0.5:8001/v1");
        call(
            &app,
            "PUT",
            "/api/settings/model",
            Some(json!({"url": "http://10.0.0.5:8001/v1", "model": "other", "key": "", "has_key": true})),
            T,
        )
        .await;
        assert_eq!(shared.get().key, "sk-1", "an empty key keeps the saved one");
        assert_eq!(shared.get().model, "other");
        let saved = f.api.runner.store().setting("model").unwrap().unwrap();
        assert!(saved.contains("\"other\""));
    }

    #[tokio::test]
    async fn every_api_route_needs_the_token() {
        let f = fixture();
        assert_eq!(call(&f.app, "GET", "/health", None, None).await.0, StatusCode::OK);
        assert_eq!(
            call(&f.app, "GET", "/api/issues", None, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&f.app, "GET", "/api/issues", None, Some("wrong")).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(call(&f.app, "GET", "/api/issues", None, T).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_project_is_its_repo_and_worktrees() {
        let f = fixture();
        let pid = project(&f).await;
        let (_, overview) = call(&f.app, "GET", "/api/overview", None, T).await;
        assert_eq!(overview["projects"][0]["name"], "demo");
        assert_eq!(overview["worktrees"].as_array().unwrap().len(), 1);
        assert_eq!(overview["worktrees"][0]["main"], true);

        let (s, w) = call(
            &f.app,
            "POST",
            &format!("/api/projects/{pid}/worktrees"),
            Some(json!({"branch": "side"})),
            T,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{w}");
        assert_eq!(w["branch"], "side");
        let wid = w["id"].as_i64().unwrap();
        let (_, trees) =
            call(&f.app, "GET", &format!("/api/projects/{pid}/worktrees"), None, T).await;
        assert_eq!(trees.as_array().unwrap().len(), 2);
        assert_eq!(
            call(&f.app, "DELETE", &format!("/api/worktrees/{wid}"), None, T).await.0,
            StatusCode::NO_CONTENT
        );
        let (s, _) = call(&f.app, "POST", "/api/projects", Some(json!({"path": "/tmp"})), T).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "not a repo");
    }

    #[tokio::test]
    async fn issues_and_comments_over_http() {
        let f = fixture();
        let pid = project(&f).await;
        let (s, issue) = call(
            &f.app,
            "POST",
            "/api/issues",
            Some(json!({"project_id": pid, "title": "Fix it"})),
            T,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let id = issue["id"].as_i64().unwrap();
        let (_, patched) = call(
            &f.app,
            "PATCH",
            &format!("/api/issues/{id}"),
            Some(json!({"state": "review"})),
            T,
        )
        .await;
        assert_eq!(patched["state"], "review");
        call(
            &f.app,
            "POST",
            &format!("/api/issues/{id}/comments"),
            Some(json!({"body": "looks good"})),
            T,
        )
        .await;
        let (_, comments) =
            call(&f.app, "GET", &format!("/api/issues/{id}/comments"), None, T).await;
        assert_eq!(comments[0]["body"], "looks good");
        assert_eq!(call(&f.app, "GET", "/api/issues/999", None, T).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn starting_an_issue_in_a_new_worktree() {
        let f = fixture();
        let pid = project(&f).await;
        let (_, issue) = call(
            &f.app,
            "POST",
            "/api/issues",
            Some(json!({"project_id": pid, "title": "Say hi"})),
            T,
        )
        .await;
        let id = issue["id"].as_i64().unwrap();
        let (s, session) = call(
            &f.app,
            "POST",
            &format!("/api/issues/{id}/start"),
            Some(json!({"project_id": 0, "agent": "echo", "place": {"kind": "new", "branch": ""}})),
            T,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{session}");
        assert_eq!(session["title"], format!("#{id} Say hi"));
        assert!(session["cwd"].as_str().unwrap().ends_with(&format!("demo-issue-{id}-say-hi")));
        let sid = session["id"].as_i64().unwrap();
        settle(&f.api, sid).await;
        let (_, msgs) =
            call(&f.app, "GET", &format!("/api/sessions/{sid}/messages"), None, T).await;
        assert_eq!(
            msgs[1]["entry"]["text"],
            format!("echo: Work on issue #{id}: Say hi in demo-issue-{id}-say-hi")
        );
        let (_, issue) = call(&f.app, "GET", &format!("/api/issues/{id}"), None, T).await;
        assert_eq!(issue["state"], "doing");
        let (_, trees) =
            call(&f.app, "GET", &format!("/api/projects/{pid}/worktrees"), None, T).await;
        assert_eq!(trees[1]["created_by_foreman"], true);
    }

    #[tokio::test]
    async fn two_sessions_share_the_main_worktree() {
        let f = fixture();
        let pid = project(&f).await;
        for _ in 0..2 {
            let (s, sess) = call(
                &f.app,
                "POST",
                "/api/sessions",
                Some(json!({"project_id": pid, "agent": "echo", "prompt": "hi"})),
                T,
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{sess}");
            assert_eq!(sess["cwd"], f.repo.canonicalize().unwrap().to_string_lossy().as_ref());
        }
    }

    #[tokio::test]
    async fn a_busy_worktree_cannot_be_removed() {
        let f = fixture();
        let pid = project(&f).await;
        let mut slow = JsonCommand {
            name: "slow".into(),
            label: None,
            program: "sh".into(),
            args: vec!["-c".into(), "sleep 5".into()],
            resume_args: vec![],
            model_args: vec![],
            prompt_on_stdin: false,
            session: None,
            text: None,
            error: None,
            input_tokens: None,
            output_tokens: None,
            plain: true,
        };
        slow.plain = true;
        let api = Api {
            runner: Runner::new(f.api.runner.store().clone(), vec![Box::new(slow)]),
            token: "secret".into(),
            model: None,
        };
        let app = router(api.clone());
        let (_, sess) = call(
            &app,
            "POST",
            "/api/sessions",
            Some(json!({"project_id": pid, "agent": "slow", "place": {"kind": "new", "branch": "busy"}, "prompt": "go"})),
            T,
        )
        .await;
        let wid = sess["worktree_id"].as_i64().unwrap();
        assert_eq!(
            call(&app, "DELETE", &format!("/api/worktrees/{wid}"), None, T).await.0,
            StatusCode::CONFLICT
        );
        call(&app, "POST", &format!("/api/sessions/{}/interrupt", sess["id"]), None, T).await;
        settle(&api, sess["id"].as_i64().unwrap()).await;
        assert_eq!(
            call(&app, "DELETE", &format!("/api/worktrees/{wid}?force=true"), None, T).await.0,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn spawning_checks_the_agent_and_place() {
        let f = fixture();
        let pid = project(&f).await;
        let (s, e) = call(
            &f.app,
            "POST",
            "/api/sessions",
            Some(json!({"project_id": pid, "agent": "nope"})),
            T,
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(e["error"], "no agent named nope");
        let (s, _) = call(
            &f.app,
            "POST",
            "/api/sessions",
            Some(
                json!({"project_id": pid, "agent": "echo", "place": {"kind": "new", "branch": ""}}),
            ),
            T,
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "a new worktree needs a branch name");
        let (s, _) = call(
            &f.app,
            "POST",
            "/api/sessions",
            Some(json!({"project_id": 99, "agent": "echo"})),
            T,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn changes_long_poll_wakes_on_a_write() {
        let f = fixture();
        let pid = project(&f).await;
        let (_, first) = call(&f.app, "GET", "/api/changes", None, T).await;
        let cursor = first["cursor"].as_i64().unwrap();
        let app2 = f.app.clone();
        let waiter = tokio::spawn(async move {
            call(&app2, "GET", &format!("/api/changes?since={cursor}&wait=10"), None, T).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        call(&f.app, "POST", "/api/issues", Some(json!({"project_id": pid, "title": "x"})), T)
            .await;
        let (_, woke) =
            tokio::time::timeout(Duration::from_secs(3), waiter).await.unwrap().unwrap();
        assert!(woke["cursor"].as_i64().unwrap() > cursor);
        assert_eq!(woke["changes"][0]["kind"], "issue");
    }
}
