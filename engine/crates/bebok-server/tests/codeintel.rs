//! Code intelligence endpoints over HTTP, mounted exactly as they are meant to
//! be registered in `routes::router` (without the auth layer, which
//! `tests/http.rs` covers).
//!
//! One test only: `BEBOK_PROJECT_ROOT` is process-global.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use bebok_server::routes::codeintel;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> Router {
    Router::new()
        .route("/code-index/status", get(codeintel::index_status))
        .route("/code-index/search", get(codeintel::index_search_get).post(codeintel::index_search_post))
        .route("/code-ast/status", get(codeintel::ast_status))
        .route("/code-ast", get(codeintel::ast_search))
        .route("/code-graph", get(codeintel::graph))
        .route("/code-map", get(codeintel::map))
}

async fn call(req: Request<Body>) -> (StatusCode, Value) {
    let res = app().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn get_json(uri: &str) -> (StatusCode, Value) {
    call(Request::get(uri).body(Body::empty()).unwrap()).await
}

#[tokio::test]
async fn code_intelligence_endpoints() {
    let d = tempfile::tempdir().unwrap();
    let w = |p: &str, c: &str| {
        let p = d.path().join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    };
    w("engine/src/lib.rs", "pub mod plugin_process;\n");
    w("engine/src/plugin_process.rs", "pub struct PluginProcess;\nimpl PluginProcess {\n    pub fn spawn() {}\n}\n");
    w("client/app.ts", "import { X } from './x';\nexport class App {}\n");
    w("client/x.ts", "export const X = 1;\n");
    // SAFETY: single test in this binary; nothing else reads the env concurrently.
    unsafe { std::env::set_var("BEBOK_PROJECT_ROOT", d.path()) };

    let (s, v) = get_json("/code-index/status").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["ok"], true);
    assert_eq!(v["status"], "ready");
    assert_eq!(v["files"], 4);
    assert!(v["symbols"].as_u64().unwrap() >= 5, "{v}");

    // POST, Bebok plugin shape.
    let req = Request::post("/code-index/search")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "query": "PluginProcess spawn", "limit": 5, "extension": "rs" }).to_string()))
        .unwrap();
    let (s, v) = call(req).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["results"][0]["path"], "engine/src/plugin_process.rs", "{v}");
    assert!(v["results"][0]["score"].as_f64().unwrap() > 0.0);

    // GET variant + directory scoping.
    let (s, v) = get_json("/code-index/search?q=App&directory=client").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["results"][0]["path"], "app.ts", "{v}");
    let (s, _) = get_json("/code-index/search").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = get_json("/code-index/status?directory=..").await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let (s, v) = get_json("/code-ast/status").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["enabled"], true);
    assert_eq!(v["kinds"]["struct"], 1);

    let (_, v) = get_json("/code-ast?kind=fn&name=spa").await;
    assert_eq!(
        v["results"],
        json!([{ "path": "engine/src/plugin_process.rs", "kind": "fn", "name": "spawn", "line": 3 }])
    );

    let (_, v) = get_json("/code-graph").await;
    let edges = v["edges"].as_array().unwrap();
    assert!(edges.contains(&json!({ "from": "engine/src/lib.rs", "to": "engine/src/plugin_process.rs" })), "{v}");
    assert!(edges.contains(&json!({ "from": "client/app.ts", "to": "client/x.ts" })));
    let (_, v) = get_json("/code-graph?file=client/x.ts").await;
    assert_eq!(v["dependents"], json!(["client/app.ts"]));

    let (_, v) = get_json("/code-map?path=client").await;
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f["path"].as_str().unwrap().starts_with("client/")));

    // Refresh picks up new files.
    w("client/y.ts", "export function y() {}\n");
    let (_, v) = get_json("/code-index/status?refresh=true").await;
    assert_eq!(v["files"], 5);
}
