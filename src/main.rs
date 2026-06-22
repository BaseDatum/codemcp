//! codemcp — meta-MCP code-mode gateway.
//!
//! Connects to many upstream MCP servers and exposes a single `execute_python`
//! tool. Agents write Python that calls all upstream tools as typed functions.

mod config;
mod control;
mod env;
mod error;
mod exec;
mod prompt;
mod sdk;
mod server;
mod upstream;

use std::sync::Arc;

use anyhow::Result;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use crate::env::{ServerTransport, Settings};
use crate::exec::host::HostExecutor;
use crate::exec::Executor;
use crate::sdk::SdkRegistry;
use crate::server::CodeServer;
use crate::upstream::UpstreamManager;

#[tokio::main]
async fn main() -> Result<()> {
    let settings = Settings::from_env()?;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&settings.log))
        .with_writer(std::io::stderr)
        .init();

    tracing::info!(
        isolation = ?settings.isolation,
        transport = ?settings.transport,
        config = %settings.config.display(),
        "codemcp starting"
    );

    let configs = config::load(&settings.config)?;
    tracing::info!(count = configs.len(), "loaded upstream server configs");

    let manager = UpstreamManager::connect_all(&configs).await;
    let tools = manager.all_tools();
    tracing::info!(total_tools = tools.len(), "discovered upstream tools");

    let registry = SdkRegistry::build(&tools);
    tracing::info!(bindings = registry.bindings.len(), "generated SDK bindings");

    // Debug dump when requested.
    if std::env::var("CODEMCP_DUMP").is_ok() {
        eprintln!("===== sdk.py =====\n{}", registry.generate_sdk_py());
        eprintln!(
            "===== execute_python description =====\n{}",
            prompt::build_description(&registry, settings.isolation)
        );
    }

    let sdk_py = registry.generate_sdk_py();
    let upstreams = Arc::new(manager);

    // Smoke-test path: start the host worker, run a snippet, print, exit.
    if let Ok(code) = std::env::var("CODEMCP_SMOKE") {
        let executor = HostExecutor::start(&settings, &sdk_py, upstreams.clone()).await?;
        let out = executor.run(code).await?;
        eprintln!("=== result ===\n{}", serde_json::to_string_pretty(&out.result)?);
        eprintln!("=== stdout ===\n{}", out.stdout);
        eprintln!("=== stderr ===\n{}", out.stderr);
        if let Some(err) = &out.error {
            eprintln!("=== error ===\n{err}");
        }
        executor.shutdown().await;
        if let Ok(m) = Arc::try_unwrap(upstreams) {
            m.shutdown().await;
        }
        return Ok(());
    }

    // Start the Python worker, build the SDK description, and serve.
    let executor = HostExecutor::start(&settings, &sdk_py, upstreams.clone()).await?;
    let description = prompt::build_description(&registry, settings.isolation);
    let code_server = CodeServer::new(Box::new(executor), description);

    match settings.transport {
        ServerTransport::Stdio => {
            tracing::info!("serving execute_python over stdio");
            let running = code_server
                .serve(stdio())
                .await
                .map_err(|e| anyhow::anyhow!("failed to start stdio server: {e}"))?;
            running
                .waiting()
                .await
                .map_err(|e| anyhow::anyhow!("server task error: {e}"))?;
        }
        ServerTransport::Http => {
            let config = StreamableHttpServerConfig::default()
                .with_stateful_mode(!settings.http_json_response)
                .with_json_response(settings.http_json_response);
            // Each session shares the single Python worker via the cloned server.
            let factory_server = code_server.clone();
            let service = StreamableHttpService::new(
                move || Ok(factory_server.clone()),
                Arc::new(LocalSessionManager::default()),
                config,
            );

            let app = axum::Router::new().nest_service(&settings.http_path, service);
            let listener = tokio::net::TcpListener::bind(settings.http_bind).await?;
            tracing::info!(
                bind = %settings.http_bind,
                path = %settings.http_path,
                "serving execute_python over Streamable HTTP"
            );
            axum::serve(listener, app)
                .await
                .map_err(|e| anyhow::anyhow!("http server error: {e}"))?;
        }
    }

    if let Ok(m) = Arc::try_unwrap(upstreams) {
        m.shutdown().await;
    }
    Ok(())
}
