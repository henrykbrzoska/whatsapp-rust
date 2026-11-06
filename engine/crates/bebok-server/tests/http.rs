use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bebok_server::{Config, routes, state::AppState};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn app(extra: &[&str]) -> axum::Router {
    let mut args = vec!["--token", "secret", "--diagnostic"];
    args.extend_from_slice(extra);
    let cfg = Config::parse(args.iter().map(|s| s.to_string()), |_| None).unwrap();
    routes::router(AppState::memory(cfg).await.unwrap())
}

async fn call(app: &axum::Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).header(header::AUTHORIZATION, "Bearer secret").body(Body::empty()).unwrap()
}

fn post(uri: &str) -> Request<Body> {
    Request::post(uri).header(header::AUTHORIZATION, "Bearer secret").body(Body::empty()).unwrap()
}

#[tokio::test]
async fn version_requires_token() {
    let app = app(&[]).await;
    let (st, _, _) = call(&app, Request::get("/version").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _, _) = call(&app, Request::get("/version?token=nope").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    let (st, _, body) = call(&app, get("/version")).await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));

    let (st, _, _) = call(&app, Request::get("/version?token=secret").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn config_redacts_token() {
    let (_, _, body) = call(&app(&[]).await, get("/config")).await;
    assert!(!body.contains("secret"));
    assert!(body.contains("<redacted>"));
}

#[tokio::test]
async fn no_auth_mode() {
    let cfg = Config::parse(["--no-auth".to_string()], |_| None).unwrap();
    let app = routes::router(AppState::memory(cfg).await.unwrap());
    let (st, _, _) = call(&app, Request::get("/version").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn cors_preflight_for_dev_client() {
    let app = app(&[]).await;
    let req = Request::options("/version")
        .header(header::ORIGIN, "http://localhost:4200")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
        .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization")
        .body(Body::empty())
        .unwrap();
    let (st, h, _) = call(&app, req).await;
    assert!(st.is_success());
    assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], "http://localhost:4200");
}

async fn seed(app: &axum::Router) {
    let msgs = json!([
        {"id":"m1","chat_jid":"1@g.us","sender":"a@s.whatsapp.net","timestamp":1,"type":"text","text":"test message"},
        {"id":"m2","chat_jid":"1@g.us","sender":"b@s.whatsapp.net","timestamp":2,"type":"text","text":"second, \"quoted\""},
        {"id":"m3","chat_jid":"2@g.us","sender":"b@s.whatsapp.net","timestamp":3,"type":"text","text":"another test"}
    ]);
    let req = Request::post("/debug/seed")
        .header(header::AUTHORIZATION, "Bearer secret")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(msgs.to_string()))
        .unwrap();
    let (st, _, _) = call(app, req).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn search_endpoint() {
    let app = app(&[]).await;
    let (st, _, body) = call(&app, get("/search?q=test")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["results"], json!([]));

    seed(&app).await;
    let (_, _, body) = call(&app, get("/search?q=test&mode=keyword")).await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["results"].as_array().unwrap().len(), 2);
    let (_, _, body) = call(&app, get("/search?q=test&chat=2%40g.us")).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["results"][0]["message"]["id"], "m3");

    let (st, _, _) = call(&app, get("/search?q=test&mode=nope")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn export_stats_delete() {
    let app = app(&[]).await;
    seed(&app).await;

    let (st, h, body) = call(&app, get("/chats/1@g.us/history/export?format=json")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h[header::CONTENT_DISPOSITION], "attachment; filename=\"chat-1@g.us.json\"");
    assert_eq!(h["x-total-count"], "2");
    let v: Vec<Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(v.len(), 2);

    let (_, h, body) = call(&app, get("/chats/1@g.us/history/export?format=csv")).await;
    assert!(h[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/csv"));
    assert_eq!(body.lines().next().unwrap(), "id,chat_jid,sender,timestamp,type,text");

    let (_, _, body) = call(&app, get("/chats/1@g.us/history/export?format=jsonl&from=2")).await;
    assert_eq!(body.lines().count(), 1);

    let (st, _, _) = call(&app, get("/chats/1@g.us/history/export?format=xml")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (_, _, body) = call(&app, get("/chats/1@g.us/stats")).await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["count"], 2);

    let req = Request::delete("/chats/1@g.us/messages")
        .header(header::AUTHORIZATION, "Bearer secret")
        .body(Body::empty())
        .unwrap();
    let (_, _, body) = call(&app, req).await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["deleted"], 2);
    let (_, _, body) = call(&app, get("/chats/1@g.us/history/export")).await;
    assert_eq!(body, "[]");
}

#[tokio::test]
async fn list_chats_endpoint() {
    let app = app(&[]).await;
    let (st, _, body) = call(&app, get("/chats")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), json!([]));

    seed(&app).await;
    let (_, _, body) = call(&app, get("/chats")).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v[0]["jid"], "2@g.us");
    assert_eq!(v[1]["jid"], "1@g.us");
    assert_eq!(v[1]["count"], 2);
    assert_eq!(v[1]["last_text"], "second, \"quoted\"");
}

