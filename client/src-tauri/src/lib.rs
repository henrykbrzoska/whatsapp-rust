//! Tauri shell: spawns the engine (`binaries/bebok-server`) as a sidecar with
//! `--port 0`, reads the `BEBOK_READY http://127.0.0.1:<port>/?token=…` line
//! from its stdout and hands it to the WebView through the `engine_ready`
//! command. The frontend then talks HTTP + SSE to the sidecar exactly like in
//! browser mode (engine commands move to native IPC in Phase 3).
//!
//! Native commands:
//! * `engine_ready` — the BEBOK_READY line.
//! * `export_chat_history(chat, format)` — streams
//!   `GET /chats/:jid/history/export` from the sidecar into the user's
//!   Downloads folder (WebViews do not reliably honour `<a download>` for blob
//!   URLs). Progress still arrives as `history_export_progress` over SSE.
//! * `group_*`, `newsletter_*`, `presence_*`, `privacy_*` — thin JSON proxies
//!   for the Phase 5 endpoints (groups, newsletters, presence, privacy). They
//!   exist for native callers; the Angular client reaches the same routes over
//!   HTTP (`whatsapp.service.ts`) until engine commands move to native IPC.
//!
//! Shell events: `engine-ready` (payload: the BEBOK_READY line) and
//! `engine-exit` (payload: exit code or `null`).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};
use tauri_plugin_shell::ShellExt;
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

const READY_PREFIX: &str = "BEBOK_READY ";
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const EXPORT_FORMATS: [&str; 3] = ["json", "jsonl", "csv"];

struct Engine {
    ready: watch::Receiver<Option<String>>,
    child: Mutex<Option<CommandChild>>,
}

/// Extracts the BEBOK_READY line from one chunk of sidecar stdout.
fn parse_ready(chunk: &str) -> Option<String> {
    chunk
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with(READY_PREFIX))
        .map(str::to_string)
}

async fn wait_ready(engine: &Engine) -> Result<String, String> {
    let mut rx = engine.ready.clone();
    let wait = rx.wait_for(|v| v.is_some());
    match tokio::time::timeout(READY_TIMEOUT, wait).await {
        Ok(Ok(line)) => Ok(line.clone().unwrap_or_default()),
        Ok(Err(_)) => Err("engine sidecar exited before it was ready".into()),
        Err(_) => Err(format!(
            "engine sidecar not ready after {}s",
            READY_TIMEOUT.as_secs()
        )),
    }
}

/// Returns the full BEBOK_READY line, waiting for the sidecar if needed.
#[tauri::command]
async fn engine_ready(engine: State<'_, Engine>) -> Result<String, String> {
    wait_ready(&engine).await
}

/// Splits the BEBOK_READY line into the engine base URL (no query) and the
/// capability token it carries.
fn engine_base(ready_line: &str) -> Result<(reqwest::Url, Option<String>), String> {
    let raw = ready_line
        .trim()
        .strip_prefix(READY_PREFIX)
        .unwrap_or(ready_line)
        .trim();
    let base = reqwest::Url::parse(raw).map_err(|e| format!("invalid engine URL {raw:?}: {e}"))?;
    let token = base
        .query_pairs()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
        .filter(|t| !t.is_empty());
    let mut url = base;
    url.set_query(None);
    Ok((url, token))
}

/// `base` + path `segments` (each percent-encoded) + the query pairs.
fn endpoint_url(
    base: &reqwest::Url,
    segments: &[&str],
    query: &[(&str, String)],
) -> Result<reqwest::Url, String> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|_| format!("engine URL cannot be a base: {base}"))?
        .clear()
        .extend(segments);
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            pairs.append_pair(key, value);
        }
    }
    Ok(url)
}

/// `GET /chats/:jid/history/export` as a full URL plus the capability token.
fn export_request(
    ready_line: &str,
    chat: &str,
    format: &str,
) -> Result<(reqwest::Url, Option<String>), String> {
    let (base, token) = engine_base(ready_line)?;
    let url = endpoint_url(
        &base,
        &["chats", chat, "history", "export"],
        &[("format", format.to_string())],
    )?;
    Ok((url, token))
}

/// `attachment; filename="chat-1@g.us.json"` → `chat-1@g.us.json`.
fn filename_from_disposition(header: &str) -> Option<String> {
    header.split(';').map(str::trim).find_map(|part| {
        let (key, value) = part.split_once('=')?;
        if !key.trim().eq_ignore_ascii_case("filename") {
            return None;
        }
        let name = value.trim().trim_matches('"').trim();
        (!name.is_empty()).then(|| name.to_string())
    })
}

