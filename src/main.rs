use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use teloxide::{Bot, dispatching::Dispatcher};
use tracing_subscriber::{EnvFilter, fmt};

mod app;
mod config;
mod daemon;
mod notifier;
mod opencode;
mod pairing;
mod paste;
mod render;
mod state;
mod telegram;

use app::AppState;
use config::Config;
use notifier::Notifier;
use paste::PasteClient;
use state::Db;

#[derive(Debug, Parser)]
#[command(name = "telagent", version, about = "Telegram bridge for opencode")]
struct Cli {
    /// Path to the telagent config file.
    #[arg(long, global = true, env = "TELAGENT_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the Telegram bridge daemon.
    Daemon,
    /// Create a one-time pairing code for a Telegram chat.
    Pair,
    /// Print bindings and known sessions.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    let config_path = cli.config.clone().unwrap_or_else(Config::default_path);
    let config = Config::load(cli.config.as_deref())?;

    match cli.command {
        Some(Command::Daemon) => run_daemon(config).await,
        Some(Command::Pair) => run_pair(config).await,
        Some(Command::Status) => run_status(config).await,
        None => {
            let mut command = <Cli as clap::CommandFactory>::command();
            command.print_help()?;
            println!();
            Ok(())
        }
    }
    .with_context(|| format!("config: {}", config_path.display()))
}

async fn run_daemon(config: Config) -> Result<()> {
    let db_path = config.state_path();
    let db = Db::open(&db_path)?;
    let instance_id = db
        .get_or_create_instance_id(config.instance_id.as_deref())
        .await?;

    tracing::info!(
        instance_id,
        opencode = %config.opencode.url,
        db = %db_path.display(),
        "starting telagent"
    );

    let backend = opencode::build_backend(&config.opencode)?;
    tracing::info!(backend = backend.name(), "opencode backend ready");
    let token = config.bot_token()?;
    let bot = build_bot(token, config.telegram.proxy.as_deref())?;
    let paste = PasteClient::new(&config.notify.paste)?;
    let notifier = Notifier::new(
        bot.clone(),
        config.clone(),
        db.clone(),
        paste,
        instance_id.clone(),
    );
    let config = Arc::new(config);
    let state = Arc::new(AppState::new(config, instance_id, db, backend, notifier));

    let handler = dptree::entry().endpoint(telegram::bot::handle_update);
    let mut dispatcher = Dispatcher::builder(bot.clone(), handler)
        .dependencies(dptree::deps![state.clone()])
        .build();
    let dispatcher_task = tokio::spawn(async move {
        dispatcher.dispatch().await;
        tracing::error!("telegram dispatcher stopped");
    });

    let outbox_state = (*state).clone();
    let outbox_task = tokio::spawn(daemon::outbox_loop(outbox_state));

    let event_state = (*state).clone();
    let event_task = tokio::spawn(daemon::event_loop(event_state));

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");

    dispatcher_task.abort();
    outbox_task.abort();
    event_task.abort();
    Ok(())
}

async fn run_pair(config: Config) -> Result<()> {
    let db = Db::open(&config.state_path())?;
    let instance_id = db
        .get_or_create_instance_id(config.instance_id.as_deref())
        .await?;
    let code =
        pairing::create_pairing_code(&db, &instance_id, config.security.pairing_ttl_secs).await?;

    println!("Pairing code: {code}");
    println!(
        "Send /bind {code} to your Telegram bot within {} seconds.",
        config.security.pairing_ttl_secs
    );
    println!("Instance: {instance_id}");
    Ok(())
}

async fn run_status(config: Config) -> Result<()> {
    let db = Db::open(&config.state_path())?;
    let instance_id = db
        .get_or_create_instance_id(config.instance_id.as_deref())
        .await?;
    let chats = db.bound_chats(&instance_id).await?;
    let sessions = db.list_sessions(&instance_id, 20).await?;

    println!("Instance: {instance_id}");
    println!("Database: {}", config.state_path().display());
    println!("Bound chats: {chats:?}");
    if sessions.is_empty() {
        println!("Sessions: none");
    } else {
        println!("Sessions:");
        for session in sessions {
            println!(
                "  {}  [{}]  {}  {}",
                session.session_id, session.state, session.title, session.directory
            );
        }
    }
    Ok(())
}

fn build_bot(token: String, proxy: Option<&str>) -> Result<Bot> {
    let mut builder = teloxide::net::default_reqwest_settings();
    if let Some(proxy) = proxy.filter(|proxy| !proxy.trim().is_empty()) {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy)
                .with_context(|| format!("invalid Telegram proxy URL: {proxy}"))?,
        );
        tracing::info!("using Telegram proxy");
    }
    let client = builder
        .build()
        .context("failed to create Telegram HTTP client")?;
    Ok(Bot::with_client(token, client))
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("telagent=info"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}
