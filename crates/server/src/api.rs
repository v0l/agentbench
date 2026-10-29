use crate::projects;
use crate::runner::Runner;
use crate::store::StoreError;
use agentbench_core::{
    Answer, ApiError, Id, IssuePatch, IssueState, NewComment, NewIssue, NewProject, NewSession,
    NewWorktree, Overview, SendMessage, SessionId,
};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

const LONG_POLL: Duration = Duration::from_secs(25);

#[derive(Clone)]
pub struct Api {
    pub runner: Runner,
    pub token: String,
    pub model: Option<crate::agent::native::Shared>,
    pub speech: crate::speech::Speech,
    pub voice: crate::voice::Voice,
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

/// Room for a message with every attachment it may carry, base64 encoded.
const BODY_MAX: usize =
    agentbench_core::ATTACHMENTS_MAX * agentbench_core::ATTACHMENT_MAX * 4 / 3 + (1 << 20);

pub fn router(api: Api) -> Router {
    let protected = Router::new()
        .route("/api/overview", get(overview))
        .route("/api/changes", get(changes))
        .route("/api/agents", get(agents))
        .route("/api/settings/model", get(get_model).put(put_model))
        .route("/api/settings/models", post(list_models))
        .route("/api/settings/cross-project", get(get_cross).delete(clear_cross))
        .route("/api/settings/title", get(get_title).put(put_title))
        .route("/api/settings/title/models", post(list_title_models))
        .route("/api/agents/{name}/models", get(agent_models))
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
        .route("/v1/audio/transcriptions", post(transcribe))
        .route("/v1/audio/speech", post(speak))
        .route("/v1/audio/voices", get(voices))
        .route("/v1/audio/models", get(speech_models))
        .route("/v1/audio/status", get(audio_status))
        .route("/api/sessions/{id}/attachments/{file}", get(attachment))
        .layer(axum::extract::DefaultBodyLimit::max(BODY_MAX))
        .layer(middleware::from_fn_with_state(api.clone(), auth));
    Router::new()
        .route("/health", get(|| async { Json(json!({ "ok": true })) }))
        .merge(protected)
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(api)
}

/// OpenAI's speech to text, answered with Whisper on this machine. Takes a multipart form with a
/// `file` (WAV) and optionally `model`; answers `{"text": "..."}`.
async fn transcribe(
    State(api): State<Api>,
    mut form: axum::extract::Multipart,
) -> Result<Json<serde_json::Value>, Failure> {
    let (mut wav, mut asked) = (None, String::new());
    while let Some(field) = form.next_field().await.map_err(|e| bad(e.to_string()))? {
        match field.name().unwrap_or_default() {
            "file" => wav = Some(field.bytes().await.map_err(|e| bad(e.to_string()))?),
            "model" => asked = field.text().await.map_err(|e| bad(e.to_string()))?,
            _ => {}
        }
    }
    let wav = wav.ok_or_else(|| bad("the form has no file"))?;
    let (pcm, rate) = egui_bench::voice::audio::read_wav(&wav).map_err(bad)?;
    let pcm = egui_bench::voice::audio::to_whisper_rate(&pcm, rate);
    let id = api.speech.pick(&asked);
    let started = std::time::Instant::now();
    let (text, on) = api
        .speech
        .read(&id, pcm.clone())
        .await
        .map_err(|e| Failure(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    tracing::info!(
        "read {:.1}s of speech with {id} on the {on} in {:.1}s",
        pcm.len() as f32 / egui_bench::voice::audio::SPEECH_RATE as f32,
        started.elapsed().as_secs_f32()
    );
    Ok(Json(json!({ "text": text })))
}

#[derive(Deserialize)]
struct SpeechRequest {
    input: String,
    #[serde(default)]
    voice: String,
    /// `pcm` streams raw 16 bit little endian mono samples as each sentence is made; anything
    /// else is one WAV when all of it is made.
    #[serde(default)]
    response_format: String,
}

fn pcm_bytes(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect()
}

/// OpenAI's text to speech, answered with Kokoro on this machine. Takes `{"input": "...",
/// "voice": "af_heart"}` and answers a 16 bit mono WAV. With `"response_format": "pcm"` it
/// answers raw samples instead, a sentence at a time as they are made, with the rate in the
/// `x-sample-rate` header, so the first words can be played while the rest are still coming.
async fn speak(
    State(api): State<Api>,
    Json(req): Json<SpeechRequest>,
) -> Result<Response, Failure> {
    let text = req.input.trim();
    if text.is_empty() {
        return Err(bad("there is nothing to say"));
    }
    if text.len() > crate::voice::INPUT_MAX {
        return Err(bad(format!(
            "too long to say in one go: {} characters at most",
            crate::voice::INPUT_MAX
        )));
    }
    let voice = req.voice.clone();
    let started = std::time::Instant::now();
    if req.response_format.eq_ignore_ascii_case("pcm") {
        let (rate, on, rx) = api
            .voice
            .stream(&voice, text.to_string())
            .await
            .map_err(|e| Failure(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        tracing::info!(
            "streaming speech on the {on}, ready in {:.1}s",
            started.elapsed().as_secs_f32()
        );
        let frames = futures_util::stream::unfold(rx, |mut rx| async move {
            match rx.recv().await {
                Some(Ok(pcm)) => {
                    Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(pcm_bytes(&pcm))), rx))
                }
                // A failure part way ends the stream early; the headers are long gone.
                Some(Err(e)) => {
                    tracing::warn!("speech stopped part way: {e}");
                    None
                }
                None => None,
            }
        });
        return Ok((
            [
                (axum::http::header::CONTENT_TYPE, "audio/L16".to_string()),
                (axum::http::HeaderName::from_static("x-sample-rate"), rate.to_string()),
                (axum::http::header::CACHE_CONTROL, "no-store".to_string()),
            ],
            axum::body::Body::from_stream(frames),
        )
            .into_response());
    }
    let said = api
        .voice
        .say(&voice, text.to_string())
        .await
        .map_err(|e| Failure(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    tracing::info!(
        "said {:.1}s on the {} in {:.1}s",
        said.samples.len() as f32 / said.rate as f32,
        said.on,
        started.elapsed().as_secs_f32()
    );
    let wav = egui_bench::voice::audio::wav_bytes(&said.samples, said.rate);
    Ok(([(axum::http::header::CONTENT_TYPE, "audio/wav")], wav).into_response())
}

/// Where the speech models run: the device a model loaded now would use, and what each is using
/// once it has been used.
async fn audio_status(State(api): State<Api>) -> Json<agentbench_core::AudioStatus> {
    let device = tokio::task::spawn_blocking(crate::voice::Voice::device).await.unwrap_or_default();
    Json(agentbench_core::AudioStatus {
        device,
        reading_on: api.speech.on(),
        speaking_on: api.voice.on(),
    })
}

/// The speech to text models this server reads with, in OpenAI's `/v1/models` shape plus what a
/// picker needs: `{"data": [{"id", "label", "size_mb", "installed", "default"}]}`.
async fn speech_models(State(api): State<Api>) -> Json<serde_json::Value> {
    let data: Vec<_> = api
        .speech
        .list()
        .into_iter()
        .map(|(id, label, size_mb, installed, default)| {
            json!({ "id": id, "object": "model", "label": label, "size_mb": size_mb, "installed": installed, "default": default })
        })
        .collect();
    Json(json!({ "object": "list", "data": data }))
}

/// The voices this server can speak in: `{"voices": [{"id", "label", "installed"}]}`.
async fn voices(State(api): State<Api>) -> Json<serde_json::Value> {
    if let Err(e) = api.voice.catalogue().await {
        tracing::warn!("could not fetch the voice catalogue: {e}");
    }
    let list: Vec<_> = api
        .voice
        .list()
        .into_iter()
        .map(|(id, label, installed)| json!({ "id": id, "label": label, "installed": installed }))
        .collect();
    Json(json!({ "voices": list }))
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
        sessions: api.runner.with_queued(store.sessions()?),
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

async fn agents(State(api): State<Api>) -> Reply<Vec<agentbench_core::AgentInfo>> {
    Ok(Json(api.runner.agents().iter().map(|a| a.info()).collect()))
}

fn level_masked(l: &crate::agent::native::Level) -> agentbench_core::LevelSettings {
    agentbench_core::LevelSettings {
        url: l.url.clone(),
        model: l.model.clone(),
        key: String::new(),
        has_key: !l.key.trim().is_empty(),
        effort: l.effort.clone(),
    }
}

fn masked(m: &crate::agent::native::Model) -> agentbench_core::ModelSettings {
    agentbench_core::ModelSettings {
        url: m.url.clone(),
        model: m.model.clone(),
        key: String::new(),
        has_key: !m.key.trim().is_empty(),
        steps: m.steps,
        compact_at: m.compact_at,
        low: level_masked(&m.low),
        medium: level_masked(&m.medium),
        high: level_masked(&m.high),
        effort: m.effort.clone(),
    }
}

/// Apply what the window sent for one level. A blank key keeps the saved one
/// unless the window says none is kept, and a level with no server of its own
/// has no key to keep.
fn level_merged(
    old: &crate::agent::native::Level,
    new: &agentbench_core::LevelSettings,
) -> crate::agent::native::Level {
    let url = new.url.trim().to_string();
    let key = if url.is_empty() {
        String::new()
    } else if !new.key.trim().is_empty() {
        new.key.trim().to_string()
    } else if new.has_key {
        old.key.clone()
    } else {
        String::new()
    };
    crate::agent::native::Level {
        url,
        model: new.model.trim().to_string(),
        key,
        effort: clean_effort(&new.effort),
    }
}

fn clean_effort(effort: &str) -> String {
    let effort = effort.trim().to_lowercase();
    if agentbench_core::EFFORTS.contains(&effort.as_str()) { effort } else { String::new() }
}

async fn get_model(State(api): State<Api>) -> Reply<agentbench_core::ModelSettings> {
    let shared = api.model.as_ref().ok_or_else(|| bad("this server has no native agent"))?;
    Ok(Json(masked(&shared.get())))
}

async fn put_model(
    State(api): State<Api>,
    Json(new): Json<agentbench_core::ModelSettings>,
) -> Reply<agentbench_core::ModelSettings> {
    let shared = api.model.as_ref().ok_or_else(|| bad("this server has no native agent"))?;
    let mut m = shared.get();
    m.url = new.url.trim().to_string();
    m.model = new.model.trim().to_string();
    if !new.key.trim().is_empty() {
        m.key = new.key.trim().to_string();
    } else if !new.has_key {
        m.key.clear();
    }
    if new.steps > 0 {
        m.steps = new.steps.min(100_000);
    }
    m.compact_at = new.compact_at.min(100);
    m.effort = clean_effort(&new.effort);
    m.low = level_merged(&m.low, &new.low);
    m.medium = level_merged(&m.medium, &new.medium);
    m.high = level_merged(&m.high, &new.high);
    if m.url.is_empty() {
        return Err(bad("the server address is empty"));
    }
    let text = serde_json::to_string(&m).map_err(|e| bad(e.to_string()))?;
    api.runner.store().set_setting("model", &text)?;
    shared.set(m.clone());
    changed(&api);
    Ok(Json(masked(&m)))
}

fn title_masked(t: &crate::titles::TitleModel) -> agentbench_core::TitleSettings {
    agentbench_core::TitleSettings {
        enabled: t.enabled,
        url: t.url.clone(),
        model: t.model.clone(),
        key: String::new(),
        has_key: !t.key.trim().is_empty(),
    }
}

fn titles_of(api: &Api) -> Result<&crate::titles::Titles, Failure> {
    api.runner.titles().ok_or_else(|| bad("this server has no title model"))
}

/// Which projects agents may start sessions in without asking.
async fn get_cross(State(api): State<Api>) -> Reply<serde_json::Value> {
    let store = api.runner.store();
    let any = store.setting("start_session_any_project")?.as_deref() == Some("1");
    let ids: Vec<Id> = store
        .setting("start_session_projects")?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Ok(Json(json!({ "any": any, "projects": ids })))
}

/// Take back every "always allow" and ask again.
async fn clear_cross(State(api): State<Api>) -> Result<StatusCode, Failure> {
    let store = api.runner.store();
    store.delete_setting("start_session_any_project")?;
    store.delete_setting("start_session_projects")?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_title(State(api): State<Api>) -> Reply<agentbench_core::TitleSettings> {
    Ok(Json(title_masked(&titles_of(&api)?.config())))
}

async fn put_title(
    State(api): State<Api>,
    Json(new): Json<agentbench_core::TitleSettings>,
) -> Reply<agentbench_core::TitleSettings> {
    let titles = titles_of(&api)?;
    let mut t = titles.config();
    t.enabled = new.enabled;
    t.url = new.url.trim().to_string();
    t.model = new.model.trim().to_string();
    if !new.key.trim().is_empty() {
        t.key = new.key.trim().to_string();
    } else if !new.has_key || t.url.is_empty() {
        t.key.clear();
    }
    let text = serde_json::to_string(&t).map_err(|e| bad(e.to_string()))?;
    api.runner.store().set_setting("title_model", &text)?;
    titles.set_config(t.clone());
    changed(&api);
    Ok(Json(title_masked(&t)))
}

async fn list_title_models(
    State(api): State<Api>,
    Json(probe): Json<agentbench_core::TitleSettings>,
) -> Reply<Vec<String>> {
    let titles = titles_of(&api)?;
    let saved = titles.config();
    let key = if probe.key.trim().is_empty() && probe.has_key { saved.key } else { probe.key };
    // Ask whichever server the title model would use: its own, else the native agent's.
    let ask = crate::titles::TitleModel { enabled: true, url: probe.url, model: "-".into(), key };
    let target = ask.resolve(&titles.main()).ok_or_else(|| bad("no server to ask"))?;
    let url = target.url.trim().trim_end_matches('/').to_string();
    let list = crate::agent::native::models(&url, &target.key).await.map_err(bad)?;
    Ok(Json(list))
}

async fn agent_models(State(api): State<Api>, Path(name): Path<String>) -> Reply<Vec<String>> {
    let agent = api.runner.agent(&name).ok_or_else(|| bad(format!("no agent {name}")))?;
    Ok(Json(agent.models().await.map_err(bad)?))
}

async fn list_models(
    State(api): State<Api>,
    Json(probe): Json<agentbench_core::ModelSettings>,
) -> Reply<Vec<String>> {
    let saved = api.model.as_ref().map(|m| m.get()).unwrap_or_default();
    let key = if probe.key.trim().is_empty() && probe.has_key { saved.key } else { probe.key };
    let url = probe.url.trim().trim_end_matches('/').to_string();
    let list = crate::agent::native::models(&url, &key).await.map_err(bad)?;
    Ok(Json(list))
}

async fn list_projects(State(api): State<Api>) -> Reply<Vec<agentbench_core::Project>> {
    Ok(Json(api.runner.store().projects()?))
}

async fn add_project(
    State(api): State<Api>,
    Json(new): Json<NewProject>,
) -> Reply<agentbench_core::Project> {
    let store = api.runner.store().clone();
    let project = tokio::task::spawn_blocking(move || {
        projects::add(
            &store,
            std::path::Path::new(new.path.trim()),
            new.name.as_deref(),
            new.create,
        )
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
) -> Reply<Vec<agentbench_core::Worktree>> {
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
) -> Reply<Vec<agentbench_core::Worktree>> {
    api.runner.store().project(id)?;
    Ok(Json(api.runner.store().worktrees(Some(id))?))
}

async fn add_worktree(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(new): Json<NewWorktree>,
) -> Reply<agentbench_core::Worktree> {
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

async fn issues(State(api): State<Api>) -> Reply<Vec<agentbench_core::Issue>> {
    Ok(Json(api.runner.store().issues()?))
}

async fn issue(State(api): State<Api>, Path(id): Path<Id>) -> Reply<agentbench_core::Issue> {
    Ok(Json(api.runner.store().issue(id)?))
}

async fn add_issue(
    State(api): State<Api>,
    Json(new): Json<NewIssue>,
) -> Reply<agentbench_core::Issue> {
    let issue = api.runner.store().add_issue(&new)?;
    changed(&api);
    Ok(Json(issue))
}

async fn update_issue(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(patch): Json<IssuePatch>,
) -> Reply<agentbench_core::Issue> {
    let issue = api.runner.store().update_issue(id, &patch)?;
    changed(&api);
    Ok(Json(issue))
}

async fn delete_issue(State(api): State<Api>, Path(id): Path<Id>) -> Result<StatusCode, Failure> {
    api.runner.store().delete_issue(id)?;
    changed(&api);
    Ok(StatusCode::NO_CONTENT)
}

async fn comments(
    State(api): State<Api>,
    Path(id): Path<Id>,
) -> Reply<Vec<agentbench_core::Comment>> {
    api.runner.store().issue(id)?;
    Ok(Json(api.runner.store().comments(id)?))
}

async fn add_comment(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(new): Json<NewComment>,
) -> Reply<agentbench_core::Comment> {
    let c = api.runner.store().add_comment(id, &new)?;
    changed(&api);
    Ok(Json(c))
}

async fn sessions(State(api): State<Api>) -> Reply<Vec<agentbench_core::Session>> {
    Ok(Json(api.runner.with_queued(api.runner.store().sessions()?)))
}

async fn session(
    State(api): State<Api>,
    Path(id): Path<SessionId>,
) -> Reply<agentbench_core::Session> {
    let session = api.runner.store().session(id)?;
    Ok(Json(api.runner.with_queued(vec![session]).remove(0)))
}

async fn spawn(api: &Api, new: NewSession) -> Result<agentbench_core::Session, Failure> {
    let runner = api.runner.clone();
    tokio::task::spawn_blocking(move || runner.start_session(new))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(Failure::from)
}

async fn add_session(
    State(api): State<Api>,
    Json(new): Json<NewSession>,
) -> Reply<agentbench_core::Session> {
    Ok(Json(spawn(&api, new).await?))
}

async fn start_issue(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(mut new): Json<NewSession>,
) -> Reply<agentbench_core::Session> {
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

async fn delete_session(
    State(api): State<Api>,
    Path(id): Path<SessionId>,
) -> Result<StatusCode, Failure> {
    api.runner.interrupt(id);
    api.runner.end(id);
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
    Path(id): Path<SessionId>,
    Query(q): Query<After>,
) -> Reply<Vec<agentbench_core::Message>> {
    api.runner.store().session(id)?;
    Ok(Json(api.runner.store().messages(id, q.after)?))
}

async fn send(
    State(api): State<Api>,
    Path(id): Path<SessionId>,
    Json(msg): Json<SendMessage>,
) -> Result<StatusCode, Failure> {
    let session = api.runner.store().session(id)?;
    if !std::path::Path::new(&session.cwd).is_dir() {
        return Err(bad(format!("{} no longer exists", session.cwd)));
    }
    api.runner
        .send_with(&session, &msg.text, &msg.attachments)
        .map_err(|e| Failure(StatusCode::CONFLICT, e.0))?;
    Ok(StatusCode::ACCEPTED)
}

async fn answer(
    State(api): State<Api>,
    Path(id): Path<SessionId>,
    Json(a): Json<Answer>,
) -> Result<StatusCode, Failure> {
    api.runner.store().session(id)?;
    let mut answers: Vec<String> = a.answers.iter().map(|s| s.trim().to_string()).collect();
    if answers.is_empty() {
        answers.push(a.answer.trim().to_string());
    }
    if answers.iter().all(|s| s.is_empty()) {
        return Err(bad("an answer is needed"));
    }
    api.runner.answer(id, &a.question, answers).map_err(|e| Failure(StatusCode::CONFLICT, e.0))?;
    Ok(StatusCode::ACCEPTED)
}

async fn attachment(
    State(api): State<Api>,
    Path((id, file)): Path<(SessionId, Id)>,
) -> Result<Response, Failure> {
    let (a, data) = api.runner.store().session_attachment(id, file)?;
    let disposition = format!("inline; filename=\"{}\"", a.name.replace(['"', '\\'], "_"));
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, a.mime),
            (axum::http::header::CONTENT_DISPOSITION, disposition),
            (axum::http::header::CACHE_CONTROL, "private, max-age=31536000, immutable".into()),
        ],
        data,
    )
        .into_response())
}

async fn interrupt(
    State(api): State<Api>,
    Path(id): Path<SessionId>,
) -> Reply<agentbench_core::Interrupted> {
    api.runner.store().session(id)?;
    let queued = api.runner.interrupt_returning_queued(id);
    Ok(Json(agentbench_core::Interrupted {
        interrupted: queued.is_some(),
        queued: queued.unwrap_or_default(),
    }))
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
            speech: Default::default(),
            voice: Default::default(),
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

    fn sid(v: &serde_json::Value) -> SessionId {
        v["id"].as_str().expect("a session id").parse().unwrap()
    }

    async fn settle(api: &Api, sid: SessionId) {
        for _ in 0..200 {
            if !api.runner.is_running(sid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("session {sid} never settled");
    }

    #[tokio::test]
    async fn speech_needs_the_token_and_something_to_say() {
        let f = fixture();
        let body = json!({"input": "hello", "voice": "af_heart"});
        let (code, _) = call(&f.app, "POST", "/v1/audio/speech", Some(body), None).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let blank = json!({"input": "   "});
        let (code, _) = call(&f.app, "POST", "/v1/audio/speech", Some(blank), Some("secret")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "nothing to say");
        let long = json!({"input": "a".repeat(crate::voice::INPUT_MAX + 1)});
        let (code, _) = call(&f.app, "POST", "/v1/audio/speech", Some(long), Some("secret")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "too long");
    }

    #[tokio::test]
    async fn the_audio_status_names_a_device_behind_the_token() {
        let f = fixture();
        let (code, _) = call(&f.app, "GET", "/v1/audio/status", None, None).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, v) = call(&f.app, "GET", "/v1/audio/status", None, Some("secret")).await;
        assert_eq!(code, StatusCode::OK);
        assert!(matches!(v["device"].as_str(), Some("GPU" | "CPU")), "{v}");
        assert!(v["speaking_on"].is_null() && v["reading_on"].is_null(), "nothing has run yet");
    }

    #[tokio::test]
    async fn the_speech_models_are_listed_behind_the_token() {
        let f = fixture();
        let (code, _) = call(&f.app, "GET", "/v1/audio/models", None, None).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, v) = call(&f.app, "GET", "/v1/audio/models", None, Some("secret")).await;
        assert_eq!(code, StatusCode::OK);
        let data = v["data"].as_array().unwrap();
        assert!(!data.is_empty());
        assert_eq!(data.iter().filter(|m| m["default"] == true).count(), 1, "one default");
        assert!(data.iter().all(|m| m["id"].is_string() && m["installed"].is_boolean()), "{v}");
    }

    #[tokio::test]
    async fn the_voices_are_listed_behind_the_token() {
        let f = fixture();
        let (code, _) = call(&f.app, "GET", "/v1/audio/voices", None, None).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        let (code, v) = call(&f.app, "GET", "/v1/audio/voices", None, Some("secret")).await;
        assert_eq!(code, StatusCode::OK);
        assert!(v["voices"].is_array());
    }

    #[tokio::test]
    async fn an_attached_file_is_kept_shown_and_handed_to_the_agent() {
        use base64::Engine;
        let f = fixture();
        let pid = project(&f).await;
        let png = b"\x89PNG fake bytes";
        let body = json!({
            "project_id": pid,
            "agent": "echo",
            "place": {"kind": "main"},
            "prompt": "look at this",
            "attachments": [{
                "name": "../shot.png",
                "mime": "image/png",
                "data": base64::engine::general_purpose::STANDARD.encode(png),
            }],
        });
        let (s, session) = call(&f.app, "POST", "/api/sessions", Some(body), T).await;
        assert_eq!(s, StatusCode::OK, "{session}");
        let id = sid(&session);
        settle(&f.api, id).await;
        let (_, msgs) = call(&f.app, "GET", &format!("/api/sessions/{id}/messages"), None, T).await;
        let first = &msgs[0]["entry"];
        assert_eq!(first["text"], "look at this");
        let file = &first["attachments"][0];
        assert_eq!(
            (file["name"].as_str(), file["mime"].as_str()),
            (Some("shot.png"), Some("image/png"))
        );
        let aid = file["id"].as_i64().unwrap();
        let said = msgs[1]["entry"]["text"].as_str().unwrap();
        assert!(said.contains("look at this"), "{said}");
        let path =
            std::env::temp_dir().join("agentbench").join(id.to_string()).join(format!("{aid}.png"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            png,
            "the file is written where the agent is told"
        );

        let req = axum::http::Request::builder()
            .uri(format!("/api/sessions/{id}/attachments/{aid}"))
            .header("authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let res = f.app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "image/png");
        assert_eq!(&res.into_body().collect().await.unwrap().to_bytes()[..], png);

        let (_, other) = call(
            &f.app,
            "POST",
            "/api/sessions",
            Some(json!({"project_id": pid, "agent": "echo", "place": {"kind": "main"}})),
            T,
        )
        .await;
        let (s, _) = call(
            &f.app,
            "GET",
            &format!("/api/sessions/{}/attachments/{aid}", sid(&other)),
            None,
            T,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "another chat cannot read it");
    }

    #[tokio::test]
    async fn a_file_that_is_not_base64_is_refused() {
        let f = fixture();
        let pid = project(&f).await;
        let (_, session) = call(
            &f.app,
            "POST",
            "/api/sessions",
            Some(json!({"project_id": pid, "agent": "echo", "place": {"kind": "main"}})),
            T,
        )
        .await;
        let body = json!({"text": "x", "attachments": [{"name": "a.png", "mime": "image/png", "data": "%%%"}]});
        let (s, e) = call(
            &f.app,
            "POST",
            &format!("/api/sessions/{}/messages", sid(&session)),
            Some(body),
            T,
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{e}");
        assert!(e["error"].as_str().unwrap().contains("base64"), "{e}");
    }

    #[tokio::test]
    async fn only_a_title_cut_from_the_prompt_may_be_replaced_later() {
        let f = fixture();
        let pid = project(&f).await;
        let start = |extra: serde_json::Value| {
            let mut body = json!({"project_id": pid, "agent": "echo", "place": {"kind": "main"}});
            body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            call(&f.app, "POST", "/api/sessions", Some(body), T)
        };
        let (_, cut) = start(json!({"prompt": "fix the login loop"})).await;
        let (_, named) = start(json!({"prompt": "fix the login loop", "title": "Mine"})).await;
        let (_, blank) = start(json!({})).await;
        let store = f.api.runner.store();
        assert!(store.title_is_auto(sid(&cut)).unwrap());
        assert!(!store.title_is_auto(sid(&named)).unwrap());
        assert!(store.title_is_auto(sid(&blank)).unwrap());
    }

    #[tokio::test]
    async fn the_title_model_is_set_live_and_its_key_never_read_back() {
        use crate::agent::native::{Model, Shared};
        use crate::titles::{TitleModel, Titles};
        let f = fixture();
        let titles = Titles::new(TitleModel::default(), Shared::new(Model::default()));
        let runner = f.api.runner.clone().with_titles(titles.clone());
        let app = router(Api { runner, ..f.api.clone() });
        let url = "http://10.0.0.5:11434/v1";

        let (s, got) = call(&app, "GET", "/api/settings/title", None, T).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!((got["enabled"].clone(), got["has_key"].clone()), (json!(true), json!(false)));

        let put = json!({"enabled": true, "url": format!(" {url} "), "model": "tiny", "key": "sk-2", "has_key": false});
        let (s, got) = call(&app, "PUT", "/api/settings/title", Some(put), T).await;
        assert_eq!(s, StatusCode::OK, "{got}");
        assert_eq!((got["key"].clone(), got["has_key"].clone()), (json!(""), json!(true)));
        assert_eq!((titles.config().url.as_str(), titles.config().key.as_str()), (url, "sk-2"));
        assert!(f.api.runner.store().setting("title_model").unwrap().is_some());

        let keep =
            json!({"enabled": false, "url": url, "model": "tiny", "key": "", "has_key": true});
        let (_, got) = call(&app, "PUT", "/api/settings/title", Some(keep), T).await;
        assert_eq!(got["enabled"], false);
        assert_eq!(titles.config().key, "sk-2", "an empty key with has_key keeps the saved one");
        assert!(titles.target().is_none(), "disabled means no title model");

        let drop =
            json!({"enabled": true, "url": url, "model": "tiny", "key": "", "has_key": false});
        call(&app, "PUT", "/api/settings/title", Some(drop), T).await;
        assert_eq!(titles.config().key, "");
    }

    #[tokio::test]
    async fn the_agentbench_model_is_set_live_and_its_key_never_read_back() {
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
    async fn always_allow_for_other_projects_can_be_read_and_taken_back() {
        let f = fixture();
        let app = router(f.api.clone());
        let store = f.api.runner.store();
        store.set_setting("start_session_projects", "[3]").unwrap();
        store.set_setting("start_session_any_project", "1").unwrap();
        let (s, got) = call(&app, "GET", "/api/settings/cross-project", None, T).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(got, json!({"any": true, "projects": [3]}));
        let (s, _) = call(&app, "DELETE", "/api/settings/cross-project", None, T).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (_, got) = call(&app, "GET", "/api/settings/cross-project", None, T).await;
        assert_eq!(got, json!({"any": false, "projects": []}));
    }

    #[tokio::test]
    async fn the_model_levels_are_saved_and_a_level_key_is_never_read_back_or_borrowed() {
        let f = fixture();
        let shared = crate::agent::native::Shared::new(crate::agent::native::Model::default());
        let api = Api { model: Some(shared.clone()), ..f.api.clone() };
        let app = router(api);
        let put = |body: serde_json::Value| {
            let app = app.clone();
            async move { call(&app, "PUT", "/api/settings/model", Some(body), T).await }
        };
        let base =
            json!({"url": "http://main/v1", "model": "mid", "key": "main-key", "has_key": false});
        let mut body = base.clone();
        body["low"] = json!({"model": "tiny"});
        body["high"] =
            json!({"url": "http://big/v1", "model": "huge", "key": "big-key", "has_key": false});
        let (s, m) = put(body).await;
        assert_eq!(s, StatusCode::OK, "{m}");
        assert_eq!(m["low"]["model"], "tiny");
        assert_eq!(m["high"]["key"], "");
        assert_eq!(m["high"]["has_key"], true);
        assert_eq!(m["medium"]["model"], "");
        let saved = shared.get();
        assert_eq!(saved.level("low").unwrap().key, "main-key");
        assert_eq!(saved.level("high").unwrap().key, "big-key");
        // Saving again with a blank key keeps the level's key.
        let mut again = base.clone();
        again["high"] =
            json!({"url": "http://big/v1", "model": "huger", "key": "", "has_key": true});
        put(again).await;
        assert_eq!(shared.get().high.key, "big-key");
        assert_eq!(shared.get().high.model, "huger");
        assert_eq!(shared.get().low.model, "", "a level left out is cleared");
        // A level that lost its own server must not keep that server's key.
        let mut bare = base.clone();
        bare["high"] = json!({"model": "huger", "key": "", "has_key": true});
        put(bare).await;
        assert_eq!(shared.get().high.key, "");
        let saved = f.api.runner.store().setting("model").unwrap().unwrap();
        assert!(saved.contains("huger"));
        let mut efforts = base.clone();
        efforts["effort"] = json!("Low");
        efforts["high"] = json!({"model": "huger", "effort": "medium"});
        efforts["low"] = json!({"model": "tiny", "effort": "turbo"});
        let (s, m) = put(efforts).await;
        assert_eq!(s, StatusCode::OK, "{m}");
        assert_eq!(m["effort"], "low");
        assert_eq!(m["high"]["effort"], "medium");
        assert_eq!(m["low"]["effort"], "", "an unknown effort is dropped");
        assert_eq!(shared.get().level("high").unwrap().effort, "medium");
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
        let sid = sid(&session);
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
        assert_eq!(trees[1]["owned"], true);
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
            speech: Default::default(),
            voice: Default::default(),
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
        call(&app, "POST", &format!("/api/sessions/{}/interrupt", sid(&sess)), None, T).await;
        settle(&api, sid(&sess)).await;
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
