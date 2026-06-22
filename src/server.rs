//! The downstream-facing MCP server.
//!
//! Exposes exactly one tool, `execute_python`, whose description carries the
//! generated SDK signatures. Calls are forwarded to the configured [`Executor`],
//! which runs the user's Python (with the SDK preloaded) and returns its result
//! plus captured output.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::{json, Map, Value};

use crate::exec::Executor;

const TOOL_NAME: &str = "execute_python";

/// The downstream MCP server. Cheap to clone (everything behind `Arc`).
#[derive(Clone)]
pub struct CodeServer {
    inner: Arc<Inner>,
}

struct Inner {
    executor: Box<dyn Executor>,
    description: String,
    input_schema: Arc<Map<String, Value>>,
}

impl CodeServer {
    pub fn new(executor: Box<dyn Executor>, description: String) -> Self {
        let schema: Map<String, Value> = json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Python source to execute. SDK functions are preloaded; \
                                    assign to `result` (or leave a final expression) to return a value."
                }
            },
            "required": ["code"],
            "additionalProperties": false
        })
        .as_object()
        .cloned()
        .expect("schema is an object");

        Self {
            inner: Arc::new(Inner {
                executor,
                description,
                input_schema: Arc::new(schema),
            }),
        }
    }

    fn tool(&self) -> Tool {
        Tool::new(
            Cow::Borrowed(TOOL_NAME),
            Cow::Owned(self.inner.description.clone()),
            self.inner.input_schema.clone(),
        )
    }
}

impl ServerHandler for CodeServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "codemcp gateway: write Python that calls connected MCP tools as typed \
                 functions and returns a combined result in one step. See the \
                 `execute_python` tool description for the available SDK.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: vec![self.tool()],
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if request.name != TOOL_NAME {
            return Err(ErrorData::invalid_params(
                format!("unknown tool: {}", request.name),
                None,
            ));
        }

        let code = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("code"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ErrorData::invalid_params("missing required string argument `code`", None)
            })?
            .to_string();

        let out = self.inner.executor.run(code).await?;

        // User code raised: surface as a tool error (structured), not a protocol
        // error — the agent can read the traceback and retry.
        if let Some(err) = out.error {
            return Ok(CallToolResult::structured_error(json!({
                "error": err,
                "stdout": out.stdout,
                "stderr": out.stderr,
            })));
        }

        Ok(CallToolResult::structured(json!({
            "result": out.result,
            "stdout": out.stdout,
            "stderr": out.stderr,
        })))
    }
}

// Allow constructing `Content` for callers that prefer text; kept for clarity.
#[allow(dead_code)]
fn text(s: impl Into<String>) -> Content {
    Content::text(s)
}
