mod config;

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use argus_core::Health;
use argus_notifier_feishu::FeishuNotifier;
use argus_plugin_getllm::{GetLlmMonitor, ImageProbeConfig, TextProbeConfig};
use argus_runtime::{AlertPolicy, Runner};
use clap::Parser;
use config::Settings;
use tokio::sync::watch;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(about = "Plugin-based API availability monitor", version)]
struct Cli {
    /// Run every configured probe once, then exit without sending alerts.
    #[arg(long)]
    once: bool,

    /// Validate environment configuration, then exit.
    #[arg(long)]
    check_config: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    init_tracing();
    let cli = Cli::parse();
    let settings = Settings::from_env().context("invalid configuration")?;
    let runner = build_runner(settings, !cli.once || cli.check_config)?;

    if cli.check_config {
        info!(monitors = runner.monitor_count(), "configuration valid");
        return Ok(());
    }

    if cli.once {
        let results = runner.run_once().await;
        if results
            .iter()
            .any(|(_, outcome)| outcome.health == Health::Unhealthy)
        {
            bail!("one or more probes failed");
        }
        return Ok(());
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let runner_task = tokio::spawn(runner.run_until(shutdown_rx));
    shutdown_signal().await?;
    info!("shutdown requested");
    let _ = shutdown_tx.send(true);
    runner_task.await.context("monitor runtime task failed")?;
    Ok(())
}

fn build_runner(settings: Settings, require_notifier: bool) -> Result<Runner> {
    let text_interval_secs = settings.text_interval.as_secs();
    let image_interval_secs = settings.image_interval.as_secs();
    let mut runner = Runner::new(AlertPolicy {
        failure_threshold: settings.failure_threshold,
        recovery_threshold: settings.recovery_threshold,
    });
    match (
        settings.feishu_app_id,
        settings.feishu_app_secret,
        settings.feishu_chat_id,
    ) {
        (Some(app_id), Some(app_secret), Some(chat_id)) => {
            let notifier =
                FeishuNotifier::new(app_id, app_secret, chat_id, settings.feishu_timeout)
                    .context("could not configure Feishu notifier")?;
            runner.add_notifier(Arc::new(notifier));
        }
        (None, None, None) if !require_notifier => {}
        (None, None, None) => bail!(
            "FEISHU_APP_ID, FEISHU_APP_SECRET and FEISHU_CHAT_ID are required for continuous monitoring"
        ),
        _ => {
            bail!("FEISHU_APP_ID, FEISHU_APP_SECRET and FEISHU_CHAT_ID must be configured together")
        }
    }

    if settings.text_enabled {
        let monitor = GetLlmMonitor::text(
            &settings.getllm_base_url,
            settings.getllm_api_key.clone(),
            settings.getllm_route.clone(),
            settings.text_interval,
            settings.request_timeout,
            TextProbeConfig {
                model: settings.text_model,
                prompt: settings.text_prompt,
                max_tokens: settings.text_max_tokens,
            },
        )
        .context("could not configure GetLLM text probe")?;
        runner.add_monitor(Arc::new(monitor));
    }

    if settings.image_enabled {
        let monitor = GetLlmMonitor::image(
            &settings.getllm_base_url,
            settings.getllm_api_key,
            settings.getllm_route,
            settings.image_interval,
            settings.image_timeout,
            ImageProbeConfig {
                model: settings.image_model,
                prompt: settings.image_prompt,
                size: settings.image_size,
                quality: settings.image_quality,
                response_format: settings.image_response_format,
            },
        )
        .context("could not configure GetLLM image probe")?;
        runner.add_monitor(Arc::new(monitor));
    }

    info!(
        monitors = runner.monitor_count(),
        text_interval_secs, image_interval_secs, "Argus configured"
    );
    Ok(runner)
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("argus=info,argus_server=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate =
            signal(SignalKind::terminate()).context("could not install SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("could not install Ctrl-C handler")?,
            _ = terminate.recv() => {},
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("could not install Ctrl-C handler")
    }
}
