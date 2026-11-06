use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Every panic (including tokio worker threads) must land in the diagnostic
    // log so the frontend sees it via GET /debug/log — not only in the
    // terminal. Install before anything else so early panics are also captured.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info.to_string();
        let loc = info.location().map(|l| format!(" at {l}")).unwrap_or_default();
        let full = format!("panic{loc}: {msg}");
        eprintln!("{full}");
        bebok_server::debug_log::global_note(format!("[panic] {full}"));
        default_hook(info);
    }));

    // rustls 0.23 requires a process-wide CryptoProvider. When both `ring`
    // and `aws-lc-rs` are enabled (via feature unification) auto-detection
    // panics with "Could not automatically determine...". Install explicitly
    // before any TLS code runs.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let config = bebok_server::Config::from_args(std::env::args().skip(1))?;
    if let Err(err) = bebok_server::run(config).await {
        let msg = format!("{err:#}");
        eprintln!("engine error: {msg}");
        bebok_server::debug_log::global_note(format!("[fatal] {msg}"));
        return Err(err);
    }
    Ok(())
}
