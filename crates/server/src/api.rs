use crate::projects;
use crate::runner::Runner;
use crate::store::StoreError;
use agentbench_core::{
    Answer, ApiError, Id, IssuePatch, IssueState, NewComment, NewIssue, NewProject, NewSession,
    NewWorktree, Overview, ProjectPatch, SendMessage, SessionId,
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
    // Fold the length difference in so the comparison time does not depend on
    // the token length either.
    let mut acc = a.len() ^ b.len();
    for (x, y) in a.iter().zip(b) {
        acc |= (x ^ y) as usize;
    }
    acc == 0
}

async fn nosniff(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, header::HeaderValue::from_static("nosniff"));
    res
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
    note_client(&headers);
    next.run(req).await
}

fn note_client(headers: &HeaderMap) {
    static WARNED: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
    let Some(v) = headers
        .get(agentbench_core::API_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u32>().ok())
    else {
        return;
    };
    if v < agentbench_core::MIN_CLIENT_API {
        let mut seen = WARNED.lock().unwrap_or_else(|e| e.into_inner());
        if !seen.contains(&v) {
            seen.push(v);
            tracing::warn!(
                "a client speaking api v{v} connected; this server needs v{} or newer, so that app must be updated",
                agentbench_core::MIN_CLIENT_API
            );
        }
    }
}

async fn api_header(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut().insert(
        agentbench_core::API_HEADER,
        header::HeaderValue::from(agentbench_core::API_VERSION),
    );
    res
}

/// Room for a message with every attachment it may carry, base64 encoded.
const BODY_MAX: usize =
    agentbench_core::ATTACHMENTS_MAX * agentbench_core::ATTACHMENT_MAX * 4 / 3 + (1 << 20);

