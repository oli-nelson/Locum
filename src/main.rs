use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use locum::config::Config;
use locum::harness::{HarnessKind, Registry, Turn};
use locum::server::AppState;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(
    name = "locum",
    version,
    about = "OpenAI-compatible API served by your local agent CLIs"
)]
struct Cli {
    /// Path to config.toml (default: ~/.config/locum/config.toml)
    #[arg(long, global = true, env = "LOCUM_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP server (default)
    Serve {
        /// Address to bind, e.g. 127.0.0.1:11435
        #[arg(long)]
        bind: Option<SocketAddr>,
    },
    /// Probe each harness: path, version, auth, optional live canary
    Doctor {
        /// Run a one-token canary request through each harness
        #[arg(long)]
        live: bool,
    },
    /// List models the server would expose
    Models,
    /// Configuration helpers
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the effective configuration as TOML
    Print,
    /// Print the default config file location
    Path,
}

fn init_tracing(cfg: &Config) {
    let filter = EnvFilter::try_from_env("LOCUM_LOG")
        .or_else(|_| EnvFilter::try_new(&cfg.log.level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    if cfg.log.json {
        builder.json().init();
    } else {
        builder.init();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = Config::load(cli.config.as_deref())?;

    match cli.cmd.unwrap_or(Cmd::Serve { bind: None }) {
        Cmd::Serve { bind } => {
            if let Some(b) = bind {
                cfg.server.bind = b;
            }
            cfg.validate()?;
            init_tracing(&cfg);
            serve(cfg).await
        }
        Cmd::Doctor { live } => {
            init_tracing(&cfg);
            doctor(cfg, live).await
        }
        Cmd::Models => {
            init_tracing(&cfg);
            let cfg = Arc::new(cfg);
            let registry = Arc::new(Registry::from_config(&cfg));
            let state = AppState::new(cfg, registry.clone());
            for e in state.catalog.entries().await {
                println!("{:<60} {}", e.id, e.owned_by);
            }
            registry.shutdown().await;
            Ok(())
        }
        Cmd::Config {
            cmd: ConfigCmd::Print,
        } => {
            println!("{}", cfg.to_toml());
            Ok(())
        }
        Cmd::Config {
            cmd: ConfigCmd::Path,
        } => {
            println!(
                "{}",
                locum::config::default_config_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<unknown>".into())
            );
            Ok(())
        }
    }
}

async fn serve(cfg: Config) -> Result<()> {
    let cfg = Arc::new(cfg);
    let registry = Arc::new(Registry::from_config(&cfg));
    let state = AppState::new(cfg.clone(), registry.clone());
    let app = locum::server::router(state.clone());

    // Optional prewarm runs in the background so the listener is up immediately.
    {
        let registry = registry.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            if cfg.harness.codex.pool.prewarm || cfg.harness.opencode.pool.prewarm {
                for (_, h) in registry.iter() {
                    h.prewarm().await;
                }
            }
        });
    }

    let shutdown = async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => {},
                _ = term.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
        tracing::info!("shutdown requested");
    };

    if let Some(path) = &cfg.server.unix {
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(path);
            let listener = tokio::net::UnixListener::bind(path)
                .with_context(|| format!("binding unix socket {}", path.display()))?;
            tracing::info!(socket = %path.display(), "locum listening");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await?;
        }
        #[cfg(not(unix))]
        anyhow::bail!("unix sockets are not supported on this platform");
    } else {
        let listener = tokio::net::TcpListener::bind(cfg.server.bind)
            .await
            .with_context(|| format!("binding {}", cfg.server.bind))?;
        tracing::info!(addr = %cfg.server.bind, harnesses = ?registry.kinds(), "locum listening");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await?;
    }

    // Grace period for in-flight turns, then reap every child.
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while state.inflight.available_permits() < cfg.limits.max_inflight {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    if let Some(sc) = &state.session_cache {
        sc.shutdown().await;
    }
    registry.shutdown().await;
    Ok(())
}

async fn doctor(cfg: Config, live: bool) -> Result<()> {
    let cfg = Arc::new(cfg);
    let registry = Arc::new(Registry::from_config(&cfg));
    let mut any_ok = false;
    for kind in HarnessKind::ALL {
        let Some(h) = registry.get(kind) else {
            println!("{kind:<9} disabled");
            continue;
        };
        print!("{kind:<9} ");
        match tokio::time::timeout(Duration::from_secs(60), h.probe()).await {
            Ok(Ok(info)) => {
                println!(
                    "ok      {} {} auth={} {}",
                    info.executable
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    info.version.unwrap_or_default(),
                    info.authenticated
                        .map(|b| b.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                    info.detail.unwrap_or_default()
                );
                any_ok = true;
            }
            Ok(Err(e)) => println!("FAIL    {}", e.message),
            Err(_) => println!("FAIL    probe timed out"),
        }
        if live {
            let model = cfg.harness_default_model(kind).to_string();
            let turn = Turn {
                request_id: "doctor".into(),
                model: model.clone(),
                effort: Some("low".into()),
                system: Some(locum::prompt::DEFAULT_PREAMBLE.into()),
                input: "Reply with the single word: pong".into(),
                images: vec![],
                response_schema: None,
                want_reasoning: false,
                stream_hint: true,
                scratch: Arc::new(tempfile::tempdir()?),
                resume: None,
            };
            let started = std::time::Instant::now();
            match tokio::time::timeout(
                Duration::from_secs(120),
                h.run(turn, CancellationToken::new()),
            )
            .await
            {
                Ok(Ok(mut stream)) => {
                    use futures::StreamExt;
                    let mut text = String::new();
                    let mut outcome = String::from("no terminal event");
                    while let Some(ev) = stream.next().await {
                        match ev {
                            locum::harness::Event::TextDelta(d) => text.push_str(&d),
                            locum::harness::Event::Done(r) => {
                                outcome = format!("{r:?}");
                                break;
                            }
                            locum::harness::Event::Error(e) => {
                                outcome = format!("error: {}", e.message);
                                break;
                            }
                            _ => {}
                        }
                    }
                    println!(
                        "          canary ({model}): {outcome} in {} ms → {:?}",
                        started.elapsed().as_millis(),
                        text.trim().chars().take(80).collect::<String>()
                    );
                }
                Ok(Err(e)) => println!("          canary ({model}): {}", e.message),
                Err(_) => println!("          canary ({model}): timed out"),
            }
        }
    }
    registry.shutdown().await;
    if !any_ok {
        anyhow::bail!("no harness is usable");
    }
    Ok(())
}