/// Keeps only a safe file name: no directories, no separators or control chars.
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if cleaned.is_empty() {
        "chat-export".to_string()
    } else {
        cleaned.to_string()
    }
}

/// `dir/name.ext`, or `dir/name (1).ext`, `dir/name (2).ext`, … if taken.
fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (filename, String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("unbounded range always yields a free name")
}

#[derive(Debug, Serialize)]
struct ExportResult {
    filename: String,
    path: String,
    size: u64,
}

/// "Download full chat history" in Tauri mode: streams the export from the
/// sidecar into the Downloads folder under the `Content-Disposition` name.
#[tauri::command]
async fn export_chat_history(
    app: AppHandle,
    engine: State<'_, Engine>,
    chat: String,
    format: Option<String>,
) -> Result<ExportResult, String> {
    let format = format.unwrap_or_else(|| "json".into());
    if !EXPORT_FORMATS.contains(&format.as_str()) {
        return Err(format!("unknown export format: {format}"));
    }
    let line = wait_ready(&engine).await?;
    let (url, token) = export_request(&line, &chat, &format)?;

    let mut req = reqwest::Client::new().get(url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let mut res = req
        .send()
        .await
        .map_err(|e| format!("engine unreachable: {e}"))?;
    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| status.to_string());
        return Err(message);
    }

    let filename = res
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(filename_from_disposition)
        .unwrap_or_else(|| format!("chat-{chat}.{format}"));
    let dir = app
        .path()
        .download_dir()
        .or_else(|_| app.path().document_dir())
        .map_err(|e| format!("no Downloads folder: {e}"))?;
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = unique_path(&dir, &sanitize_filename(&filename));

    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let mut size = 0u64;
    let written: Result<(), String> = async {
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|e| format!("export stream failed: {e}"))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("write failed: {e}"))?;
            size += chunk.len() as u64;
        }
        file.flush().await.map_err(|e| format!("write failed: {e}"))
    }
    .await;
    if let Err(e) = written {
        drop(file);
        let _ = tokio::fs::remove_file(&path).await; // don't leave a truncated export behind
        return Err(e);
    }

    Ok(ExportResult {
        filename: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: path.display().to_string(),
        size,
    })
}

/// One authenticated JSON call against the engine sidecar. The reply is
/// parsed as JSON; a `{ "error": … }` body becomes the command's `Err`.
async fn engine_call(
    engine: &Engine,
    method: reqwest::Method,
    segments: &[&str],
    query: &[(&str, String)],
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let line = wait_ready(engine).await?;
    let (base, token) = engine_base(&line)?;
    let url = endpoint_url(&base, segments, query)?;
    let mut req = reqwest::Client::new().request(method, url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    // `default-features = false` keeps reqwest's `json` feature off, so the
    // body is serialized by hand.
    if let Some(body) = body {
        let text =
            serde_json::to_string(&body).map_err(|e| format!("cannot encode the request: {e}"))?;
        req = req
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(text);
    }
    let res = req
        .send()
        .await
        .map_err(|e| format!("engine unreachable: {e}"))?;
    let status = res.status();
    let text = res
        .text()
        .await
        .map_err(|e| format!("engine reply unreadable: {e}"))?;
    if !status.is_success() {
        let message = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| status.to_string());
        return Err(message);
    }
    if text.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| format!("engine reply is not JSON: {e}"))
}

/// `POST /group/create` — `{name, participants[]}` → the new `GroupInfo`.
#[tauri::command]
async fn group_create(
    engine: State<'_, Engine>,
    name: String,
    participants: Vec<String>,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "name": name, "participants": participants });
    engine_call(
        &engine,
        reqwest::Method::POST,
        &["group", "create"],
        &[],
        Some(body),
    )
    .await
}

/// `GET /group/{jid}/info` → `GroupInfo`.
#[tauri::command]
async fn group_info(engine: State<'_, Engine>, jid: String) -> Result<serde_json::Value, String> {
    engine_call(
        &engine,
        reqwest::Method::GET,
        &["group", &jid, "info"],
        &[],
        None,
    )
    .await
}

/// `POST /group/{jid}/leave`.
#[tauri::command]
async fn group_leave(engine: State<'_, Engine>, jid: String) -> Result<serde_json::Value, String> {
    engine_call(
        &engine,
        reqwest::Method::POST,
        &["group", &jid, "leave"],
        &[],
        Some(serde_json::json!({})),
    )
    .await
}

/// `POST /group/{jid}/participants` — add/remove/promote/demote.
#[tauri::command]
async fn group_participants(
    engine: State<'_, Engine>,
    jid: String,
    action: String,
    participants: Vec<String>,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "action": action, "participants": participants });
    engine_call(
        &engine,
        reqwest::Method::POST,
        &["group", &jid, "participants"],
        &[],
        Some(body),
    )
    .await
}