pub fn router(api: Api) -> Router {
    let protected = Router::new()
        .route("/api/overview", get(overview))
        .route("/api/usage", get(usage))
        .route("/api/changes", get(changes))
        .route("/api/agents", get(agents))
        .route("/api/settings/model", get(get_model).put(put_model))
        .route("/api/settings/models", post(list_models))
        .route("/api/onboarding", get(get_onboarding).patch(patch_onboarding))
        .route("/api/onboarding/providers", get(get_providers).put(put_providers))
        .route("/api/onboarding/test", post(test_provider))
        .route("/api/onboarding/models", post(provider_models))
        .route("/api/onboarding/health", get(provider_health))
        .route("/api/onboarding/login", post(start_login))
        .route("/api/onboarding/login/screen", get(login_screen).post(login_input).delete(login_close))
        .route("/api/settings/cross-project", get(get_cross).delete(clear_cross))
        .route("/api/settings/autorun", get(get_autorun).put(put_autorun))
        .route("/api/global-notes", get(get_global_notes).put(put_global_note))
        .route("/api/autopilot", get(get_autopilot).patch(patch_autopilot))
        .route("/api/triage", get(get_triage).patch(patch_triage))
        .route("/api/triage/run", post(run_triage))
        .route("/api/triage/{id}/accept", post(accept_proposal))
        .route("/api/triage/{id}/reject", post(reject_proposal))
        .route("/api/settings/title", get(get_title).put(put_title))
        .route("/api/settings/supervisor-model", get(get_supervisor_model).put(put_supervisor_model))
        .route("/api/settings/title/models", post(list_title_models))
        .route("/api/agents/{name}/models", get(agent_models))
        .route("/api/projects", get(list_projects).post(add_project))
        .route("/api/projects/{id}", delete(delete_project).patch(update_project))
        .route("/api/projects/{id}/sync", post(sync_project))
        .route("/api/projects/{id}/github", get(github_status))
        .route("/api/projects/{id}/github/webhook", post(webhook_secret))
        .route("/api/projects/{id}/outbox/approve-all", post(outbox_approve_all))
        .route("/api/projects/{id}/github/webhook/create", post(webhook_create))
        .route("/api/settings/github", get(get_github_settings).put(put_github_settings))
        .route("/api/projects/{id}/github/sync", post(github_sync_now))
        .route("/api/projects/{id}/pulls", get(pulls_list))
        .route("/api/projects/{id}/pulls/{number}/review", post(pull_review))
        .route("/api/projects/{id}/pulls/{number}/merge", post(pull_merge))
        .route("/api/outbox", get(outbox_list))
        .route("/api/outbox/{id}/approve", post(outbox_approve))
        .route("/api/outbox/{id}/reject", post(outbox_reject))
        .route("/api/github/pause", post(github_pause))
        .route("/api/projects/{id}/notes", get(list_notes))
        .route("/api/projects/{id}/prompts", get(list_prompts))
        .route("/api/projects/{id}/notes/restore", post(restore_note))
        .route("/api/memory/compact", post(compact_memory))
        .route("/api/memory/restore", post(restore_memory))
        .route("/api/settings/memory", get(get_memory_settings).put(put_memory_settings))
        .route("/api/projects/{id}/vision", get(get_vision).put(put_vision))
        .route("/api/projects/{id}/vision/versions", get(vision_history))
        .route("/api/projects/{id}/vision/restore", post(restore_vision))
        .route("/api/projects/{id}/icon", get(project_icon))
        .route("/api/projects/{id}/worktrees", get(list_worktrees).post(add_worktree))
        .route("/api/worktrees/{id}", delete(remove_worktree))
        .route("/api/issues", get(issues).post(add_issue))
        .route("/api/issues/{id}", get(issue).patch(update_issue).delete(delete_issue))
        .route("/api/issues/{id}/comments", get(comments).post(add_comment))
        .route("/api/issues/{id}/deps", get(issue_deps).put(set_issue_deps))
        .route("/api/issues/{id}/start", post(start_issue))
        .route("/api/sessions", get(sessions).post(add_session))
        .route("/api/supervisor", post(open_supervisor))
        .route("/api/sessions/{id}", get(session).patch(rename_session).delete(delete_session))
        .route("/api/sessions/{id}/messages", get(messages).post(send))
        .route("/api/sessions/{id}/interrupt", post(interrupt))
        .route("/api/sessions/{id}/release", post(release))
        .route("/api/sessions/{id}/adopt", post(adopt))
        .route(
            "/api/sessions/{id}/terminal",
            get(terminal_screen).post(terminal_input).delete(terminal_close),
        )
        .route("/api/sessions/{id}/jobs", get(jobs))
        .route("/api/sessions/{id}/jobs/{handle}/kill", post(kill_job))
        .route("/api/sessions/{id}/answer", post(answer))
        .route("/v1/audio/transcriptions", post(transcribe))
        .route("/v1/audio/speech", post(speak))
        .route("/v1/audio/voices", get(voices))
        .route("/v1/audio/models", get(speech_models))
        .route("/v1/audio/status", get(audio_status))
        .route("/api/sessions/{id}/attachments/{file}", get(attachment))
        .layer(axum::extract::DefaultBodyLimit::max(BODY_MAX))
        .layer(tower_http::compression::CompressionLayer::new().gzip(true))
        .layer(middleware::from_fn_with_state(api.clone(), auth));
    Router::new()
        .route("/health", get(|| async { Json(json!({ "ok": true })) }))
        .route(
            "/api/github/webhook/{id}",
            post(github_webhook).layer(axum::extract::DefaultBodyLimit::max(crate::github::webhook::BODY_MAX)),
        )
        .merge(protected)
        .layer(middleware::from_fn(nosniff))
        .layer(middleware::from_fn(api_header))
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
    let (pcm, rate) = agent_voice::voice::audio::read_wav(&wav).map_err(bad)?;
    let pcm = agent_voice::voice::audio::to_whisper_rate(&pcm, rate);
    let id = api.speech.pick(&asked);
    let started = std::time::Instant::now();
    let (text, on) = api
        .speech
        .read(&id, pcm.clone())
        .await
        .map_err(|e| Failure(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    tracing::info!(
        "read {:.1}s of speech with {id} on the {on} in {:.1}s",
        pcm.len() as f32 / agent_voice::voice::audio::SPEECH_RATE as f32,
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
    let wav = agent_voice::voice::audio::wav_bytes(&said.samples, said.rate);
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

fn usage_report(api: &Api) -> Result<agentbench_core::UsageReport, StoreError> {
    let now = crate::quota::now();
    Ok(agentbench_core::UsageReport {
        spend: api.runner.store().spend(now)?,
        quotas: api.runner.quotas.all().into_iter().filter(|q| !q.expired(now)).collect(),
    })
}

async fn usage(State(api): State<Api>) -> Reply<agentbench_core::UsageReport> {
    Ok(Json(usage_report(&api)?))
}

async fn overview(State(api): State<Api>) -> Reply<Overview> {
    let store = api.runner.store();
    let cursor = store.cursor()?;
    let hidden = store.supervisor_project()?.map(|p| p.id);
    Ok(Json(Overview {
        version: env!("CARGO_PKG_VERSION").to_string(),
        api_version: agentbench_core::API_VERSION,
        min_client_api: agentbench_core::MIN_CLIENT_API,
        agents: api.runner.agents().iter().map(|a| a.info()).collect(),
        projects: store.projects()?.into_iter().filter(|p| !p.supervisor).collect(),
        worktrees: store
            .worktrees(None)?
            .into_iter()
            .filter(|w| Some(w.project_id) != hidden)
            .collect(),
        issues: store.issues()?,
        sessions: api.runner.with_queued(store.sessions()?),
        supervisor: api.runner.supervisor_session().map(|s| s.id),
        autopilot: Some(crate::autopilot::state(store)),
        triage: Some(crate::triage::state(store)),
        visions: store.visions()?,
        usage: Some(usage_report(&api)?),
        host: Some(crate::host::sample()),
        cursor,
    }))
}

#[derive(Deserialize)]
struct Since {
    #[serde(default)]
    since: Id,
    #[serde(default)]
    wait: Option<u64>,
    #[serde(default)]
    kinds: Option<String>,
    #[serde(default)]
    epoch: Option<u64>,
}

const ATTENTION_SETTLE: Duration = Duration::from_millis(400);

async fn attention_feed(api: &Api, q: &Since) -> Result<serde_json::Value, StoreError> {
    let store = api.runner.store();
    let wait = Duration::from_secs(q.wait.unwrap_or(0)).min(LONG_POLL);
    let deadline = tokio::time::Instant::now() + wait;
    let since = q.since.max(0) as u64;
    loop {
        let notified = api.runner.changed.notified();
        api.runner.attention.refresh(store.attention_rows()?);
        let feed = api.runner.attention.feed(since, q.epoch);
        if feed.wakes() || tokio::time::Instant::now() >= deadline {
            return Ok(feed.json());
        }
        if tokio::time::timeout_at(deadline, notified).await.is_ok() {
            tokio::time::sleep(ATTENTION_SETTLE.min(deadline - tokio::time::Instant::now())).await;
        }
    }
}

async fn changes(State(api): State<Api>, Query(q): Query<Since>) -> Reply<serde_json::Value> {
    if q.kinds.as_deref().map(|k| k.split(',').any(|k| k.trim() == "attention")).unwrap_or(false) {
        return Ok(Json(attention_feed(&api, &q).await?));
    }

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
        fold_idle_secs: m.fold_idle_secs,
        fold_at: m.fold_at,
        fold_keep: m.fold_keep,
        low: level_masked(&m.low),
        medium: level_masked(&m.medium),
        high: level_masked(&m.high),
        effort: m.effort.clone(),
        max_tokens: m.max_tokens,
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
    m.fold_idle_secs = new.fold_idle_secs.min(3600);
    m.fold_at = new.fold_at.min(100);
    m.fold_keep = new.fold_keep.clamp(10, 90);
    m.effort = clean_effort(&new.effort);
    m.max_tokens = new.max_tokens.min(10_000_000);
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

async fn get_supervisor_model(State(api): State<Api>) -> Reply<serde_json::Value> {
    let model = api
        .runner
        .store()
        .setting(crate::providers::SUPERVISOR_SETTING)?
        .unwrap_or_default();
    Ok(Json(json!({ "model": model, "default": crate::providers::SUPERVISOR_DEFAULT })))
}

async fn put_supervisor_model(
    State(api): State<Api>,
    Json(body): Json<serde_json::Value>,
) -> Reply<serde_json::Value> {
    let model = body["model"].as_str().unwrap_or_default().trim();
    let store = api.runner.store();
    if model.is_empty() {
        store.delete_setting(crate::providers::SUPERVISOR_SETTING)?;
    } else {
        store.set_setting(crate::providers::SUPERVISOR_SETTING, model)?;
    }
    changed(&api);
    get_supervisor_model(State(api)).await
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

fn saved_with_keys(api: &Api, mut p: agentbench_core::Provider) -> agentbench_core::Provider {
    if p.key.trim().is_empty() && p.has_key {
        let old = crate::providers::saved(api.runner.store());
        p.key = old.iter().find(|o| o.kind == p.kind).map(|o| o.key.clone()).unwrap_or_default();
    }
    p
}

async fn get_onboarding(State(api): State<Api>) -> Reply<agentbench_core::Onboarding> {
    let store = api.runner.store().clone();
    let model_set = api.model.as_ref().is_some_and(|m| !m.get().model.trim().is_empty());
    let (claude, codex) = tokio::task::spawn_blocking(|| {
        (crate::providers::detect_claude(), crate::providers::detect_codex())
    })
    .await
    .map_err(|e| bad(e.to_string()))?;
    Ok(Json(crate::providers::onboarding(&store, model_set, claude, codex)))
}

async fn patch_onboarding(
    State(api): State<Api>,
    Json(patch): Json<agentbench_core::OnboardingPatch>,
) -> Result<StatusCode, Failure> {
    let store = api.runner.store();
    let status = patch.status.unwrap_or_else(|| crate::providers::status(store));
    crate::providers::set_status(store, &status, patch.step.as_deref())?;
    changed(&api);
    Ok(StatusCode::NO_CONTENT)
}

async fn get_providers(State(api): State<Api>) -> Reply<Vec<agentbench_core::Provider>> {
    Ok(Json(crate::providers::masked(&crate::providers::saved(api.runner.store()))))
}

async fn put_providers(
    State(api): State<Api>,
    Json(new): Json<Vec<agentbench_core::Provider>>,
) -> Reply<Vec<agentbench_core::Provider>> {
    let store = api.runner.store();
    let old = crate::providers::saved(store);
    let list = crate::providers::merged(&old, new);
    crate::providers::save(store, &list)?;
    crate::providers::apply(store, api.model.as_ref())?;
    changed(&api);
    Ok(Json(crate::providers::masked(&list)))
}

async fn test_provider(
    State(api): State<Api>,
    Json(p): Json<agentbench_core::Provider>,
) -> Reply<agentbench_core::ProviderCheck> {
    let p = saved_with_keys(&api, p);
    Ok(Json(crate::providers::test(&p).await))
}

async fn provider_models(
    State(api): State<Api>,
    Json(p): Json<agentbench_core::Provider>,
) -> Reply<Vec<String>> {
    let p = saved_with_keys(&api, p);
    Ok(Json(crate::providers::models(&p).await.map_err(bad)?))
}

async fn provider_health(State(api): State<Api>) -> Reply<Vec<agentbench_core::ProviderHealth>> {
    let list = crate::providers::saved(api.runner.store());
    let got = crate::providers::health(list).await;
    Ok(Json(
        got.into_iter()
            .map(|(kind, c)| agentbench_core::ProviderHealth { kind, ok: c.ok, message: c.message })
            .collect(),
    ))
}

const LOGIN_TERMINAL: SessionId = SessionId(uuid::Uuid::from_u128(0x1844_0000_0000_4000_8000_0000_0000_0184));

async fn start_login(
    State(api): State<Api>,
    Json(start): Json<agentbench_core::LoginStart>,
) -> Result<StatusCode, Failure> {
    let (program, args) = crate::providers::login_command(&start.cli).ok_or_else(|| {
        bad(format!("{} is not installed on this machine", start.cli))
    })?;
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    api.runner.terminals.close(LOGIN_TERMINAL);
    api.runner
        .terminals
        .open_command(LOGIN_TERMINAL, &home, Some(&program.display().to_string()), &args)
        .map_err(|e| Failure(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn login_screen(State(api): State<Api>) -> Reply<agentbench_core::Screen> {
    api.runner
        .terminals
        .screen(LOGIN_TERMINAL)
        .map(Json)
        .ok_or_else(|| Failure(StatusCode::NOT_FOUND, "no login is running".into()))
}

async fn login_input(
    State(api): State<Api>,
    Json(input): Json<agentbench_core::TerminalInput>,
) -> Result<StatusCode, Failure> {
    let t = &api.runner.terminals;
    if input.rows > 0 && input.cols > 0 {
        t.resize(LOGIN_TERMINAL, input.rows, input.cols).map_err(bad)?;
    }
    if !input.data.is_empty() {
        t.write(LOGIN_TERMINAL, input.data.as_bytes()).map_err(bad)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn login_close(State(api): State<Api>) -> Result<StatusCode, Failure> {
    api.runner.terminals.close(LOGIN_TERMINAL);
    Ok(StatusCode::NO_CONTENT)
}

async fn list_projects(State(api): State<Api>) -> Reply<Vec<agentbench_core::Project>> {
    Ok(Json(api.runner.store().projects()?.into_iter().filter(|p| !p.supervisor).collect()))
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

async fn update_project(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(patch): Json<ProjectPatch>,
) -> Reply<agentbench_core::Project> {
    let project = crate::autopilot::set_project(api.runner.store(), id, &patch)?;
    changed(&api);
    Ok(Json(project))
}

#[derive(Deserialize, Default)]
struct OutboxApprove {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize, Default)]
struct Paused {
    #[serde(default)]
    paused: bool,
}

async fn outbox_list(State(api): State<Api>, Query(q): Query<OutboxQuery>) -> Reply<agentbench_core::OutboxList> {
    let store = api.runner.store();
    Ok(Json(agentbench_core::OutboxList {
        paused: crate::github::outbox::is_paused(store),
        items: crate::github::outbox::linked(store, q.project)?,
    }))
}

#[derive(Deserialize, Default)]
struct OutboxQuery {
    #[serde(default)]
    project: Option<Id>,
}

async fn webhook_secret(State(api): State<Api>, Path(id): Path<Id>) -> Reply<agentbench_core::WebhookSecret> {
    let made = crate::github::webhook::rotate(api.runner.store(), id).map_err(bad)?;
    changed(&api);
    Ok(Json(made))
}

async fn webhook_create(State(api): State<Api>, Path(id): Path<Id>) -> Reply<serde_json::Value> {
    let store = api.runner.store().clone();
    let done = tokio::task::spawn_blocking(move || crate::github::webhook::create(&store, &crate::github::gh_program(), id))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(bad)?;
    Ok(Json(done))
}

fn github_settings_now(store: &crate::store::Store) -> agentbench_core::GithubSettings {
    agentbench_core::GithubSettings {
        poll_secs: Some(crate::github::poll_secs(store)),
        webhook_base: crate::github::webhook::base(store),
    }
}

async fn get_github_settings(State(api): State<Api>) -> Reply<agentbench_core::GithubSettings> {
    Ok(Json(github_settings_now(api.runner.store())))
}

async fn put_github_settings(State(api): State<Api>, Json(s): Json<agentbench_core::GithubSettings>) -> Reply<agentbench_core::GithubSettings> {
    let store = api.runner.store();
    if let Some(p) = s.poll_secs {
        if p < 5 {
            return Err(bad("the poll interval is at least 5 seconds"));
        }
        store.set_setting(crate::github::POLL_SETTING, &p.to_string())?;
    }
    if let Some(b) = s.webhook_base {
        let b = b.trim();
        if !b.is_empty() && !(b.starts_with("https://") || b.starts_with("http://")) {
            return Err(bad("the webhook base is an http or https address"));
        }
        store.set_setting(crate::github::webhook::BASE_SETTING, b)?;
    }
    changed(&api);
    Ok(Json(github_settings_now(store)))
}

async fn github_webhook(State(api): State<Api>, Path(id): Path<Id>, headers: HeaderMap, body: axum::body::Bytes) -> StatusCode {
    use crate::github::webhook::{Delivery, Received, receive};
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let (event, delivery, signature) = (header("x-github-event"), header("x-github-delivery"), header("x-hub-signature-256"));
    let store = api.runner.store().clone();
    let got = tokio::task::spawn_blocking(move || {
        let d = Delivery { event: event.as_deref(), id: delivery.as_deref(), signature: signature.as_deref(), body: &body };
        receive(&store, &crate::github::gh_program(), id, &d, crate::store::now())
    })
    .await;
    match got {
        Ok(Received::Unauthorized) => StatusCode::UNAUTHORIZED,
        Ok(Received::Refetched) => {
            changed(&api);
            StatusCode::NO_CONTENT
        }
        Ok(Received::Ignored(_)) => StatusCode::ACCEPTED,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn outbox_approve(State(api): State<Api>, Path(id): Path<Id>, body: Option<Json<OutboxApprove>>) -> Reply<serde_json::Value> {
    let text = body.and_then(|b| b.0.text);
    crate::github::outbox::approve(api.runner.store(), id, text.as_deref(), "user").map_err(bad)?;
    changed(&api);
    Ok(Json(json!({ "approved": id })))
}

async fn outbox_approve_all(State(api): State<Api>, Path(id): Path<Id>) -> Reply<serde_json::Value> {
    let done = crate::github::outbox::approve_all(api.runner.store(), id, "user").map_err(bad)?;
    changed(&api);
    Ok(Json(json!({ "approved": done })))
}

async fn outbox_reject(State(api): State<Api>, Path(id): Path<Id>) -> Reply<serde_json::Value> {
    crate::github::outbox::reject(api.runner.store(), id).map_err(bad)?;
    changed(&api);
    Ok(Json(json!({ "rejected": id })))
}

async fn github_pause(State(api): State<Api>, body: Option<Json<Paused>>) -> Reply<serde_json::Value> {
    let paused = body.map(|b| b.0.paused).unwrap_or(true);
    crate::github::outbox::set_paused(api.runner.store(), paused).map_err(bad)?;
    changed(&api);
    Ok(Json(json!({ "paused": paused })))
}

async fn pulls_list(State(api): State<Api>, Path(id): Path<Id>) -> Reply<Vec<agentbench_core::PullRequest>> {
    Ok(Json(api.runner.store().pull_requests(id)?))
}

async fn pull_review(State(api): State<Api>, Path((id, number)): Path<(Id, i64)>) -> Reply<serde_json::Value> {
    let store = api.runner.store();
    store.pull_request(id, number)?.ok_or_else(|| Failure(StatusCode::NOT_FOUND, format!("no mirrored pull request #{number}")))?;
    store.ask_pr_review(id, number)?;
    changed(&api);
    Ok(Json(json!({ "review_asked": number })))
}

async fn pull_merge(State(api): State<Api>, Path((id, number)): Path<(Id, i64)>) -> Reply<serde_json::Value> {
    let store = api.runner.store().clone();
    let done = tokio::task::spawn_blocking(move || {
        crate::github::merge::merge(&store, &crate::github::gh_program(), id, number, crate::github::merge::OWNER)
    })
    .await
    .map_err(|e| bad(e.to_string()))?
    .map_err(bad)?;
    changed(&api);
    Ok(Json(json!({ "merged": number, "result": done })))
}

async fn github_sync_now(State(api): State<Api>, Path(id): Path<Id>) -> Reply<agentbench_core::GithubStatus> {
    let store = api.runner.store().clone();
    let project = store.project(id)?;
    if !project.tracker.is_github() {
        return Err(bad("this project does not use GitHub"));
    }
    let s = store.clone();
    tokio::task::spawn_blocking(move || {
        crate::github::sync_project(&s, &crate::github::gh_program(), id, crate::store::now(), true)
    })
    .await
    .map_err(|e| bad(e.to_string()))?;
    changed(&api);
    github_status(State(api), Path(id)).await
}

async fn github_status(State(api): State<Api>, Path(id): Path<Id>) -> Reply<agentbench_core::GithubStatus> {
    let store = api.runner.store().clone();
    let project = store.project(id)?;
    let last_sync = store.setting(&format!("github_last_sync_{id}")).ok().flatten().and_then(|v| v.parse().ok());
    let status = tokio::task::spawn_blocking(move || {
        crate::github::status(&store, &crate::github::gh_program(), &project, last_sync)
    })
    .await
    .map_err(|e| bad(e.to_string()))?;
    Ok(Json(status))
}

async fn project_icon(State(api): State<Api>, Path(id): Path<Id>) -> Result<Response, Failure> {
    let project = api.runner.store().project(id)?;
    let path = std::path::PathBuf::from(project.path);
    let found = tokio::task::spawn_blocking(move || crate::projects::icon(&path))
        .await
        .map_err(|e| Failure(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let Some((mime, bytes)) = found else {
        return Err(Failure(StatusCode::NOT_FOUND, "no icon".into()));
    };
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, mime.to_string()),
            (axum::http::header::CACHE_CONTROL, "private, max-age=600".to_string()),
        ],
        bytes,
    )
        .into_response())
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

async fn list_notes(
    State(api): State<Api>,
    Path(id): Path<Id>,
) -> Reply<Vec<agentbench_core::NoteHistory>> {
    let store = api.runner.store();
    store.project(id)?;
    let current = store.notes(id)?;
    let mut out = Vec::new();
    for topic in store.note_topics(id)? {
        let body = current.iter().find(|(t, _)| *t == topic).map(|(_, b)| b.clone());
        out.push(agentbench_core::NoteHistory { topic, current: body, versions: Vec::new() });
    }
    Ok(Json(out))
}

#[derive(serde::Deserialize)]
struct RestoreNote {
    version: Id,
}

async fn restore_note(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(req): Json<RestoreNote>,
) -> Reply<()> {
    api.runner.store().restore_note(id, req.version)?;
    changed(&api);
    Ok(Json(()))
}

async fn list_prompts(
    State(api): State<Api>,
    Path(id): Path<Id>,
) -> Reply<Vec<agentbench_core::prompts::Template>> {
    let project = api.runner.store().project(id)?;
    let home = dirs::home_dir().unwrap_or_default();
    Ok(Json(agentbench_core::prompts::load(&home, Some(std::path::Path::new(&project.path)))))
}

async fn compact_memory(
    State(api): State<Api>,
    Json(req): Json<agentbench_core::MemoryCompactRequest>,
) -> Reply<agentbench_core::MemoryCompaction> {
    let store = api.runner.store();
    let (project, global) = match req.scope.trim().to_lowercase().as_str() {
        "" | "project" => (Some(req.project.ok_or_else(|| bad("name the project"))?), false),
        "global" => (None, true),
        "all" => (None, true),
        other => return Err(bad(format!("{other} is not a scope: project, global or all"))),
    };
    let mut scopes = crate::compact::scopes(store, project, global);
    if req.scope.trim().eq_ignore_ascii_case("global") {
        scopes.retain(|(s, _)| *s == crate::memory::Scope::Global);
    }
    let target = crate::compact::target(&api.runner).map_err(bad)?;
    let done = crate::compact::run(store, &target, scopes, true).await;
    changed(&api);
    Ok(Json(done))
}

#[derive(serde::Deserialize)]
struct RestoreMemory {
    #[serde(default)]
    project: Option<Id>,
    version: String,
}

async fn restore_memory(State(api): State<Api>, Json(req): Json<RestoreMemory>) -> Reply<()> {
    let scope = match req.project {
        Some(id) => crate::memory::Scope::Project(id),
        None => crate::memory::Scope::Global,
    };
    api.runner.store().memory_store().restore(scope, &req.version).map_err(bad)?;
    changed(&api);
    Ok(Json(()))
}

async fn get_memory_settings(State(api): State<Api>) -> Reply<agentbench_core::MemorySettings> {
    Ok(Json(crate::compact::settings(api.runner.store())))
}

async fn put_memory_settings(
    State(api): State<Api>,
    Json(new): Json<agentbench_core::MemorySettings>,
) -> Reply<agentbench_core::MemorySettings> {
    Ok(Json(crate::compact::save_settings(api.runner.store(), new).map_err(bad)?))
}

async fn get_vision(
    State(api): State<Api>,
    Path(id): Path<Id>,
) -> Reply<Option<agentbench_core::Vision>> {
    let store = api.runner.store();
    store.project(id)?;
    Ok(Json(store.vision(id)?))
}

async fn put_vision(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(body): Json<agentbench_core::VisionBody>,
) -> Reply<agentbench_core::Vision> {
    let v = api.runner.store().set_vision(id, body, "user")?;
    changed(&api);
    Ok(Json(v))
}

async fn vision_history(
    State(api): State<Api>,
    Path(id): Path<Id>,
) -> Reply<Vec<agentbench_core::Vision>> {
    let store = api.runner.store();
    store.project(id)?;
    Ok(Json(store.vision_versions(id)?))
}

#[derive(serde::Deserialize)]
struct RestoreVision {
    version: Id,
}

async fn restore_vision(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(req): Json<RestoreVision>,
) -> Reply<agentbench_core::Vision> {
    let store = api.runner.store();
    let old = store
        .vision_versions(id)?
        .into_iter()
        .find(|v| v.id == req.version)
        .ok_or_else(|| bad(format!("no vision version {}", req.version)))?;
    let v = store.set_vision(id, old.body, "user")?;
    changed(&api);
    Ok(Json(v))
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
    let before = api.runner.store().issue(id)?;
    let mut issue = api.runner.store().update_issue(id, &patch)?;
    if issue.state != before.state {
        issue = moved(&api, issue).await?;
    }
    changed(&api);
    Ok(Json(issue))
}

async fn issue_deps(State(api): State<Api>, Path(id): Path<Id>) -> Reply<serde_json::Value> {
    Ok(Json(deps_json(&api.runner.store().issue(id)?)))
}

fn deps_json(i: &agentbench_core::Issue) -> serde_json::Value {
    json!({ "depends_on": i.depends_on, "waiting_on": i.waiting_on, "blocks": i.blocks })
}

async fn set_issue_deps(
    State(api): State<Api>,
    Path(id): Path<Id>,
    Json(deps): Json<Vec<Id>>,
) -> Reply<serde_json::Value> {
    api.runner.store().set_deps(id, &deps)?;
    changed(&api);
    Ok(Json(deps_json(&api.runner.store().issue(id)?)))
}

async fn get_autopilot(State(api): State<Api>) -> Reply<agentbench_core::AutopilotState> {
    Ok(Json(crate::autopilot::state(api.runner.store())))
}

async fn patch_autopilot(
    State(api): State<Api>,
    Json(patch): Json<agentbench_core::AutopilotPatch>,
) -> Reply<agentbench_core::AutopilotState> {
    let state = crate::autopilot::apply(api.runner.store(), &patch)?;
    changed(&api);
    Ok(Json(state))
}

async fn get_triage(State(api): State<Api>) -> Reply<agentbench_core::TriageState> {
    Ok(Json(crate::triage::state(api.runner.store())))
}

async fn patch_triage(
    State(api): State<Api>,
    Json(patch): Json<agentbench_core::TriagePatch>,
) -> Reply<agentbench_core::TriageState> {
    let state = crate::triage::apply(api.runner.store(), &patch)?;
    changed(&api);
    Ok(Json(state))
}

async fn run_triage(State(api): State<Api>) -> Reply<agentbench_core::TriageState> {
    if api.runner.titles().and_then(|t| t.target()).is_none() {
        return Err(bad("no model to triage with: set one under settings"));
    }
    if !crate::triage::start(api.runner.clone()) {
        return Err(bad("a triage run is already going"));
    }
    Ok(Json(crate::triage::state(api.runner.store())))
}

async fn accept_proposal(
    State(api): State<Api>,
    Path(id): Path<Id>,
) -> Reply<agentbench_core::Proposal> {
    let store = api.runner.store();
    let proposal = store.proposal(id)?;
    if proposal.state != agentbench_core::ProposalState::Pending {
        return Err(bad("that proposal was already decided"));
    }
    if crate::add_project::request_of(&proposal).is_some() {
        return Ok(Json(decide_add_project(&api, id, true).await?));
    }
    for change in &proposal.changes {
        match change {
            agentbench_core::Change::Update { issue_id, patch } => {
                let before = store.issue(*issue_id)?;
                let mut issue = store.update_issue(*issue_id, patch)?;
                if issue.state != before.state {
                    issue = moved(&api, issue).await?;
                }
                let _ = issue;
            }
            agentbench_core::Change::Comment { issue_id, body } => {
                store.add_comment(
                    *issue_id,
                    &NewComment { author: Some("triage".into()), body: body.clone() },
                )?;
            }
            agentbench_core::Change::Create { issue } => {
                store.add_issue(issue)?;
            }
            agentbench_core::Change::AddProject { .. } => {}
            agentbench_core::Change::GlobalNote { topic, note } => {
                store.set_global_note(topic, note)?;
                crate::compact::after_write(&api.runner, crate::memory::Scope::Global);
            }
        }
    }
    let decided = store.decide_proposal(id, agentbench_core::ProposalState::Accepted)?;
    changed(&api);
    Ok(Json(decided))
}

async fn reject_proposal(
    State(api): State<Api>,
    Path(id): Path<Id>,
) -> Reply<agentbench_core::Proposal> {
    if crate::add_project::request_of(&api.runner.store().proposal(id)?).is_some() {
        return Ok(Json(decide_add_project(&api, id, false).await?));
    }
    let decided =
        api.runner.store().decide_proposal(id, agentbench_core::ProposalState::Rejected)?;
    changed(&api);
    Ok(Json(decided))
}

async fn decide_add_project(
    api: &Api,
    id: Id,
    accept: bool,
) -> Result<agentbench_core::Proposal, Failure> {
    let store = api.runner.store();
    let request = crate::add_project::request_of(&store.proposal(id)?).cloned();
    let home = dirs::home_dir().unwrap_or_default();
    let outcome = {
        let (store, home) = (store.clone(), home.clone());
        tokio::task::spawn_blocking(move || crate::add_project::decide(&store, &home, id, accept))
            .await
            .map_err(|e| bad(e.to_string()))?
    }
    .map_err(bad)?;
    if let Some(session) = request.and_then(|r| r.requester).and_then(|s| store.session(s).ok()) {
        let note = crate::add_project::requester_message(&outcome);
        let _ = api.runner.tell(&session, &note, crate::runner::Origin::new("add_project", None));
    }
    changed(api);
    Ok(store.proposal(id)?)
}

const AUTORUN_AGENT: &str = "native";

async fn get_global_notes(State(api): State<Api>) -> Reply<serde_json::Value> {
    let notes = api.runner.store().global_notes()?;
    let notes: Vec<_> = notes.into_iter().map(|(t, n)| json!({ "topic": t, "note": n })).collect();
    Ok(Json(json!(notes)))
}

async fn put_global_note(
    State(api): State<Api>,
    Json(body): Json<serde_json::Value>,
) -> Reply<serde_json::Value> {
    let topic = body["topic"].as_str().map(str::trim).filter(|t| !t.is_empty());
    let topic = topic.ok_or_else(|| bad("missing `topic`"))?;
    let note = body["note"].as_str().unwrap_or_default();
    api.runner.store().set_global_note(topic, note)?;
    crate::compact::after_write(&api.runner, crate::memory::Scope::Global);
    changed(&api);
    get_global_notes(State(api)).await
}

async fn get_autorun(State(api): State<Api>) -> Reply<serde_json::Value> {
    let store = api.runner.store();
    let enabled = store.setting("autorun")?.as_deref() == Some("1");
    let agent = store.setting("autorun_agent")?.unwrap_or_else(|| AUTORUN_AGENT.into());
    Ok(Json(json!({ "enabled": enabled, "agent": agent })))
}

async fn put_autorun(
    State(api): State<Api>,
    Json(body): Json<serde_json::Value>,
) -> Reply<serde_json::Value> {
    let store = api.runner.store();
    let enabled = body["enabled"].as_bool().unwrap_or(false);
    store.set_setting("autorun", if enabled { "1" } else { "0" })?;
    if let Some(agent) = body["agent"].as_str().filter(|a| !a.trim().is_empty()) {
        store.set_setting("autorun_agent", agent.trim())?;
    }
    get_autorun(State(api)).await
}

async fn moved(
    api: &Api,
    issue: agentbench_core::Issue,
) -> Result<agentbench_core::Issue, Failure> {
    let store = api.runner.store();
    if store.setting("autorun")?.as_deref() != Some("1") {
        return Ok(issue);
    }
    match issue.state {
        IssueState::Review => {
            crate::tools::desktop_notice(
                "ready for review",
                &format!("#{} {}", issue.id, issue.title),
            );
            Ok(issue)
        }
        IssueState::Todo => {
            let running = store.sessions()?.into_iter().any(|s| {
                s.issue_id == Some(issue.id) && s.status != agentbench_core::SessionStatus::Stopped
            });
            if running {
                return Ok(issue);
            }
            let agent = store.setting("autorun_agent")?.unwrap_or_else(|| AUTORUN_AGENT.into());
            let new = NewSession {
                project_id: issue.project_id,
                agent,
                place: agentbench_core::Place::New { branch: String::new() },
                issue_id: Some(issue.id),
                ..Default::default()
            };
            match spawn(api, new).await {
                Ok(_) => Ok(store.update_issue(
                    issue.id,
                    &IssuePatch { state: Some(IssueState::Doing), ..Default::default() },
                )?),
                Err(e) => {
                    store.add_comment(
                        issue.id,
                        &agentbench_core::NewComment {
                            author: Some("agentbench".into()),
                            body: format!("auto-run could not start an agent: {}", e.1),
                        },
                    )?;
                    Ok(issue)
                }
            }
        }
        _ => Ok(issue),
    }
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

async fn rename_session(
    State(api): State<Api>,
    Path(id): Path<SessionId>,
    Json(patch): Json<agentbench_core::SessionPatch>,
) -> Reply<agentbench_core::Session> {
    if let Some(title) = &patch.title {
        api.runner.store().set_title(id, title, true)?;
        changed(&api);
    }
    if let Some(on) = patch.queue_system {
        api.runner.store().set_queue_system(id, on)?;
        changed(&api);
    }
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

#[derive(Deserialize)]
struct OpenSupervisor {
    agent: String,
}

async fn open_supervisor(
    State(api): State<Api>,
    Json(open): Json<OpenSupervisor>,
) -> Reply<agentbench_core::Session> {
    let runner = api.runner.clone();
    let session = tokio::task::spawn_blocking(move || runner.open_supervisor(&open.agent))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(Failure::from)?;
    Ok(Json(session))
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
    let tree = api.runner.store().session(id).map(|s| s.worktree_id);
    api.runner.store().delete_session(id)?;
    if let Ok(tree) = tree {
        let _ = crate::projects::tidy_worktree(api.runner.store(), tree, false, None);
    }
    let _ = std::fs::remove_dir_all(crate::runner::attachment_dir(id));
    changed(&api);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct After {
    #[serde(default)]
    after: Id,
    before: Option<Id>,
    limit: Option<usize>,
}

async fn messages(
    State(api): State<Api>,
    Path(id): Path<SessionId>,
    Query(q): Query<After>,
) -> Reply<Vec<agentbench_core::Message>> {
    api.runner.store().session(id)?;
    let mut rows = match (q.limit, q.before) {
        (None, None) => api.runner.store().messages(id, q.after)?,
        (limit, before) => api.runner.store().messages_page(
            id,
            q.after,
            before,
            limit.unwrap_or(usize::MAX >> 1),
        )?,
    };
    for row in &mut rows {
        if let agentbench_core::Entry::Wire { messages } = &mut row.entry {
            for sent in messages {
                strip_inline_images(&mut sent.message);
            }
        }
    }
    Ok(Json(rows))
}

fn strip_inline_images(message: &mut serde_json::Value) {
    let Some(parts) = message["content"].as_array_mut() else {
        return;
    };
    for part in parts.iter_mut().filter(|p| p["type"] == "image_url") {
        let url = &mut part["image_url"]["url"];
        if url.as_str().is_some_and(|u| u.starts_with("data:")) {
            *url = serde_json::Value::String("data:omitted".into());
        }
    }
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
    crate::process::correction(api.runner.store(), &session, &msg.text);
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

async fn release(State(api): State<Api>, Path(id): Path<SessionId>) -> Reply<agentbench_core::Session> {
    let store = api.runner.store();
    store.session(id)?;
    crate::autopilot::release(store, id, "user")?;
    changed(&api);
    Ok(Json(store.session(id)?))
}

async fn adopt(State(api): State<Api>, Path(id): Path<SessionId>) -> Reply<agentbench_core::Session> {
    let store = api.runner.store();
    let session = store.session(id)?;
    let supervisor = api
        .runner
        .supervisor_session()
        .ok_or_else(|| bad("there is no supervisor chat to hand it to"))?;
    if session.project_id == supervisor.project_id {
        return Err(bad("the supervisor chat cannot be handed to itself"));
    }
    crate::autopilot::adopt(store, id, supervisor.id)?;
    changed(&api);
    Ok(Json(store.session(id)?))
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

async fn terminal_screen(
    State(api): State<Api>,
    Path(id): Path<SessionId>,
) -> Reply<agentbench_core::Screen> {
    let s = api.runner.store().session(id)?;
    api.runner
        .terminals
        .open(id, std::path::Path::new(&s.cwd), None)
        .map_err(|e| Failure(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    api.runner
        .terminals
        .screen(id)
        .map(Json)
        .ok_or_else(|| Failure(StatusCode::NOT_FOUND, "no terminal".into()))
}

async fn terminal_input(
    State(api): State<Api>,
    Path(id): Path<SessionId>,
    Json(input): Json<agentbench_core::TerminalInput>,
) -> Result<StatusCode, Failure> {
    api.runner.store().session(id)?;
    let t = &api.runner.terminals;
    if input.rows > 0 && input.cols > 0 {
        t.resize(id, input.rows, input.cols).map_err(bad)?;
    }
    if !input.data.is_empty() {
        t.write(id, input.data.as_bytes()).map_err(bad)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn terminal_close(
    State(api): State<Api>,
    Path(id): Path<SessionId>,
) -> Result<StatusCode, Failure> {
    api.runner.store().session(id)?;
    api.runner.terminals.close(id);
    Ok(StatusCode::NO_CONTENT)
}

async fn jobs(
    State(api): State<Api>,
    Path(id): Path<SessionId>,
) -> Reply<Vec<agentbench_core::Job>> {
    api.runner.store().session(id)?;
    Ok(Json(api.runner.jobs(id)))
}

async fn kill_job(
    State(api): State<Api>,
    Path((id, handle)): Path<(SessionId, String)>,
) -> Reply<serde_json::Value> {
    api.runner.store().session(id)?;
    api.runner.kill_job(id, &handle).await.map_err(|e| Failure(StatusCode::NOT_FOUND, e))?;
    Ok(Json(json!({ "ok": true })))
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

    #[tokio::test]
    async fn responses_carry_nosniff() {
        let f = fixture();
        for (uri, token) in [("/health", None), ("/api/projects", Some("secret")), ("/api/projects", Some("wrong"))] {
            let mut req = axum::http::Request::builder().uri(uri);
            if let Some(t) = token {
                req = req.header("authorization", format!("Bearer {t}"));
            }
            let res = f.app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(res.headers()["x-content-type-options"], "nosniff", "{uri} {token:?}");
        }
    }

    #[test]
    fn constant_eq_rejects_prefixes_and_other_lengths() {
        assert!(constant_eq(b"secret", b"secret"));
        assert!(!constant_eq(b"secre", b"secret"));
        assert!(!constant_eq(b"secret", b"secre"));
        assert!(!constant_eq(b"", b"secret"));
        assert!(!constant_eq(b"secret", b"secreT"));
        let long = vec![b'a'; 256];
        assert!(!constant_eq(&long, b""));
    }

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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            assert_eq!(mode(&path), 0o600);
        }

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

        let (s, _) = call(&f.app, "DELETE", &format!("/api/sessions/{id}"), None, T).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert!(!path.parent().unwrap().exists(), "deleting the session removes its files");
    }

    #[tokio::test]
    async fn a_slash_template_is_stored_expanded() {
        let f = fixture();
        let dir = f.repo.join(".agents/prompts");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zz-greet.md"), "---\ndescription: Greet\n---\nHello ${1:-world} and $2\n").unwrap();
        let pid = project(&f).await;
        let (_, listed) = call(&f.app, "GET", &format!("/api/projects/{pid}/prompts"), None, T).await;
        assert!(listed.as_array().unwrap().iter().any(|t| t["name"] == "zz-greet" && t["description"] == "Greet"), "{listed}");
        let (_, session) = call(
            &f.app,
            "POST",
            "/api/sessions",
            Some(json!({"project_id": pid, "agent": "echo", "place": {"kind": "main"}})),
            T,
        )
        .await;
        let id = sid(&session);
        let (s, e) = call(
            &f.app,
            "POST",
            &format!("/api/sessions/{id}/messages"),
            Some(json!({"text": "/zz-greet \"big team\" now"})),
            T,
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{e}");
        settle(&f.api, id).await;
        let (_, msgs) = call(&f.app, "GET", &format!("/api/sessions/{id}/messages"), None, T).await;
        let text = msgs.to_string();
        assert!(text.contains("Hello big team and now"), "{text}");
        assert!(!text.contains("/zz-greet \\\"big"), "{text}");
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
    async fn onboarding_saves_providers_hides_keys_and_is_finished_once() {
        let f = fixture();
        let (s, o) = call(&f.app, "GET", "/api/onboarding", None, T).await;
        assert_eq!(s, StatusCode::OK, "{o}");
        assert_eq!(o["needed"], true);
        assert_eq!(o["status"], "new");

        let put = json!([
            {"kind": "openai_key", "key": "sk-secret", "default": true, "medium": "gpt-x"},
            {"kind": "claude_subscription"}
        ]);
        let (s, got) = call(&f.app, "PUT", "/api/onboarding/providers", Some(put), T).await;
        assert_eq!(s, StatusCode::OK, "{got}");
        assert_eq!(got[0]["key"], "");
        assert_eq!(got[0]["has_key"], true);
        let stored = crate::providers::saved(f.api.runner.store());
        assert_eq!(stored[0].key, "sk-secret");

        let again = json!([{"kind": "openai_key", "has_key": true, "default": true, "medium": "gpt-y"}]);
        let (_, got) = call(&f.app, "PUT", "/api/onboarding/providers", Some(again), T).await;
        assert_eq!(got[0]["has_key"], true);
        assert_eq!(crate::providers::saved(f.api.runner.store())[0].key, "sk-secret");

        call(&f.app, "PATCH", "/api/onboarding", Some(json!({"status": "done"})), T).await;
        let (_, o) = call(&f.app, "GET", "/api/onboarding", None, T).await;
        assert_eq!(o["needed"], false);
        assert_eq!(o["status"], "done");
        assert_eq!(o["providers"][0]["key"], "");
        assert_eq!(o["default_agent"], "native");
    }

    #[tokio::test]
    async fn the_test_endpoint_reports_pass_and_the_real_error() {
        let f = fixture();
        let up = crate::providers::tests::fake_server(vec![
            ("/models", 200, r#"{"data":[{"id":"m1"}]}"#),
            ("/chat/completions", 200, r#"{"choices":[]}"#),
        ]);
        let ok = json!({"kind": "endpoint", "url": up});
        let (_, got) = call(&f.app, "POST", "/api/onboarding/test", Some(ok), T).await;
        assert_eq!(got["ok"], true, "{got}");
        let down = crate::providers::tests::fake_server(vec![
            ("/models", 200, r#"{"data":[{"id":"m1"}]}"#),
            ("/chat/completions", 500, r#"{"error":{"message":"model overloaded"}}"#),
        ]);
        let bad = json!({"kind": "endpoint", "url": down});
        let (s, got) = call(&f.app, "POST", "/api/onboarding/test", Some(bad), T).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(got["ok"], false);
        assert!(got["message"].as_str().unwrap().contains("model overloaded"), "{got}");
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
    async fn the_supervisor_model_is_a_setting_that_empty_clears() {
        let f = fixture();
        let app = router(f.api.clone());
        let (s, got) = call(&app, "GET", "/api/settings/supervisor-model", None, T).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(got, json!({"model": "", "default": "high"}));
        let (s, got) =
            call(&app, "PUT", "/api/settings/supervisor-model", Some(json!({"model": " medium "})), T).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(got["model"], "medium");
        let store = f.api.runner.store();
        assert_eq!(crate::providers::supervisor_model(store, "claude").as_deref(), Some("medium"));
        call(&app, "PUT", "/api/settings/supervisor-model", Some(json!({"model": ""})), T).await;
        assert_eq!(crate::providers::supervisor_model(store, "claude").as_deref(), Some("high"));
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
    async fn levels_and_toggles_are_set_over_the_api_and_shown_in_the_overview() {
        let f = fixture();
        let pid = project(&f).await;
        let (_, o) = call(&f.app, "GET", "/api/overview", None, T).await;
        assert_eq!(o["autopilot"]["default_level"], "delegate");
        assert!(o["projects"][0]["automation"].is_null());
        let uri = format!("/api/projects/{pid}");
        let (s, p) = call(&f.app, "PATCH", &uri, Some(json!({"automation": "full_auto", "risky": ["deploy"]})), T).await;
        assert_eq!(s, StatusCode::OK, "{p}");
        assert_eq!((p["automation"].clone(), p["risky"].clone()), (json!("full_auto"), json!(["deploy"])));
        let (_, p) = call(&f.app, "PATCH", &uri, Some(json!({"automation": null})), T).await;
        assert!(p["automation"].is_null(), "null goes back to the default");
        assert_eq!(p["risky"], json!(["deploy"]));
        let (s, _) = call(&f.app, "PATCH", &uri, Some(json!({"automation": "turbo"})), T).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        let (_, a) = call(&f.app, "PATCH", "/api/autopilot", Some(json!({"default_level": "ship"})), T).await;
        assert_eq!(a["default_level"], "ship");
        assert!(a["activity"].as_array().unwrap().iter().any(|x| x["text"].as_str().unwrap().contains("default automation level set to ship")));
    }

    #[tokio::test]
    async fn the_supervisor_chat_is_one_session_and_its_project_is_never_listed() {
        let f = fixture();
        let dir = tempfile::tempdir().unwrap();
        crate::projects::ensure_supervisor(f.api.runner.store(), dir.path()).unwrap();
        project(&f).await;

        let (_, overview) = call(&f.app, "GET", "/api/overview", None, T).await;
        assert!(overview["supervisor"].is_null());
        let names: Vec<_> =
            overview["projects"].as_array().unwrap().iter().map(|p| p["name"].clone()).collect();
        assert_eq!(names, [json!("demo")], "the supervisor has no project in the list");
        let (_, listed) = call(&f.app, "GET", "/api/projects", None, T).await;
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(overview["worktrees"].as_array().unwrap().len(), 1, "nor its folder");

        let open = || call(&f.app, "POST", "/api/supervisor", Some(json!({"agent": "echo"})), T);
        let (s, first) = open().await;
        assert_eq!(s, StatusCode::OK, "{first}");
        let (_, again) = open().await;
        assert_eq!(first["id"], again["id"], "one standing chat");
        assert_eq!(first["title"], "Supervisor");
        assert_eq!(f.api.runner.store().sessions().unwrap().len(), 1);

        let (_, overview) = call(&f.app, "GET", "/api/overview", None, T).await;
        assert_eq!(overview["supervisor"], first["id"]);
    }

    #[tokio::test]
    async fn release_and_adopt_move_a_session_between_the_user_and_the_supervisor() {
        let f = fixture();
        let store = f.api.runner.store().clone();
        let dir = tempfile::tempdir().unwrap();
        let (_, main) = crate::store::testing::project(&store, "/a");
        let child = store.add_session(main.id, "echo", "child", None, None).unwrap();
        let uri = |verb: &str| format!("/api/sessions/{}/{verb}", child.id);

        assert_eq!(call(&f.app, "POST", &uri("release"), None, None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(call(&f.app, "POST", &uri("adopt"), None, None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(call(&f.app, "POST", &uri("adopt"), None, T).await.0, StatusCode::BAD_REQUEST, "no supervisor");

        let sup = crate::projects::ensure_supervisor(&store, dir.path()).unwrap();
        let sup_main = store.main_worktree(sup.id).unwrap();
        let boss = store.add_session(sup_main.id, "echo", "boss", None, None).unwrap();
        store.set_parent(child.id, Some(boss.id)).unwrap();

        let (s, body) = call(&f.app, "POST", &uri("release"), None, T).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(body["released"], true);
        assert!(body["parent"].is_null());

        let (s, body) = call(&f.app, "POST", &uri("adopt"), None, T).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(body["parent"], json!(boss.id));
        assert!(body["released"].is_null());
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
    async fn accepting_a_proposal_applies_it_and_rejecting_keeps_it_out() {
        use agentbench_core::{Change, IssuePatch, Proposal, ProposalKind, ProposalState};
        let f = fixture();
        let store = f.api.runner.store();
        let p = store.add_project("other", "/tmp/other", Some("main"), None).unwrap();
        let mk = |project_id| {
            store
                .add_issue(&agentbench_core::NewIssue {
                    project_id,
                    title: "dup".into(),
                    ..Default::default()
                })
                .unwrap()
                .id
        };
        let (a, b) = (mk(p.id), mk(p.id));
        let proposal = |change| Proposal {
            id: 0,
            kind: ProposalKind::Labels,
            summary: "s".into(),
            issues: vec![a],
            sessions: vec![],
            projects: vec![p.id],
            changes: vec![change],
            state: ProposalState::Pending,
            created_at: 0,
        };
        let patch = IssuePatch { priority: Some(7), labels: Some(vec!["bug".into()]), ..Default::default() };
        let yes = store.add_proposal("x:1", &proposal(Change::Update { issue_id: a, patch })).unwrap();
        let no = store
            .add_proposal("x:2", &proposal(Change::Comment { issue_id: b, body: "nope".into() }))
            .unwrap();
        let (st, list) = call(&f.app, "GET", "/api/triage", None, T).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(list["proposals"].as_array().unwrap().len(), 2);

        let (st, _) = call(&f.app, "POST", &format!("/api/triage/{yes}/accept"), None, T).await;
        assert_eq!(st, StatusCode::OK);
        let issue = store.issue(a).unwrap();
        assert_eq!((issue.priority, issue.labels), (7, vec!["bug".to_string()]));
        let (st, _) = call(&f.app, "POST", &format!("/api/triage/{yes}/accept"), None, T).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "decided once");

        let (st, _) = call(&f.app, "POST", &format!("/api/triage/{no}/reject"), None, T).await;
        assert_eq!(st, StatusCode::OK);
        assert!(store.comments(b).unwrap().is_empty(), "a rejected proposal changes nothing");
        assert!(store.proposal_known("x:2").unwrap(), "and is remembered");
        let (_, list) = call(&f.app, "GET", "/api/triage", None, T).await;
        assert!(list["proposals"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn accepting_a_global_note_proposal_writes_it_and_rejecting_drops_it() {
        use agentbench_core::{Change, Proposal, ProposalKind, ProposalState};
        let f = fixture();
        let store = f.api.runner.store();
        let mk = |topic: &str| Proposal {
            id: 0,
            kind: ProposalKind::GlobalNote,
            summary: topic.into(),
            issues: vec![],
            sessions: vec![],
            projects: vec![],
            changes: vec![Change::GlobalNote { topic: topic.into(), note: "plain words".into() }],
            state: ProposalState::Pending,
            created_at: 0,
        };
        let yes = store.add_proposal("g:1", &mk("style")).unwrap();
        let no = store.add_proposal("g:2", &mk("other")).unwrap();
        let (st, _) = call(&f.app, "POST", &format!("/api/triage/{no}/reject"), None, T).await;
        assert_eq!(st, StatusCode::OK);
        assert!(store.global_notes().unwrap().is_empty());
        let (st, _) = call(&f.app, "POST", &format!("/api/triage/{yes}/accept"), None, T).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(store.global_notes().unwrap(), [("style".to_string(), "plain words".to_string())]);
    }

    #[tokio::test]
    async fn the_vision_is_read_written_and_restored_over_http() {
        let f = fixture();
        let pid = project(&f).await;
        let path = format!("/api/projects/{pid}/vision");
        let (s, none) = call(&f.app, "GET", &path, None, T).await;
        assert_eq!((s, none), (StatusCode::OK, serde_json::Value::Null));
        let first = json!({"summary": "One", "goals": [{"id": "", "text": "ship"}], "non_goals": ["cloud"]});
        let (s, v) = call(&f.app, "PUT", &path, Some(first), T).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!((v["summary"].as_str(), v["author"].as_str()), (Some("One"), Some("user")));
        assert_eq!(v["goals"][0]["id"], "g1");
        assert_eq!(v["goals"][0]["status"], "active");
        let (s, v2) = call(&f.app, "PUT", &path, Some(json!({"summary": "Two"})), T).await;
        assert_eq!(s, StatusCode::OK);
        let (_, now) = call(&f.app, "GET", &path, None, T).await;
        assert_eq!(now["summary"], "Two");
        let (_, versions) = call(&f.app, "GET", &format!("{path}/versions"), None, T).await;
        assert_eq!(versions.as_array().unwrap().len(), 2);
        let first_id = versions[1]["id"].clone();
        assert_ne!(first_id, v2["id"]);
        let (s, back) = call(&f.app, "POST", &format!("{path}/restore"), Some(json!({"version": first_id})), T).await;
        assert_eq!((s, back["summary"].as_str()), (StatusCode::OK, Some("One")));
        let (_, versions) = call(&f.app, "GET", &format!("{path}/versions"), None, T).await;
        assert_eq!(versions.as_array().unwrap().len(), 3, "a restore is a new version");
        let (s, _) = call(&f.app, "POST", &format!("{path}/restore"), Some(json!({"version": 9999})), T).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = call(&f.app, "GET", "/api/projects/9999/vision", None, T).await;
        assert_ne!(s, StatusCode::OK);
        let (_, issue) = call(&f.app, "POST", "/api/issues", Some(json!({"project_id": pid, "title": "t", "goal": "g1"})), T).await;
        assert_eq!(issue["goal"], "g1");
        assert_eq!(issue["off_vision"], false);
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
    async fn issue_dependencies_are_read_and_replaced_over_rest() {
        let f = fixture();
        let pid = project(&f).await;
        let mut ids = Vec::new();
        for t in ["a", "b"] {
            let (_, i) = call(&f.app, "POST", "/api/issues", Some(json!({"project_id": pid, "title": t})), T).await;
            ids.push(i["id"].as_i64().unwrap());
        }
        let url = format!("/api/issues/{}/deps", ids[1]);
        let (code, put) = call(&f.app, "PUT", &url, Some(json!([ids[0]])), T).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(put["waiting_on"], json!([ids[0]]));
        assert_eq!(put["depends_on"], json!([ids[0]]));
        assert_eq!(put["blocks"], json!([]));
        let got = call(&f.app, "GET", &url, None, T).await.1;
        assert_eq!(got["depends_on"], json!([ids[0]]));
        let first = call(&f.app, "GET", &format!("/api/issues/{}/deps", ids[0]), None, T).await.1;
        assert_eq!(first["blocks"], json!([ids[1]]));
        let back = format!("/api/issues/{}/deps", ids[0]);
        assert!(!call(&f.app, "PUT", &back, Some(json!([ids[1]])), T).await.0.is_success());
        call(&f.app, "PUT", &url, Some(json!([])), T).await;
        assert_eq!(call(&f.app, "GET", &url, None, T).await.1["depends_on"], json!([]));
    }

    #[tokio::test]
    async fn finishing_a_dependency_over_rest_releases_its_dependent() {
        let f = fixture();
        let pid = project(&f).await;
        let (_, a) = call(&f.app, "POST", "/api/issues", Some(json!({"project_id": pid, "title": "a"})), T).await;
        let a = a["id"].as_i64().unwrap();
        let (_, b) = call(&f.app, "POST", "/api/issues", Some(json!({"project_id": pid, "title": "b", "state": "backlog", "depends_on": [a]})), T).await;
        let b = b["id"].as_i64().unwrap();
        call(&f.app, "PATCH", &format!("/api/issues/{a}"), Some(json!({"state": "done"})), T).await;
        let (_, comments) = call(&f.app, "GET", &format!("/api/issues/{b}/comments"), None, T).await;
        assert!(comments[0]["body"].as_str().unwrap().starts_with(&format!("unblocked by #{a}")));
        let (_, got) = call(&f.app, "GET", &format!("/api/issues/{b}"), None, T).await;
        assert_eq!(got["waiting_on"], json!([]));
    }

    #[tokio::test]
    async fn moving_an_issue_to_todo_starts_an_agent_when_autorun_is_on() {
        let f = fixture();
        let pid = project(&f).await;
        let (_, issue) = call(
            &f.app,
            "POST",
            "/api/issues",
            Some(json!({"project_id": pid, "title": "Say hi", "state": "backlog"})),
            T,
        )
        .await;
        let id = issue["id"].as_i64().unwrap();
        let todo = || Some(json!({"state": "todo"}));
        let url = format!("/api/issues/{id}");

        let (_, got) = call(&f.app, "PATCH", &url, todo(), T).await;
        assert_eq!(got["state"], "todo", "off by default: nothing starts");
        assert!(f.api.runner.store().sessions().unwrap().is_empty());

        call(
            &f.app,
            "PUT",
            "/api/settings/autorun",
            Some(json!({"enabled": true, "agent": "echo"})),
            T,
        )
        .await;
        call(&f.app, "PATCH", &url, Some(json!({"state": "backlog"})), T).await;
        let (s, got) = call(&f.app, "PATCH", &url, todo(), T).await;
        assert_eq!(s, StatusCode::OK, "{got}");
        assert_eq!(got["state"], "doing");
        let sessions = f.api.runner.store().sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].issue_id, Some(id));
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
    async fn the_transcript_can_be_read_latest_first_in_pages() {
        use agentbench_core::Entry;
        let f = fixture();
        let pid = project(&f).await;
        let (_, sess) = call(
            &f.app,
            "POST",
            "/api/sessions",
            Some(json!({"project_id": pid, "agent": "echo", "prompt": "hi"})),
            T,
        )
        .await;
        let id = sid(&sess);
        settle(&f.api, id).await;
        let store = f.api.runner.store();
        for n in 0..10 {
            store.add_message(id, &Entry::Notice { text: format!("note {n}") }).unwrap();
        }
        let all = store.messages(id, 0).unwrap();
        let ids: Vec<i64> = all.iter().map(|m| m.id).collect();
        let get = |q: String| {
            let app = f.app.clone();
            async move {
                let (s, v) =
                    call(&app, "GET", &format!("/api/sessions/{id}/messages{q}"), None, T).await;
                assert_eq!(s, StatusCode::OK);
                v.as_array().unwrap().iter().map(|m| m["id"].as_i64().unwrap()).collect::<Vec<_>>()
            }
        };
        assert_eq!(get(String::new()).await, ids, "no limit still returns everything");
        let n = ids.len();
        assert_eq!(get("?limit=3".into()).await, ids[n - 3..], "the latest rows, oldest first");
        let first_shown = ids[n - 3];
        assert_eq!(
            get(format!("?before={first_shown}&limit=4")).await,
            ids[n - 7..n - 3],
            "the page just before"
        );
        assert_eq!(get(format!("?before={}&limit=50", ids[0])).await, Vec::<i64>::new());
        assert_eq!(get(format!("?after={}&limit=2", ids[n - 6])).await, ids[n - 2..]);
    }

    #[tokio::test]
    async fn the_transcript_leaves_out_inline_image_data() {
        use agentbench_core::{Entry, Said, Sent};
        let f = fixture();
        let pid = project(&f).await;
        let (_, sess) = call(
            &f.app,
            "POST",
            "/api/sessions",
            Some(json!({"project_id": pid, "agent": "echo", "prompt": "hi"})),
            T,
        )
        .await;
        let id = sid(&sess);
        settle(&f.api, id).await;
        let blob = "A".repeat(200_000);
        let wire = Entry::Wire {
            messages: vec![Sent {
                message: json!({"role": "user", "content": [
                    {"type": "text", "text": "the screenshot"},
                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{blob}")}},
                ]}),
                said: Said::Plain,
            }],
        };
        f.api.runner.store().add_message(id, &wire).unwrap();
        let (_, msgs) = call(&f.app, "GET", &format!("/api/sessions/{id}/messages"), None, T).await;
        let body = msgs.to_string();
        assert!(!body.contains("AAAAAAAA"), "{} bytes came back", body.len());
        assert!(body.contains("the screenshot"));
        assert!(body.contains("image_url"), "the part stays, so the shape is kept");
    }

    #[tokio::test]
    async fn a_big_transcript_is_sent_gzipped_to_a_client_that_accepts_it() {
        use agentbench_core::{Entry, Said, Sent};
        let f = fixture();
        let pid = project(&f).await;
        let (_, sess) = call(
            &f.app,
            "POST",
            "/api/sessions",
            Some(json!({"project_id": pid, "agent": "echo", "prompt": "hi"})),
            T,
        )
        .await;
        let id = sid(&sess);
        settle(&f.api, id).await;
        let text = "the same line of output\n".repeat(20_000);
        let wire = Entry::Wire {
            messages: vec![Sent {
                message: json!({"role": "user", "content": text}),
                said: Said::Plain,
            }],
        };
        f.api.runner.store().add_message(id, &wire).unwrap();
        let req = axum::http::Request::builder()
            .uri(format!("/api/sessions/{id}/messages"))
            .header("authorization", "Bearer secret")
            .header("accept-encoding", "gzip")
            .body(Body::empty())
            .unwrap();
        let res = f.app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.headers().get("content-encoding").map(|v| v.as_bytes()), Some(&b"gzip"[..]));
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        assert!(bytes.len() < 20_000, "{} bytes on the wire", bytes.len());
    }

    #[tokio::test]
    async fn a_chat_is_renamed_and_keeps_the_name() {
        let f = fixture();
        let pid = project(&f).await;
        let (_, sess) = call(
            &f.app,
            "POST",
            "/api/sessions",
            Some(json!({"project_id": pid, "agent": "echo", "prompt": "hi"})),
            T,
        )
        .await;
        let id = sess["id"].as_str().unwrap().to_string();
        let path = format!("/api/sessions/{id}");
        let (s, renamed) =
            call(&f.app, "PATCH", &path, Some(json!({"title": "  Billing work "})), T).await;
        assert_eq!(s, StatusCode::OK, "{renamed}");
        assert_eq!(renamed["title"], "Billing work");
        let (s, _) = call(&f.app, "PATCH", &path, Some(json!({"title": "  "})), T).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (_, again) = call(&f.app, "GET", &path, None, T).await;
        assert_eq!(again["title"], "Billing work");
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
    async fn deleting_the_last_chat_of_a_clean_merged_worktree_removes_it_and_its_branch() {
        let f = fixture();
        let pid = project(&f).await;
        let (_, sess) = call(
            &f.app,
            "POST",
            "/api/sessions",
            Some(json!({"project_id": pid, "agent": "echo", "place": {"kind": "new", "branch": "gone"}})),
            T,
        )
        .await;
        let path = std::path::PathBuf::from(sess["cwd"].as_str().unwrap());
        settle(&f.api, sid(&sess)).await;
        assert!(path.exists());
        let (s, _) = call(&f.app, "DELETE", &format!("/api/sessions/{}", sid(&sess)), None, T).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert!(!path.exists());
        assert!(!crate::git::branch_exists(&f.repo, "gone"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_chat_has_a_terminal_that_takes_input_and_shows_a_screen() {
        let f = fixture();
        let pid = project(&f).await;
        let (_, sess) = call(
            &f.app,
            "POST",
            "/api/sessions",
            Some(json!({"project_id": pid, "agent": "echo", "place": {"kind": "new", "branch": "tty"}, "prompt": "hi"})),
            T,
        )
        .await;
        let id = sid(&sess);
        let uri = format!("/api/sessions/{id}/terminal");
        let (s, screen) = call(&f.app, "GET", &uri, None, T).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(screen["alive"], true);
        let (s, _) =
            call(&f.app, "POST", &uri, Some(json!({"data": "echo $((20+22))ok\r"})), T).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let mut seen = false;
        for _ in 0..60 {
            let (_, screen) = call(&f.app, "GET", &uri, None, T).await;
            if screen["rows"].to_string().contains("42ok") {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(seen);
        let (s, _) = call(&f.app, "DELETE", &uri, None, T).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_chats_background_commands_can_be_read_and_stopped() {
        use crate::agent::native::{Model, Native};
        let f = fixture();
        let native = Native::new(Model { model: "m".into(), ..Model::default() });
        let jobs = native.jobs.clone();
        let api = Api {
            runner: Runner::new(f.api.runner.store().clone(), vec![Box::new(native)]),
            ..f.api.clone()
        };
        let app = router(api);
        let pid = project(&f).await;
        let (_, sess) = call(
            &app,
            "POST",
            "/api/sessions",
            Some(json!({"project_id": pid, "agent": "native", "place": {"kind": "new", "branch": "dev"}, "prompt": null})),
            T,
        )
        .await;
        let id = sid(&sess).to_string();
        let (s, none) = call(&app, "GET", &format!("/api/sessions/{id}/jobs"), None, T).await;
        assert_eq!((s, none), (StatusCode::OK, json!([])));
        jobs.start(
            &id,
            &f.repo,
            "echo serving; sleep 60",
            &agent_core::spawn::Confine::Open { env: Vec::new() },
        )
        .unwrap();
        let mut text = String::new();
        for _ in 0..50 {
            let (_, list) = call(&app, "GET", &format!("/api/sessions/{id}/jobs"), None, T).await;
            text = list[0]["text"].as_str().unwrap_or_default().to_string();
            if text.contains("serving") {
                assert_eq!(list[0]["running"], true);
                assert_eq!(list[0]["handle"], "job-1");
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(text.contains("serving"), "{text}");
        let (s, _) =
            call(&app, "POST", &format!("/api/sessions/{id}/jobs/job-1/kill"), None, T).await;
        assert_eq!(s, StatusCode::OK);
        let (_, list) = call(&app, "GET", &format!("/api/sessions/{id}/jobs"), None, T).await;
        assert_eq!(list[0]["running"], false);
        let (s, _) =
            call(&app, "POST", &format!("/api/sessions/{id}/jobs/job-9/kill"), None, T).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
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

    #[tokio::test]
    async fn the_attention_poll_ignores_messages_and_wakes_on_waiting() {
        let f = fixture();
        let pid = project(&f).await;
        let store = f.api.runner.store();
        let wt = store.worktrees(Some(pid)).unwrap()[0].id;
        let s = store.add_session(wt, "echo", "chat", None, None).unwrap();
        let (_, first) = call(&f.app, "GET", "/api/changes?kinds=attention", None, T).await;
        assert_eq!(first["reset"], true);
        assert_eq!(first["changes"].as_array().unwrap().len(), 1);
        assert_eq!(first["changes"][0]["title"], "chat");
        let (cursor, epoch) = (first["cursor"].as_i64().unwrap(), first["epoch"].as_u64().unwrap());
        let uri = format!("/api/changes?kinds=attention&since={cursor}&epoch={epoch}&wait=1");
        let app2 = f.app.clone();
        let u2 = uri.clone();
        let waiter = tokio::spawn(async move { call(&app2, "GET", &u2, None, T).await });
        for i in 0..5 {
            store.add_message(s.id, &agentbench_core::Entry::Notice { text: format!("m{i}") }).unwrap();
            f.api.runner.changed.notify_waiters();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (_, quiet) = waiter.await.unwrap();
        assert_eq!(quiet["changes"].as_array().unwrap().len(), 0, "{quiet}");
        assert_eq!(quiet["reset"], false);

        let uri = format!("/api/changes?kinds=attention&since={cursor}&epoch={epoch}&wait=10");
        let app2 = f.app.clone();
        let waiter = tokio::spawn(async move { call(&app2, "GET", &uri, None, T).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        store.set_status(s.id, agentbench_core::SessionStatus::Waiting).unwrap();
        f.api.runner.changed.notify_waiters();
        let (_, woke) = tokio::time::timeout(Duration::from_secs(3), waiter).await.unwrap().unwrap();
        assert_eq!(
            woke["changes"],
            json!([{"kind": "attention", "id": s.id.to_string(), "status": "waiting"}])
        );
        assert!(woke.to_string().len() < 300, "{woke}");
    }

    #[tokio::test]
    async fn the_attention_feed_for_50_sessions_stays_small() {
        let f = fixture();
        let pid = project(&f).await;
        let store = f.api.runner.store();
        let wt = store.worktrees(Some(pid)).unwrap()[0].id;
        for i in 0..50 {
            let s = store.add_session(wt, "echo", &format!("chat number {i}"), None, None).unwrap();
            store.set_status(s.id, agentbench_core::SessionStatus::Running).unwrap();
        }
        let (_, snapshot) = call(&f.app, "GET", "/api/changes?kinds=attention", None, T).await;
        assert_eq!(snapshot["changes"].as_array().unwrap().len(), 50);
        assert!(snapshot.to_string().len() < 8_000, "{}", snapshot.to_string().len());
        let text = snapshot.to_string();
        assert!(!text.contains("question") && !text.contains("entry"));
    }

    #[tokio::test]
    async fn the_webhook_route_needs_a_signature_not_a_token() {
        let f = fixture();
        let pid = project(&f).await;
        let store = f.api.runner.store().clone();
        store.update_project(pid, &ProjectPatch { tracker: Some(agentbench_core::Tracker::Github { repo: "o/r".into() }), ..Default::default() }).unwrap();
        let (s, made) = call(&f.app, "POST", &format!("/api/projects/{pid}/github/webhook"), None, T).await;
        assert_eq!(s, StatusCode::OK, "{made}");
        let secret = made["secret"].as_str().unwrap().to_string();
        let (_, st) = call(&f.app, "GET", &format!("/api/projects/{pid}/github"), None, T).await;
        assert_eq!(st["webhook_secret_set"], true);
        assert!(!st.to_string().contains(&secret), "the status never shows the secret");
        let body = json!({"zen": "hi", "repository": {"full_name": "o/r"}}).to_string();
        let send = |sig: String, id: &str| {
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/api/github/webhook/{pid}"))
                .header("x-github-event", "ping")
                .header("x-github-delivery", id)
                .header("x-hub-signature-256", sig)
                .body(Body::from(body.clone()))
                .unwrap()
        };
        let bad = f.app.clone().oneshot(send("sha256=00".into(), "a")).await.unwrap();
        assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);
        assert!(bad.into_body().collect().await.unwrap().to_bytes().is_empty(), "a refusal has no body");
        let sig = crate::github::webhook::signature(&secret, body.as_bytes());
        let ok = f.app.clone().oneshot(send(sig.clone(), "b")).await.unwrap();
        assert_eq!(ok.status(), StatusCode::NO_CONTENT);
        let again = f.app.clone().oneshot(send(sig, "b")).await.unwrap();
        assert_eq!(again.status(), StatusCode::ACCEPTED);
        let big = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/github/webhook/{pid}"))
            .body(Body::from(vec![b'x'; crate::github::webhook::BODY_MAX + 10]))
            .unwrap();
        assert_eq!(f.app.clone().oneshot(big).await.unwrap().status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn github_settings_and_the_project_outbox_round_trip() {
        let f = fixture();
        let pid = project(&f).await;
        let (_, s) = call(&f.app, "GET", "/api/settings/github", None, T).await;
        assert_eq!(s["poll_secs"], 60);
        let (code, s) = call(&f.app, "PUT", "/api/settings/github", Some(json!({"poll_secs": 120, "webhook_base": "https://b.example/"})), T).await;
        assert_eq!(code, StatusCode::OK, "{s}");
        assert_eq!((s["poll_secs"].as_i64(), s["webhook_base"].as_str()), (Some(120), Some("https://b.example")));
        let (code, _) = call(&f.app, "PUT", "/api/settings/github", Some(json!({"poll_secs": 1})), T).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let store = f.api.runner.store().clone();
        store.update_project(pid, &ProjectPatch { tracker: Some(agentbench_core::Tracker::Github { repo: "o/r".into() }), ..Default::default() }).unwrap();
        let ask = |text: &str| crate::github::outbox::Ask { project: pid, kind: "comment".into(), number: 3, text: text.into(), params: json!({}), asked_by: "supervisor".into(), trusted: true };
        crate::github::outbox::propose(&store, ask("one")).unwrap();
        crate::github::outbox::propose(&store, ask("two")).unwrap();
        let (_, list) = call(&f.app, "GET", &format!("/api/outbox?project={pid}"), None, T).await;
        assert_eq!(list["items"].as_array().unwrap().len(), 2);
        let (_, none) = call(&f.app, "GET", &format!("/api/outbox?project={}", pid + 100), None, T).await;
        assert!(none["items"].as_array().unwrap().is_empty());
        let (code, done) = call(&f.app, "POST", &format!("/api/projects/{pid}/outbox/approve-all"), None, T).await;
        assert_eq!(code, StatusCode::OK, "{done}");
        assert_eq!(done["approved"].as_array().unwrap().len(), 2);
        let (_, paused) = call(&f.app, "POST", "/api/github/pause", Some(json!({"paused": true})), T).await;
        assert_eq!(paused["paused"], true);
        let (_, list) = call(&f.app, "GET", "/api/outbox", None, T).await;
        assert_eq!(list["paused"], true);
        assert!(list["items"].as_array().unwrap().iter().all(|i| i["status"] == "approved"));
    }

    #[tokio::test]
    async fn project_tracker_settings_round_trip_through_the_api() {
        let f = fixture();
        let pid = project(&f).await;
        let (_, p) = call(&f.app, "GET", "/api/projects", None, T).await;
        assert_eq!(p[0]["post_mode"], "draft");
        assert_eq!(p[0]["tracker"]["kind"], "internal");
        let patch = json!({"tracker": {"kind": "github", "repo": "o/r"}, "post_mode": "off"});
        let (s, p) = call(&f.app, "PATCH", &format!("/api/projects/{pid}"), Some(patch), T).await;
        assert_eq!(s, StatusCode::OK, "{p}");
        assert_eq!(p["tracker"], json!({"kind": "github", "repo": "o/r"}));
        assert_eq!(p["post_mode"], "off");
        let (_, p) = call(&f.app, "GET", "/api/projects", None, T).await;
        assert_eq!(p[0]["tracker"]["repo"], "o/r");
        let (_, back) = call(&f.app, "PATCH", &format!("/api/projects/{pid}"), Some(json!({"tracker": {"kind": "internal"}})), T).await;
        assert_eq!(back["tracker"]["kind"], "internal");
        assert_eq!(back["post_mode"], "off");
    }

    fn stub(dir: &std::path::Path, name: &str, script: &str) -> String {
        let path = dir.join(name);
        crate::testing_stub::write_script(&path, &format!("#!/bin/sh\n{script}\n"));
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn the_github_status_names_a_missing_or_unauthenticated_gh() {
        let f = fixture();
        let pid = project(&f).await;
        let patch = json!({"tracker": {"kind": "github", "repo": "o/r"}});
        call(&f.app, "PATCH", &format!("/api/projects/{pid}"), Some(patch), T).await;
        let uri = format!("/api/projects/{pid}/github");
        let dir = f._dir.path();
        let ok = stub(dir, "gh-ok", "case \"$*\" in *rate_limit*) echo 4999;; *repos/*) echo true;; esac");
        let denied = stub(dir, "gh-denied", "exit 1");
        let _guard = crate::github::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("AGENTBENCH_GH", dir.join("absent")) };
        let (s, st) = call(&f.app, "GET", &uri, None, T).await;
        assert_eq!(s, StatusCode::OK, "{st}");
        assert_eq!((st["installed"].as_bool(), st["problem"].as_str()), (Some(false), Some("gh missing")));
        unsafe { std::env::set_var("AGENTBENCH_GH", &denied) };
        let (_, st) = call(&f.app, "GET", &uri, None, T).await;
        assert_eq!((st["installed"].as_bool(), st["authenticated"].as_bool()), (Some(true), Some(false)));
        assert_eq!(st["problem"], "not authenticated");
        unsafe { std::env::set_var("AGENTBENCH_GH", &ok) };
        let (_, st) = call(&f.app, "GET", &uri, None, T).await;
        unsafe { std::env::remove_var("AGENTBENCH_GH") };
        assert_eq!(st["authenticated"], true, "{st}");
        assert_eq!(st["repo"], "o/r");
        assert_eq!(st["write_access"], true);
        assert_eq!(st["rate_limit_remaining"], 4999);
        assert!(st["problem"].is_null());
    }

    #[tokio::test]
    async fn every_response_names_the_api_version_and_the_overview_says_what_it_needs() {
        let f = fixture();
        for (uri, token) in [("/health", None), ("/api/overview", Some("secret")), ("/api/overview", Some("wrong"))] {
            let mut req = axum::http::Request::builder().uri(uri);
            if let Some(t) = token {
                req = req.header("authorization", format!("Bearer {t}"));
            }
            let res = f.app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(
                res.headers()["x-agentbench-api"],
                agentbench_core::API_VERSION.to_string().as_str(),
                "{uri} {token:?}"
            );
        }
        let (_, o) = call(&f.app, "GET", "/api/overview", None, T).await;
        assert_eq!(o["api_version"], agentbench_core::API_VERSION);
        assert_eq!(o["min_client_api"], agentbench_core::MIN_CLIENT_API);
    }

    #[tokio::test]
    async fn the_recorded_api_fixtures_match_this_server() {
        let f = fixture();
        let pid = project(&f).await;
        let record = std::env::var("RECORD_API_FIXTURES").is_ok();
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../core/tests/fixtures/api")
            .join(format!("v{}", agentbench_core::API_VERSION));
        let mut keep = |name: &str, v: &serde_json::Value| {
            if record {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join(format!("{name}.json")), serde_json::to_string_pretty(v).unwrap() + "\n")
                    .unwrap();
            }
        };
        let new_issue = json!({"project_id": pid, "title": "Fix the thing", "body": "details", "state": "todo", "priority": 3, "labels": ["bug"], "model_level": "high"});
        keep("request_new_issue", &new_issue);
        let (s, issue) = call(&f.app, "POST", "/api/issues", Some(new_issue), T).await;
        assert_eq!(s, StatusCode::OK, "{issue}");
        let patch = json!({"state": "doing", "title": "Fix it", "model_level": "med"});
        keep("request_issue_patch", &patch);
        call(&f.app, "PATCH", &format!("/api/issues/{}", issue["id"]), Some(patch), T).await;
        keep("request_new_project", &json!({"path": f.repo.to_str().unwrap(), "name": "demo"}));
        let new_session = json!({
            "project_id": pid, "agent": "echo", "place": {"kind": "main"}, "prompt": "hello",
        });
        keep("request_new_session", &new_session);
        let (s, session) = call(&f.app, "POST", "/api/sessions", Some(new_session), T).await;
        assert_eq!(s, StatusCode::OK, "{session}");
        let id = sid(&session);
        settle(&f.api, id).await;
        let send = json!({"text": "again"});
        keep("request_send_message", &send);
        let (_, overview) = call(&f.app, "GET", "/api/overview", None, T).await;
        keep("overview", &overview);
        let (_, sessions) = call(&f.app, "GET", "/api/sessions", None, T).await;
        keep("sessions", &sessions);
        let (_, issues) = call(&f.app, "GET", "/api/issues", None, T).await;
        keep("issues", &issues);
        let (_, messages) = call(&f.app, "GET", &format!("/api/sessions/{id}/messages"), None, T).await;
        keep("messages", &messages);
        let (_, changes) = call(&f.app, "GET", "/api/changes", None, T).await;
        keep("changes", &changes);
        let pid_num = pid;
        f.api.runner.store().update_project(pid_num, &agentbench_core::ProjectPatch { tracker: Some(agentbench_core::Tracker::Github { repo: "o/r".into() }), ..Default::default() }).unwrap();
        f.api
            .runner
            .store()
            .mirror_pr(
                pid_num,
                &crate::store::MirrorPr {
                    number: 9,
                    title: "Fix".into(),
                    body: "Closes #4".into(),
                    author: "agentbench".into(),
                    association: String::new(),
                    head_ref: "issue-4".into(),
                    base_ref: "main".into(),
                    head_sha: "abc".into(),
                    state: "open".into(),
                    draft: false,
                    url: "https://github.com/o/r/pull/9".into(),
                    created_at: 1,
                    updated_at: 2,
                    trust: agentbench_core::Trust::Trusted,
                },
            )
            .unwrap();
        let check = agentbench_core::PrCheck { name: "test".into(), status: "completed".into(), conclusion: Some("success".into()), run_id: Some(5), url: "u".into() };
        f.api.runner.store().set_pr_checks(pid_num, 9, "abc", &[check]).unwrap();
        f.api.runner.store().set_pr_review(pid_num, 9, "abc", "approve", 1, 0).unwrap();
        let (s, pulls) = call(&f.app, "GET", &format!("/api/projects/{pid}/pulls"), None, T).await;
        assert_eq!(s, StatusCode::OK, "{pulls}");
        keep("pulls", &pulls);
        let ask = crate::github::outbox::Ask { project: pid, kind: "label_add".into(), number: 4, text: String::new(), params: json!({"label": "bug"}), asked_by: "supervisor".into(), trusted: true };
        crate::github::outbox::propose(f.api.runner.store(), ask).unwrap();
        let (s, outbox) = call(&f.app, "GET", &format!("/api/outbox?project={pid}"), None, T).await;
        assert_eq!(s, StatusCode::OK, "{outbox}");
        keep("outbox", &outbox);
        let _: agentbench_core::OutboxList = serde_json::from_value(outbox).unwrap();
        let (_, settings) = call(&f.app, "GET", "/api/settings/github", None, T).await;
        keep("github_settings", &settings);
        let _: agentbench_core::GithubSettings = serde_json::from_value(settings).unwrap();
        let _: Vec<agentbench_core::PullRequest> = serde_json::from_value(pulls).unwrap();
        let overview: agentbench_core::Overview = serde_json::from_value(overview).unwrap();
        assert_eq!(overview.api_version, agentbench_core::API_VERSION);
        let _: agentbench_core::ChangesFeed = serde_json::from_value(changes).unwrap();
        let _: Vec<agentbench_core::Message> = serde_json::from_value(messages).unwrap();
        let _: agentbench_core::SendMessage = serde_json::from_value(send).unwrap();
    }
}
