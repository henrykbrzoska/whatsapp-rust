//! Code intelligence endpoints (`AGENTS.md` §10, Bebok-style).
//!
//! | Method   | Path                  | Handler          |
//! |----------|-----------------------|------------------|
//! | GET      | `/code-index/status`  | [`index_status`] |
//! | GET/POST | `/code-index/search`  | [`index_search_get`] / [`index_search_post`] |
//! | GET      | `/code-ast/status`    | [`ast_status`]   |
//! | GET      | `/code-ast`           | [`ast_search`]   |
//! | GET      | `/code-graph`         | [`graph`]        |
//! | GET      | `/code-map`           | [`map`]          |
//!
//! Everything is offline, read-only and in-memory: the first request for a
//! project root walks its source files (std only, no new deps), extracts
//! symbols and import edges with line heuristics and caches the result.
//! `?refresh=true` on `/code-index/status` rebuilds it.
//!
//! The project root comes from `BEBOK_PROJECT_ROOT` (falls back to the current
//! directory). An optional `?directory=` must resolve inside that root (path
//! guard), otherwise the request answers 403.
//!
//! Response shapes follow the Bebok index plugin:
//! status → `{ok, status, files, symbols}`, search → `{ok, results: [{path, score}]}`.

#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use axum::Json;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::routes::error;

/// Directories never indexed.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".bebok",
    ".angular",
    "dist",
    "build",
    "out",
    "coverage",
    ".next",
    ".cache",
    "gen",
];
/// Indexed file extensions.
const EXTENSIONS: &[&str] = &[
    "rs", "go", "ts", "tsx", "js", "mjs", "cjs", "jsx", "html", "scss", "css", "json", "toml", "md", "proto", "sql",
    "sh", "yml", "yaml",
];
/// Files larger than this are skipped (generated / vendored blobs).
const MAX_FILE_BYTES: u64 = 512 * 1024;
/// Hard cap on indexed files to keep the first build bounded.
const MAX_FILES: usize = 50_000;
const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 100;

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Symbol {
    pub kind: &'static str,
    pub name: String,
    pub line: usize,
}

#[derive(Debug)]
pub struct FileEntry {
    /// Path relative to the root, `/`-separated.
    pub path: String,
    pub ext: String,
    lower: String,
    pub lines: usize,
    pub symbols: Vec<Symbol>,
    /// Raw import / module specifiers found in the file.
    imports: Vec<String>,
}

#[derive(Debug)]
pub struct CodeIndex {
    pub root: PathBuf,
    pub files: Vec<FileEntry>,
}

impl CodeIndex {
    /// Walk `root` and index every source file.
    pub fn build(root: &Path) -> CodeIndex {
        let mut files = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            let mut entries: Vec<_> = rd.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let Ok(ft) = e.file_type() else { continue };
                let name = e.file_name().to_string_lossy().into_owned();
                let p = e.path();
                if ft.is_dir() {
                    if !SKIP_DIRS.contains(&name.as_str()) {
                        stack.push(p);
                    }
                    continue;
                }
                if !ft.is_file() || files.len() >= MAX_FILES {
                    continue;
                }
                let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
                if !EXTENSIONS.contains(&ext.as_str()) {
                    continue;
                }
                if e.metadata().map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(&p) else { continue };
                let rel = rel_path(root, &p);
                files.push(FileEntry::new(rel, ext, &content));
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        CodeIndex { root: root.to_path_buf(), files }
    }

    pub fn symbol_count(&self) -> usize {
        self.files.iter().map(|f| f.symbols.len()).sum()
    }

    /// Full-text search: exact phrase → AND of terms → OR of terms.
    pub fn search(&self, query: &str, limit: usize, ext: Option<&str>) -> Vec<SearchHit> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let terms: Vec<&str> = q.split_whitespace().collect();
        let ext = ext.map(|e| e.trim_start_matches('.').to_ascii_lowercase()).filter(|e| !e.is_empty());
        let candidates: Vec<&FileEntry> =
            self.files.iter().filter(|f| ext.as_deref().is_none_or(|e| f.ext == e)).collect();