/// `POST /group/{jid}/name`.
#[tauri::command]
async fn group_set_name(
    engine: State<'_, Engine>,
    jid: String,
    name: String,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "name": name });
    engine_call(
        &engine,
        reqwest::Method::POST,
        &["group", &jid, "name"],
        &[],
        Some(body),
    )
    .await
}

/// `POST /group/{jid}/topic`.
#[tauri::command]
async fn group_set_topic(
    engine: State<'_, Engine>,
    jid: String,
    topic: String,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "topic": topic });
    engine_call(
        &engine,
        reqwest::Method::POST,
        &["group", &jid, "topic"],
        &[],
        Some(body),
    )
    .await
}

/// `GET /group/{jid}/invite-link?reset=…` → `{link}`.
#[tauri::command]
async fn group_invite_link(
    engine: State<'_, Engine>,
    jid: String,
    reset: Option<bool>,
) -> Result<serde_json::Value, String> {
    let query: &[(&str, String)] = if reset.unwrap_or(false) {
        &[("reset", "true".to_string())]
    } else {
        &[]
    };
    engine_call(
        &engine,
        reqwest::Method::GET,
        &["group", &jid, "invite-link"],
        query,
        None,
    )
    .await
}

/// `POST /group/join` — join through an invite code or link.
#[tauri::command]
async fn group_join(engine: State<'_, Engine>, code: String) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "code": code });
    engine_call(
        &engine,
        reqwest::Method::POST,
        &["group", "join"],
        &[],
        Some(body),
    )
    .await
}

/// `POST /newsletter/subscribe`.
#[tauri::command]
async fn newsletter_subscribe(
    engine: State<'_, Engine>,
    id: String,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "id": id });
    engine_call(
        &engine,
        reqwest::Method::POST,
        &["newsletter", "subscribe"],
        &[],
        Some(body),
    )
    .await
}

/// `POST /newsletter/unsubscribe`.
#[tauri::command]
async fn newsletter_unsubscribe(
    engine: State<'_, Engine>,
    id: String,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "id": id });
    engine_call(
        &engine,
        reqwest::Method::POST,
        &["newsletter", "unsubscribe"],
        &[],
        Some(body),
    )
    .await
}

/// `POST /presence/subscribe`.
#[tauri::command]
async fn presence_subscribe(
    engine: State<'_, Engine>,
    jid: String,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "jid": jid });
    engine_call(
        &engine,
        reqwest::Method::POST,
        &["presence", "subscribe"],
        &[],
        Some(body),
    )
    .await
}

/// `POST /presence/send` — `state` is `available` or `unavailable`.
#[tauri::command]
async fn presence_send(
    engine: State<'_, Engine>,
    state: String,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "type": state });
    engine_call(
        &engine,
        reqwest::Method::POST,
        &["presence", "send"],
        &[],
        Some(body),
    )
    .await
}

/// `POST /presence/chat` — `state` is `composing` or `paused`, `media` optional.
#[tauri::command]
async fn presence_chat(
    engine: State<'_, Engine>,
    chat: String,
    state: String,
    media: Option<String>,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "chat": chat, "state": state, "media": media });
    engine_call(
        &engine,
        reqwest::Method::POST,
        &["presence", "chat"],
        &[],
        Some(body),
    )
    .await
}

/// `GET /privacy` → `PrivacySettings`.
#[tauri::command]
async fn privacy_get(engine: State<'_, Engine>) -> Result<serde_json::Value, String> {
    engine_call(&engine, reqwest::Method::GET, &["privacy"], &[], None).await
}

/// `POST /privacy` — change one setting (`name`/`value`).
#[tauri::command]
async fn privacy_set(
    engine: State<'_, Engine>,
    name: String,
    value: String,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "name": name, "value": value });
    engine_call(
        &engine,
        reqwest::Method::POST,
        &["privacy"],
        &[],
        Some(body),
    )
    .await
}

