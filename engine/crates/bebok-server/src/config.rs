//! CLI flags + environment (`README.md` → "Engine environment variables").
//! CLI flags win over environment variables.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub addr: SocketAddr,
    /// `None` = auth disabled (`BEBOK_NO_AUTH=1`).
    pub token: Option<String>,
    pub cors: Vec<String>,
    pub diagnostic: bool,
    pub data_dir: PathBuf,
    /// WhatsApp WebSocket endpoint. `None` = the default (`socket::frame_socket::URL`).
    /// Tests and mirrors point this at a local socket.
    pub wa_url: Option<String>,
    /// How long the server may take to answer the handshake (`NoiseHandshakeResponseTimeout`).
    pub handshake_timeout: std::time::Duration,
}

pub const DEFAULT_ADDR: &str = "127.0.0.1:8787";
/// `Go handshake.go:NoiseHandshakeResponseTimeout`.
pub const DEFAULT_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
pub const DEFAULT_CORS: &[&str] =
    &["http://localhost:4200", "http://127.0.0.1:4200", "tauri://localhost", "http://tauri.localhost"];

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn truthy(v: Option<String>) -> bool {
    matches!(v.as_deref(), Some("1" | "true" | "yes" | "on"))
}

pub fn random_token() -> String {
    hex::encode(rand::random::<[u8; 24]>())
}

impl Config {
    pub fn from_args(args: impl IntoIterator<Item = String>) -> anyhow::Result<Self> {
        Self::parse(args, env)
    }

    pub fn parse(args: impl IntoIterator<Item = String>, env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let mut addr: SocketAddr = env("BEBOK_ADDR").as_deref().unwrap_or(DEFAULT_ADDR).parse()?;
        if let Some(p) = env("BEBOK_PORT") {
            addr.set_port(p.parse().map_err(|_| anyhow::anyhow!("invalid BEBOK_PORT `{p}`"))?);
        }
        let mut token = env("BEBOK_TOKEN");
        let mut no_auth = truthy(env("BEBOK_NO_AUTH"));
        let mut diagnostic = truthy(env("BEBOK_DIAGNOSTIC"));
        let mut data_dir = env("BEBOK_DATA_DIR").map(PathBuf::from);
        let mut wa_url = env("BEBOK_WA_URL");
        let mut handshake_timeout = match env("BEBOK_HANDSHAKE_TIMEOUT_SECS") {
            Some(secs) => Some(
                secs.parse::<u64>().map_err(|_| anyhow::anyhow!("invalid BEBOK_HANDSHAKE_TIMEOUT_SECS `{secs}`"))?,
            ),
            None => None,
        };

        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            let mut val = |name: &str| it.next().ok_or_else(|| anyhow::anyhow!("{name} needs a value"));
            match a.as_str() {
                "--host" => addr.set_ip(val("--host")?.parse::<IpAddr>()?),
                "--port" => addr.set_port(val("--port")?.parse()?),
                "--addr" => addr = val("--addr")?.parse()?,
                "--token" => token = Some(val("--token")?),
                "--data-dir" => data_dir = Some(PathBuf::from(val("--data-dir")?)),
                "--no-auth" => no_auth = true,
                "--diagnostic" => diagnostic = true,
                "--wa-url" => wa_url = Some(val("--wa-url")?),
                "--handshake-timeout" => handshake_timeout = Some(val("--handshake-timeout")?.parse()?),
                other => anyhow::bail!("unknown argument `{other}`"),
            }
        }