        let score = |f: &FileEntry, mode: Mode| -> Option<f64> {
            let path = f.path.to_lowercase();
            let tf = |t: &str| f.lower.matches(t).count() as f64 + if path.contains(t) { 10.0 } else { 0.0 };
            let s = match mode {
                Mode::Phrase => {
                    let n = tf(&q);
                    if n == 0.0 {
                        return None;
                    }
                    n * 2.0
                }
                Mode::And => {
                    let mut s = 0.0;
                    for t in &terms {
                        let n = tf(t);
                        if n == 0.0 {
                            return None;
                        }
                        s += n;
                    }
                    s
                }
                Mode::Or => {
                    let s: f64 = terms.iter().map(|t| tf(t)).sum();
                    if s == 0.0 {
                        return None;
                    }
                    s
                }
            };
            // Symbol-name matches weigh most; normalise by length (BM25-ish).
            let sym = f
                .symbols
                .iter()
                .filter(|s| {
                    let n = s.name.to_lowercase();
                    terms.iter().any(|t| n.contains(t))
                })
                .count() as f64;
            let norm = 1.0 + (f.lines as f64 / 200.0);
            Some((s / norm + sym * 5.0) * 100.0).map(|v| v.round() / 100.0)
        };

        let modes: &[Mode] = if terms.len() > 1 { &[Mode::Phrase, Mode::And, Mode::Or] } else { &[Mode::Phrase] };
        for &mode in modes {
            let mut hits: Vec<SearchHit> = candidates
                .iter()
                .filter_map(|f| score(f, mode).map(|score| SearchHit { path: f.path.clone(), score }))
                .collect();
            if hits.is_empty() {
                continue;
            }
            hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
            hits.truncate(limit);
            return hits;
        }
        Vec::new()
    }

    /// Module dependency edges between indexed files (Rust `mod`/`use crate::`,
    /// TS/JS relative imports, Go is skipped — package-level only).
    pub fn edges(&self) -> Vec<(String, String)> {
        let known: HashMap<&str, ()> = self.files.iter().map(|f| (f.path.as_str(), ())).collect();
        let mut out = Vec::new();
        for f in &self.files {
            for spec in &f.imports {
                for target in resolve(&f.path, &f.ext, spec) {
                    if target != f.path && known.contains_key(target.as_str()) {
                        out.push((f.path.clone(), target));
                        break;
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Phrase,
    And,
    Or,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchHit {
    pub path: String,
    pub score: f64,
}

impl FileEntry {
    fn new(path: String, ext: String, content: &str) -> FileEntry {
        let mut symbols = Vec::new();
        let mut imports = Vec::new();
        let mut lines = 0;
        for (i, raw) in content.lines().enumerate() {
            lines = i + 1;
            let line = raw.trim_start();
            if let Some((kind, name)) = symbol_of(&ext, line) {
                symbols.push(Symbol { kind, name, line: i + 1 });
            }
            if let Some(spec) = import_of(&ext, line) {
                imports.push(spec);
            }
        }
        FileEntry { path, lower: content.to_lowercase(), ext, lines, symbols, imports }
    }
}

// ---------------------------------------------------------------------------
// Heuristic parsers
// ---------------------------------------------------------------------------

fn ident(s: &str) -> Option<String> {
    let id: String = s.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect();
    (!id.is_empty()).then_some(id)
}

/// Strip leading visibility / modifier keywords.
fn strip_mods<'a>(mut line: &'a str, mods: &[&str]) -> &'a str {
    loop {
        let before = line;
        for m in mods {
            if let Some(rest) = line.strip_prefix(m)
                && rest.starts_with([' ', '('])
            {
                // `pub(crate) fn` → skip the parenthesised part too.
                let rest = if let Some(r) = rest.strip_prefix('(') {
                    r.split_once(')').map(|(_, r)| r).unwrap_or(r)
                } else {
                    rest
                };
                line = rest.trim_start();
            }
        }
        if line == before {
            return line;
        }
    }
}

fn symbol_of(ext: &str, line: &str) -> Option<(&'static str, String)> {
    let kinds: &[(&str, &'static str)] = match ext {
        "rs" => {
            let l = strip_mods(line, &["pub", "async", "unsafe", "const", "extern", "default"]);
            if let Some(r) = l.strip_prefix("impl") {
                // `impl<T> Trait for Type` / `impl Type`
                let r = r.trim_start();
                let r =
                    if r.starts_with('<') { r.split_once('>').map(|(_, r)| r.trim_start()).unwrap_or(r) } else { r };
                let target = r.split(" for ").last().unwrap_or(r).trim_start();
                return ident(target).map(|n| ("impl", n));
            }
            for (kw, kind) in [
                ("fn ", "fn"),
                ("struct ", "struct"),
                ("enum ", "enum"),
                ("trait ", "trait"),
                ("type ", "type"),
                ("mod ", "mod"),
                ("macro_rules! ", "macro"),
            ] {
                if let Some(r) = l.strip_prefix(kw) {
                    return ident(r.trim_start()).map(|n| (kind, n));
                }
            }
            return None;
        }
        "go" => &[("func ", "fn"), ("type ", "type")],
        "ts" | "tsx" | "js" | "mjs" | "cjs" | "jsx" => {
            let l = strip_mods(line, &["export", "default", "declare", "abstract", "async"]);
            for (kw, kind) in [
                ("function ", "fn"),
                ("function* ", "fn"),
                ("class ", "class"),
                ("interface ", "interface"),
                ("type ", "type"),
                ("enum ", "enum"),
            ] {
                if let Some(r) = l.strip_prefix(kw) {
                    return ident(r.trim_start()).map(|n| (kind, n));
                }
            }
            if line.starts_with("export ") {
                for kw in ["const ", "let "] {
                    if let Some(r) = l.strip_prefix(kw) {
                        return ident(r.trim_start()).map(|n| ("const", n));
                    }
                }
            }
            return None;
        }
        "proto" => &[("message ", "message"), ("enum ", "enum"), ("service ", "service")],
        _ => return None,
    };
    for (kw, kind) in kinds {
        if let Some(r) = line.strip_prefix(kw) {
            let r = r.trim_start();
            // Go methods: `func (c *Client) Connect(` → name after the receiver.
            let r = if *kind == "fn" && r.starts_with('(') {
                r.split_once(')').map(|(_, r)| r.trim_start()).unwrap_or(r)
            } else {
                r
            };
            return ident(r).map(|n| (*kind, n));
        }
    }
    None
}

fn import_of(ext: &str, line: &str) -> Option<String> {
    match ext {
        "rs" => {
            let l = strip_mods(line, &["pub"]);
            if let Some(r) = l.strip_prefix("mod ") {
                let name = ident(r.trim_start())?;
                // `mod x;` only (inline `mod x {` is not a file).
                return r.trim_end().ends_with(';').then(|| format!("mod:{name}"));
            }
            let r = l.strip_prefix("use ")?;
            let path: String = r.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':').collect();
            (path.starts_with("crate::") || path.starts_with("super::")).then_some(path)
        }
        "ts" | "tsx" | "js" | "mjs" | "cjs" | "jsx" => {
            if !(line.starts_with("import ") || line.starts_with("export ")) {
                return None;
            }
            let (_, rest) = line.rsplit_once(" from ")?;
            let rest = rest.trim();
            let q = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
            let spec = rest[1..].split(q).next()?;
            spec.starts_with('.').then(|| spec.to_string())
        }
        _ => None,
    }
}

/// Candidate target paths (relative to root) for an import spec in `from`.
fn resolve(from: &str, ext: &str, spec: &str) -> Vec<String> {
    let dir = from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    match ext {
        "rs" => {
            if let Some(name) = spec.strip_prefix("mod:") {
                let file = from.rsplit('/').next().unwrap_or(from);
                let base = if matches!(file, "mod.rs" | "lib.rs" | "main.rs") {
                    dir.to_string()
                } else {
                    join(dir, file.trim_end_matches(".rs"))
                };
                return vec![join(&base, &format!("{name}.rs")), join(&base, &format!("{name}/mod.rs"))];
            }
            // crate::a::b::Item → <crate>/src/a/b.rs | a/b/mod.rs | a.rs …
            let Some(src) = from.find("src/").map(|i| &from[..i + 3]) else { return Vec::new() };
            let segs: Vec<&str> = spec.split("::").filter(|s| !s.is_empty()).collect();
            let segs: Vec<&str> = if segs.first() == Some(&"crate") {
                segs[1..].to_vec()
            } else {
                // super:: → relative to the parent module dir.
                let file = from.rsplit('/').next().unwrap_or(from);
                let mut base: Vec<&str> = from[src.len()..].trim_start_matches('/').split('/').collect();
                base.pop();
                // In `a/b.rs` the first `super` is `a/` itself; in `a/mod.rs` it is `a/..`.
                let mut pop = matches!(file, "mod.rs");
                let mut rest = &segs[..];
                while rest.first() == Some(&"super") {
                    rest = &rest[1..];
                    if pop {
                        base.pop();
                    }
                    pop = true;
                }
                base.into_iter().chain(rest.iter().copied()).collect()
            };
            let mut out = Vec::new();
            for n in (1..=segs.len()).rev() {
                let p = segs[..n].join("/");
                out.push(format!("{src}/{p}.rs"));
                out.push(format!("{src}/{p}/mod.rs"));
            }
            out
        }
        _ => {
            let base = normalize(&join(dir, spec));
            let mut out = vec![base.clone()];
            for e in ["ts", "tsx", "js", "mjs", "jsx"] {
                out.push(format!("{base}.{e}"));
                out.push(format!("{base}/index.{e}"));
            }
            out
        }
    }
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() { rest.to_string() } else { format!("{dir}/{rest}") }
}

fn normalize(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

fn rel_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

// ---------------------------------------------------------------------------
// Root resolution + cache
// ---------------------------------------------------------------------------

/// The project root all code-intelligence requests are confined to.
pub fn project_root() -> PathBuf {
    let root = std::env::var_os("BEBOK_PROJECT_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    root.canonicalize().unwrap_or(root)
}

/// Resolve `?directory=` against `root`; must stay inside it.
pub fn resolve_directory(root: &Path, directory: Option<&str>) -> Result<PathBuf, Response> {
    let Some(d) = directory.map(str::trim).filter(|d| !d.is_empty()) else { return Ok(root.to_path_buf()) };
    let p = Path::new(d);
    let p = if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
    let canon = p.canonicalize().map_err(|_| error(StatusCode::NOT_FOUND, format!("directory not found: {d}")))?;
    if !canon.starts_with(root) {
        return Err(error(StatusCode::FORBIDDEN, "directory is outside the project root"));
    }
    if !canon.is_dir() {
        return Err(error(StatusCode::BAD_REQUEST, format!("not a directory: {d}")));
    }
    Ok(canon)
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Arc<CodeIndex>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<CodeIndex>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Cached index for `dir` (built on a blocking thread on first use).
pub async fn index_for(dir: PathBuf, refresh: bool) -> Result<Arc<CodeIndex>, Response> {
    if !refresh && let Some(ix) = cache().lock().unwrap().get(&dir) {
        return Ok(ix.clone());
    }
    let d = dir.clone();
    let ix = tokio::task::spawn_blocking(move || Arc::new(CodeIndex::build(&d)))
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    cache().lock().unwrap().insert(dir, ix.clone());
    Ok(ix)
}

async fn load(directory: Option<&str>, refresh: bool) -> Result<Arc<CodeIndex>, Response> {
    let root = project_root();
    let dir = resolve_directory(&root, directory)?;
    index_for(dir, refresh).await
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct DirQuery {
    pub directory: Option<String>,
    #[serde(default)]
    pub refresh: Option<bool>,
}

/// `GET /code-index/status` → `{ok, status, files, symbols, root}`.
pub async fn index_status(Query(q): Query<DirQuery>) -> Result<Json<Value>, Response> {
    let ix = load(q.directory.as_deref(), q.refresh.unwrap_or(false)).await?;
    Ok(Json(json!({
        "ok": true,
        "status": "ready",
        "files": ix.files.len(),
        "symbols": ix.symbol_count(),
        "root": ix.root,
    })))
}

#[derive(Debug, Default, Deserialize)]
pub struct SearchQuery {
    pub directory: Option<String>,
    /// Query text (`q` or `query`).
    pub q: Option<String>,
    pub query: Option<String>,
    pub limit: Option<usize>,
    pub extension: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SearchBody {
    pub query: Option<String>,
    pub q: Option<String>,
    pub limit: Option<usize>,
    pub extension: Option<String>,
}

async fn do_search(
    directory: Option<&str>,
    query: Option<String>,
    limit: Option<usize>,
    ext: Option<String>,
) -> Result<Json<Value>, Response> {
    let Some(query) = query.filter(|q| !q.trim().is_empty()) else {
        return Err(error(StatusCode::BAD_REQUEST, "missing query (`q` or `query`)"));
    };
    let ix = load(directory, false).await?;
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let results = ix.search(&query, limit, ext.as_deref());
    Ok(Json(json!({ "ok": true, "results": results })))
}

/// `GET /code-index/search?q=&limit=&extension=&directory=`.
pub async fn index_search_get(Query(q): Query<SearchQuery>) -> Result<Json<Value>, Response> {
    do_search(q.directory.as_deref(), q.q.or(q.query), q.limit, q.extension).await
}

/// `POST /code-index/search?directory=` with `{"query", "limit", "extension"}`.
pub async fn index_search_post(
    Query(q): Query<DirQuery>,
    body: Option<Json<SearchBody>>,
) -> Result<Json<Value>, Response> {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    do_search(q.directory.as_deref(), b.query.or(b.q), b.limit, b.extension).await
}

/// `GET /code-ast/status` → languages and symbol counts.
pub async fn ast_status(Query(q): Query<DirQuery>) -> Result<Json<Value>, Response> {
    let ix = load(q.directory.as_deref(), false).await?;
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    let mut langs: BTreeMap<&str, usize> = BTreeMap::new();
    for f in &ix.files {
        if !f.symbols.is_empty() {
            *langs.entry(f.ext.as_str()).or_default() += 1;
        }
        for s in &f.symbols {
            *kinds.entry(s.kind).or_default() += 1;
        }
    }
    Ok(Json(json!({
        "ok": true,
        "status": "ready",
        "enabled": true,
        "engine": "heuristic",
        "languages": langs,
        "kinds": kinds,
        "symbols": ix.symbol_count(),
    })))
}

#[derive(Debug, Default, Deserialize)]
pub struct AstQuery {
    pub directory: Option<String>,
    pub kind: Option<String>,
    /// Case-insensitive substring of the symbol name.
    pub name: Option<String>,
    pub path: Option<String>,
    pub limit: Option<usize>,
}

/// `GET /code-ast?kind=fn&name=handle&path=routes&limit=` — structural symbol query.
pub async fn ast_search(Query(q): Query<AstQuery>) -> Result<Json<Value>, Response> {
    let ix = load(q.directory.as_deref(), false).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let name = q.name.map(|n| n.to_lowercase());
    let mut results = Vec::new();
    'outer: for f in &ix.files {
        if q.path.as_deref().is_some_and(|p| !f.path.contains(p)) {
            continue;
        }
        for s in &f.symbols {
            if q.kind.as_deref().is_some_and(|k| k != s.kind) {
                continue;
            }
            if name.as_deref().is_some_and(|n| !s.name.to_lowercase().contains(n)) {
                continue;
            }
            results.push(json!({ "path": f.path, "kind": s.kind, "name": s.name, "line": s.line }));
            if results.len() >= limit {
                break 'outer;
            }
        }
    }
    Ok(Json(json!({ "ok": true, "results": results })))
}

#[derive(Debug, Default, Deserialize)]
pub struct GraphQuery {
    pub directory: Option<String>,
    /// Only edges touching this file (relative path).
    pub file: Option<String>,
}

/// `GET /code-graph?file=` → `{ok, nodes, edges: [{from, to}], depends, dependents}`.
pub async fn graph(Query(q): Query<GraphQuery>) -> Result<Json<Value>, Response> {
    let ix = load(q.directory.as_deref(), false).await?;
    let edges = ix.edges();
    if let Some(file) = q.file.as_deref() {
        let depends: Vec<&str> = edges.iter().filter(|(a, _)| a == file).map(|(_, b)| b.as_str()).collect();
        let dependents: Vec<&str> = edges.iter().filter(|(_, b)| b == file).map(|(a, _)| a.as_str()).collect();
        return Ok(Json(json!({ "ok": true, "file": file, "depends": depends, "dependents": dependents })));
    }
    let nodes =
        ix.files.iter().filter(|f| matches!(f.ext.as_str(), "rs" | "ts" | "tsx" | "js" | "mjs" | "jsx")).count();
    let edges: Vec<Value> = edges.iter().map(|(a, b)| json!({ "from": a, "to": b })).collect();
    Ok(Json(json!({ "ok": true, "status": "ready", "nodes": nodes, "edges": edges })))
}

#[derive(Debug, Default, Deserialize)]
pub struct MapQuery {
    pub directory: Option<String>,
    /// Only files under this relative prefix.
    pub path: Option<String>,
}

/// `GET /code-map?path=` → per-file symbol outline.
pub async fn map(Query(q): Query<MapQuery>) -> Result<Json<Value>, Response> {
    let ix = load(q.directory.as_deref(), false).await?;
    let files: Vec<Value> = ix
        .files
        .iter()
        .filter(|f| !f.symbols.is_empty())
        .filter(|f| q.path.as_deref().is_none_or(|p| f.path.starts_with(p)))
        .map(|f| json!({ "path": f.path, "lines": f.lines, "symbols": f.symbols }))
        .collect();
    Ok(Json(json!({ "ok": true, "status": "ready", "files": files })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let w = |p: &str, c: &str| {
            let p = d.path().join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, c).unwrap();
        };
        w("crate/src/lib.rs", "pub mod client;\nmod util;\npub fn run() {}\n");
        w(
            "crate/src/client.rs",
            "use crate::util::helper;\npub struct Client;\nimpl Client {\n    pub async fn connect(&self) {}\n}\n",
        );
        w("crate/src/util.rs", "pub(crate) fn helper() {}\n");
        w("web/app.ts", "import { Foo } from './foo';\nexport class AppComponent {}\n");
        w("web/foo.ts", "export interface Foo { x: number }\nexport const BAR = 1;\n");
        w("go/client.go", "package x\nfunc (c *Client) Connect() error { return nil }\ntype Client struct{}\n");
        w("node_modules/junk.ts", "export class AppComponent {}\n");
        w("target/x.rs", "fn connect() {}\n");
        d
    }

    #[test]
    fn builds_and_skips_vendor_dirs() {
        let d = fixture();
        let ix = CodeIndex::build(d.path());
        let paths: Vec<&str> = ix.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths.len(), 6, "{paths:?}");
        assert!(!paths.iter().any(|p| p.starts_with("node_modules") || p.starts_with("target")));
    }

    #[test]
    fn extracts_symbols() {
        let d = fixture();
        let ix = CodeIndex::build(d.path());
        let syms = |p: &str| ix.files.iter().find(|f| f.path == p).unwrap().symbols.clone();
        let c = syms("crate/src/client.rs");
        assert!(c.contains(&Symbol { kind: "struct", name: "Client".into(), line: 2 }));
        assert!(c.contains(&Symbol { kind: "impl", name: "Client".into(), line: 3 }));
        assert!(c.contains(&Symbol { kind: "fn", name: "connect".into(), line: 4 }));
        assert_eq!(syms("crate/src/util.rs")[0].name, "helper");
        assert_eq!(syms("go/client.go")[0], Symbol { kind: "fn", name: "Connect".into(), line: 2 });
        let f = syms("web/foo.ts");
        assert_eq!(f[0].kind, "interface");
        assert_eq!(f[1], Symbol { kind: "const", name: "BAR".into(), line: 2 });
    }

    #[test]
    fn search_ranks_and_filters() {
        let d = fixture();
        let ix = CodeIndex::build(d.path());
        let hits = ix.search("Client connect", 5, None);
        assert!(!hits.is_empty());
        assert!(hits[0].path.contains("client"), "{hits:?}");
        let rs = ix.search("connect", 5, Some(".rs"));
        assert!(rs.iter().all(|h| h.path.ends_with(".rs")));
        assert!(ix.search("zzz_nothing_here", 5, None).is_empty());
        assert!(ix.search("   ", 5, None).is_empty());
        // OR fallback: one term missing still finds something.
        assert!(!ix.search("helper zzzmissing", 5, None).is_empty());
        // Path fragments are searchable.
        assert_eq!(ix.search("foo.ts", 1, None)[0].path, "web/foo.ts");
        assert_eq!(ix.search("connect", 1, None).len(), 1);
    }

    #[test]
    fn graph_edges() {
        let d = fixture();
        let ix = CodeIndex::build(d.path());
        let e = ix.edges();
        let has = |a: &str, b: &str| e.iter().any(|(x, y)| x == a && y == b);
        assert!(has("crate/src/lib.rs", "crate/src/client.rs"), "{e:?}");
        assert!(has("crate/src/lib.rs", "crate/src/util.rs"));
        assert!(has("crate/src/client.rs", "crate/src/util.rs"));
        assert!(has("web/app.ts", "web/foo.ts"));
    }

    #[test]
    fn directory_guard() {
        let d = fixture();
        let root = d.path().canonicalize().unwrap();
        assert_eq!(resolve_directory(&root, None).unwrap(), root);
        assert_eq!(resolve_directory(&root, Some("web")).unwrap(), root.join("web"));
        assert_eq!(resolve_directory(&root, Some("..")).unwrap_err().status(), StatusCode::FORBIDDEN);
        assert_eq!(resolve_directory(&root, Some("/")).unwrap_err().status(), StatusCode::FORBIDDEN);
        assert_eq!(resolve_directory(&root, Some("nope")).unwrap_err().status(), StatusCode::NOT_FOUND);
        assert_eq!(resolve_directory(&root, Some("web/foo.ts")).unwrap_err().status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn normalize_paths() {
        assert_eq!(normalize("a/b/../c/./d"), "a/c/d");
        assert_eq!(join("", "x"), "x");
    }
}