#[tokio::test]
async fn debug_routes_are_always_on() {
    let cfg = Config::parse(["--token".into(), "secret".into()], |_| None).unwrap();
    let app = routes::router(AppState::memory(cfg).await.unwrap());
    let req = Request::post("/debug/seed")
        .header(header::AUTHORIZATION, "Bearer secret")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("[]"))
        .unwrap();
    let (st, _, _) = call(&app, req).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _, _) = call(&app, get("/debug/log")).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn connect_requires_a_token() {
    let app = app(&[]).await;
    let (st, _, _) = call(&app, Request::post("/connect").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn disconnect_reports_the_state_and_logout_is_still_unimplemented() {
    let app = app(&[]).await;
    let (st, _, body) = call(&app, post("/disconnect")).await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["state"], "disconnected");
    assert_eq!(v["connected"], false);

    // `/logout` with nothing paired answers 400 (the protocol client now lives
    // in `Connection`; see `routes::groups::session`).
    let (st, _, _) = call(&app, post("/logout")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

/// `POST /connect` must answer *before* the handshake finishes, and the failure
/// of the background attempt must show up as state (`disconnected` + `error`),
/// never as a hanging request.
#[tokio::test]
async fn connect_starts_the_session_and_does_not_block() {
    // Nothing listens on this port: the dial fails, so the attempt cannot
    // silently succeed, and the test stays offline.
    let app = app(&["--wa-url", "ws://127.0.0.1:1", "--handshake-timeout", "1"]).await;

    let req = Request::post("/connect").header(header::AUTHORIZATION, "Bearer secret").body(Body::empty()).unwrap();
    let (st, _, body) = tokio::time::timeout(std::time::Duration::from_secs(5), call(&app, req))
        .await
        .expect("POST /connect must not block on the session");
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(matches!(v["state"].as_str(), Some("connecting" | "disconnected")), "{body}");
    assert_eq!(v["connected"], false);
    assert_eq!(v["loggedIn"], false);
    assert_eq!(v["attempts"], 1);
    assert!(v.get("error").is_some(), "the error key is always present");

    // The background attempt fails against a closed port and is reported on
    // the SSE stream (`API-WHATSAPP.md` §2), not in the HTTP response.
    let event = tokio::time::timeout(std::time::Duration::from_secs(10), first_event(&app, "connect_failure"))
        .await
        .expect("the failing attempt must emit `connect_failure`");
    assert!(event.contains("connect_failure"), "{event}");

    let (_, _, body) = call(&app, get("/connected")).await;
    assert_eq!(body, "false");
}

/// `GET /events` is a `text/event-stream`; this reads it until an event whose
/// `type` matches `wanted` shows up (or the stream ends).
async fn first_event(app: &axum::Router, wanted: &str) -> String {
    let res = app.clone().oneshot(get("/events")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE].to_str().unwrap(), "text/event-stream");
    let mut body = res.into_body().into_data_stream();
    let mut seen = String::new();
    while let Some(chunk) = futures::StreamExt::next(&mut body).await {
        let Ok(chunk) = chunk else { break };
        seen.push_str(&String::from_utf8_lossy(&chunk));
        if seen.contains(wanted) {
            break;
        }
    }
    seen
}

/// A second `POST /connect` while one is in flight must not stack sessions.
#[tokio::test]
async fn connect_is_idempotent_while_a_session_exists() {
    // A listener that accepts TCP but never completes the WebSocket upgrade
    // keeps the attempt pending, so "already connecting" is deterministic.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });

    let app = app(&["--wa-url", &format!("ws://127.0.0.1:{port}"), "--handshake-timeout", "30"]).await;
    let first: Value = serde_json::from_str(&call(&app, post("/connect")).await.2).unwrap();
    let second: Value = serde_json::from_str(&call(&app, post("/connect")).await.2).unwrap();
    assert_eq!(first["state"], "connecting");
    assert_eq!(first["attempts"], 1);
    assert_eq!(second["state"], "connecting");
    assert_eq!(second["attempts"], 1, "a second connect must reuse the running attempt");
}
