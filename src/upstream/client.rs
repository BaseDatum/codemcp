//! Connect to a single upstream MCP server (stdio or streamable-http).

use std::collections::BTreeMap;
use std::time::Duration;

use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, IntoTransport,
    StreamableHttpClientTransport, TokioChildProcess,
};
use rmcp::ServiceExt;
use tokio::process::Command;

use crate::config::ServerSpec;
use crate::error::Error;

/// A live connection to one upstream server. The unit handler `()` is a valid
/// client that just doesn't react to server-initiated requests.
pub type UpstreamService = RunningService<RoleClient, ()>;

/// Default time to wait for an upstream to spawn and complete the MCP
/// handshake when the config does not specify a `timeout`.
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 30;

/// Connect to the upstream described by `spec`.
pub(crate) async fn connect(name: &str, spec: &ServerSpec) -> Result<UpstreamService, Error> {
    match spec {
        ServerSpec::Local {
            command,
            environment,
            cwd,
            timeout,
            ..
        } => connect_stdio(name, command, environment, cwd.as_deref(), *timeout).await,
        ServerSpec::Remote {
            url,
            headers,
            timeout,
            ..
        } => connect_http(name, url, headers, *timeout).await,
    }
}

/// Drive the MCP handshake to completion, failing if it does not finish within
/// the configured (or default) timeout.
async fn serve_with_timeout<T, E, A>(
    name: &str,
    transport: T,
    timeout: Option<u64>,
) -> Result<UpstreamService, Error>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let secs = timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS);
    let fut = ().serve(transport);
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(Ok(service)) => Ok(service),
        Ok(Err(e)) => Err(Error::Upstream(format!("{name}: initialize failed: {e}"))),
        Err(_) => Err(Error::Upstream(format!(
            "{name}: timed out after {secs}s waiting for the server to initialize"
        ))),
    }
}

async fn connect_stdio(
    name: &str,
    command: &[String],
    environment: &BTreeMap<String, String>,
    cwd: Option<&str>,
    timeout: Option<u64>,
) -> Result<UpstreamService, Error> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| Error::Config(format!("upstream {name}: empty command")))?;

    let mut cmd = Command::new(program);
    cmd.args(args);
    for (k, v) in environment {
        cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let transport = TokioChildProcess::new(cmd)
        .map_err(|e| Error::Upstream(format!("{name}: spawn failed: {e}")))?;

    serve_with_timeout(name, transport, timeout).await
}

async fn connect_http(
    name: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    timeout: Option<u64>,
) -> Result<UpstreamService, Error> {
    let transport = if headers.is_empty() {
        StreamableHttpClientTransport::from_uri(url.to_string())
    } else {
        // Apply arbitrary headers (Authorization, API keys, etc.) by baking them
        // into a custom reqwest client used for every request.
        let mut header_map = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            let name_hdr = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| Error::Config(format!("{name}: invalid header name {k:?}: {e}")))?;
            let val_hdr = reqwest::header::HeaderValue::from_str(v)
                .map_err(|e| Error::Config(format!("{name}: invalid header value for {k:?}: {e}")))?;
            header_map.insert(name_hdr, val_hdr);
        }
        let client = reqwest::Client::builder()
            .default_headers(header_map)
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| Error::Upstream(format!("{name}: http client build failed: {e}")))?;
        // `StreamableHttpClientTransportConfig` is `#[non_exhaustive]`; build via
        // Default and set the public `uri` field.
        let mut config = StreamableHttpClientTransportConfig::default();
        config.uri = url.to_string().into();
        StreamableHttpClientTransport::with_client(client, config)
    };

    serve_with_timeout(name, transport, timeout).await
}