fn spawn_engine(app: &AppHandle) -> Result<Engine, Box<dyn std::error::Error>> {
    let (tx, rx) = watch::channel(None::<String>);
    let (mut events, child) = app
        .shell()
        .sidecar("bebok-server")?
        .args(["--port", "0"])
        .spawn()?;

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut tx = Some(tx);
        while let Some(event) = events.recv().await {
            match event {
                CommandEvent::Stdout(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    if let Some(line) = parse_ready(&text) {
                        let _ = handle.emit("engine-ready", &line);
                        if let Some(tx) = tx.as_ref() {
                            let _ = tx.send(Some(line));
                        }
                    }
                }
                // Engine logs go to stderr — forward them to ours.
                CommandEvent::Stderr(bytes) => {
                    eprint!("[engine] {}", String::from_utf8_lossy(&bytes))
                }
                CommandEvent::Error(err) => eprintln!("[engine] error: {err}"),
                CommandEvent::Terminated(payload) => {
                    eprintln!("[engine] exited with {:?}", payload.code);
                    let _ = handle.emit("engine-exit", payload.code);
                    // Dropping the sender wakes pending `engine_ready` calls with an error.
                    tx.take();
                }
                _ => {}
            }
        }
    });

    Ok(Engine {
        ready: rx,
        child: Mutex::new(Some(child)),
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let engine = spawn_engine(app.handle())?;
            app.manage(engine);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            engine_ready,
            export_chat_history,
            group_create,
            group_info,
            group_leave,
            group_participants,
            group_set_name,
            group_set_topic,
            group_invite_link,
            group_join,
            newsletter_subscribe,
            newsletter_unsubscribe,
            presence_subscribe,
            presence_send,
            presence_chat,
            privacy_get,
            privacy_set
        ])
        .build(tauri::generate_context!())
        .expect("error while building the Tauri application")
        .run(|app, event| {
            if let RunEvent::Exit = event
                && let Some(engine) = app.try_state::<Engine>()
                && let Some(child) = engine.child.lock().unwrap().take()
            {
                let _ = child.kill();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_ready_line_in_stdout_chunk() {
        assert_eq!(
            parse_ready("noise\nBEBOK_READY http://127.0.0.1:41234/?token=abc\r\n").as_deref(),
            Some("BEBOK_READY http://127.0.0.1:41234/?token=abc")
        );
        assert_eq!(parse_ready("starting…"), None);
    }

    #[test]
    fn builds_export_url_with_encoded_jid_and_token() {
        let (url, token) = export_request(
            "BEBOK_READY http://127.0.0.1:41234/?token=abc",
            "1@g.us",
            "csv",
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:41234/chats/1@g.us/history/export?format=csv"
        );
        assert_eq!(token.as_deref(), Some("abc"));

        let (url, token) = export_request("http://127.0.0.1:1/", "a/b c", "json").unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:1/chats/a%2Fb%20c/history/export?format=json"
        );
        assert_eq!(token, None);
        assert!(export_request("BEBOK_READY not a url", "x", "json").is_err());
    }

    #[test]
    fn splits_the_ready_line_into_base_and_token() {
        let (base, token) =
            engine_base("BEBOK_READY http://127.0.0.1:41234/?token=abc").expect("a valid line");
        assert_eq!(base.as_str(), "http://127.0.0.1:41234/");
        assert_eq!(token.as_deref(), Some("abc"));

        let (base, token) = engine_base("http://127.0.0.1:1/").expect("a bare URL");
        assert_eq!(base.as_str(), "http://127.0.0.1:1/");
        assert_eq!(token, None);
        assert!(engine_base("BEBOK_READY not a url").is_err());
    }

    #[test]
    fn builds_group_and_presence_urls_from_segments() {
        let (base, _) =
            engine_base("BEBOK_READY http://127.0.0.1:41234/?token=abc").expect("a valid line");
        let url = endpoint_url(&base, &["group", "1@g.us", "info"], &[]).expect("a url");
        assert_eq!(url.as_str(), "http://127.0.0.1:41234/group/1@g.us/info");

        let url = endpoint_url(
            &base,
            &["group", "1@g.us", "invite-link"],
            &[("reset", "true".to_string())],
        )
        .expect("a url");
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:41234/group/1@g.us/invite-link?reset=true"
        );

        let url = endpoint_url(&base, &["privacy"], &[]).expect("a url");
        assert_eq!(url.as_str(), "http://127.0.0.1:41234/privacy");
    }

    #[test]
    fn reads_and_sanitizes_disposition_filename() {
        assert_eq!(
            filename_from_disposition("attachment; filename=\"chat-1@g.us.jsonl\"").as_deref(),
            Some("chat-1@g.us.jsonl")
        );
        assert_eq!(filename_from_disposition("inline"), None);
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("a:b?.csv"), "a_b_.csv");
        assert_eq!(sanitize_filename(".."), "chat-export");
    }

    #[test]
    fn unique_path_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("wa-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "c.json"), dir.join("c.json"));
        std::fs::write(dir.join("c.json"), b"x").unwrap();
        std::fs::write(dir.join("c (1).json"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "c.json"), dir.join("c (2).json"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
