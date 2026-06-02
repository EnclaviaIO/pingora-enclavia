use std::sync::Arc;

use pingora_enclavia::config::Config;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg_path = std::env::args().nth(1).unwrap_or_else(|| "config.toml".into());
    let cfg = Config::load(&cfg_path)?;
    let _cfg = Arc::new(cfg);

    eprintln!(
        "pingora-enclavia scaffold built; pingora app wiring lives in checkpoint 2"
    );
    Ok(())
}