        let mut cors: Vec<String> = DEFAULT_CORS.iter().map(|s| s.to_string()).collect();
        if let Some(extra) = env("BEBOK_CORS") {
            cors.extend(extra.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }

        let data_dir =
            data_dir.unwrap_or_else(|| dirs::data_dir().unwrap_or_else(|| PathBuf::from(".bebok")).join("bebok"));

        Ok(Self {
            addr,
            token: if no_auth { None } else { Some(token.unwrap_or_else(random_token)) },
            cors,
            diagnostic,
            data_dir,
            wa_url,
            handshake_timeout: handshake_timeout
                .map(std::time::Duration::from_secs)
                .unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT),
        })
    }

    /// Line printed on stdout once the listener is bound.
    pub fn ready_line(&self, bound: SocketAddr) -> String {
        match &self.token {
            Some(t) => format!("BEBOK_READY http://{bound}/?token={t}"),
            None => format!("BEBOK_READY http://{bound}/"),
        }
    }

    /// `GET /config` body with every secret redacted (`ARCHITECTURE.md` §9):
    /// the capability token never leaves the process, and a custom WhatsApp
    /// URL loses its credentials and query string.
    pub fn redacted_json(&self, vector_search: bool) -> serde_json::Value {
        serde_json::json!({
            "addr": self.addr.to_string(),
            "auth": self.token.is_some(),
            "token": self.token.as_ref().map(|_| REDACTED),
            "cors": self.cors,
            "diagnostic": self.diagnostic,
            "dataDir": self.data_dir,
            "waUrl": self.wa_url.as_deref().map(redact_url),
            "handshakeTimeoutSecs": self.handshake_timeout.as_secs(),
            "vectorSearch": vector_search,
        })
    }
}

/// The marker that replaces a secret in `GET /config`.
pub const REDACTED: &str = "<redacted>";

/// `scheme://user:pass@host/path?query` → `scheme://<redacted>@host/path?<redacted>`.
pub fn redact_url(url: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, _)) => (base, Some(REDACTED)),
        None => (url, None),
    };
    let base = match base.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            match authority.rsplit_once('@') {
                Some((_, host)) => format!("{scheme}://{REDACTED}@{host}{path}"),
                None => base.to_owned(),
            }
        }
        None => base.to_owned(),
    };
    match query {
        Some(q) => format!("{base}?{q}"),
        None => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str], vars: &[(&str, &str)]) -> anyhow::Result<Config> {
        let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::parse(args.iter().map(|s| s.to_string()), move |k| {
            vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        })
    }

    #[test]
    fn defaults() {
        let c = parse(&[], &[]).unwrap();
        assert_eq!(c.addr.to_string(), DEFAULT_ADDR);
        assert_eq!(c.token.as_ref().unwrap().len(), 48);
        assert!(!c.diagnostic);
    }

    #[test]
    fn cli_wins_over_env() {
        let c =
            parse(&["--port", "0", "--token", "cli"], &[("BEBOK_ADDR", "0.0.0.0:1"), ("BEBOK_TOKEN", "env")]).unwrap();
        assert_eq!(c.addr.to_string(), "0.0.0.0:0");
        assert_eq!(c.token.as_deref(), Some("cli"));
    }

    #[test]
    fn env_flags() {
        let c = parse(&[], &[("BEBOK_NO_AUTH", "1"), ("BEBOK_PORT", "9000"), ("BEBOK_CORS", "http://a, http://b")])
            .unwrap();
        assert!(c.token.is_none());
        assert_eq!(c.addr.port(), 9000);
        assert!(c.cors.contains(&"http://b".to_string()));
        assert!(parse(&[], &[("BEBOK_PORT", "70000")]).is_err());
        assert!(parse(&["--bogus"], &[]).is_err());
    }

    #[test]
    fn redacted_config_hides_the_token_and_url_credentials() {
        let c = parse(&["--token", "s3cret", "--wa-url", "wss://u:pw@mirror.example/ws?key=k1"], &[]).unwrap();
        let v = c.redacted_json(false);
        let dump = v.to_string();
        assert!(!dump.contains("s3cret") && !dump.contains("pw") && !dump.contains("k1"), "{dump}");
        assert_eq!(v["token"], REDACTED);
        assert_eq!(v["auth"], true);
        assert_eq!(v["waUrl"], "wss://<redacted>@mirror.example/ws?<redacted>");
        assert_eq!(redact_url("ws://127.0.0.1:1"), "ws://127.0.0.1:1");
        assert!(parse(&["--no-auth"], &[]).unwrap().redacted_json(true)["token"].is_null());
    }

    #[test]
    fn ready_line_format() {
        let c = parse(&["--token", "abc"], &[]).unwrap();
        assert_eq!(c.ready_line("127.0.0.1:5".parse().unwrap()), "BEBOK_READY http://127.0.0.1:5/?token=abc");
    }
}
