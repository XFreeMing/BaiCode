//! Carries request-local W3C snapshots through RMCP's background transport queues.
//! Metadata does not retain caller spans or change the SDK's cursor-based cache keys.

use std::future::Future;

use codex_protocol::protocol::W3cTraceContext;
use tracing::Span;

tokio::task_local! {
    static CURRENT_REQUEST_TRACE: McpRequestTrace;
}

/// A request-scoped W3C fallback for MCP work when no tracing layer is installed.
#[derive(Clone)]
pub struct McpRequestTrace {
    trace: Option<W3cTraceContext>,
    parent: Span,
}

impl McpRequestTrace {
    /// Retains only a valid inbound carrier; active spans take precedence over this fallback.
    pub fn from_w3c(trace: Option<W3cTraceContext>) -> Self {
        Self {
            trace: trace
                .as_ref()
                .and_then(codex_otel::context_from_w3c_trace_context)
                .and_then(|context| codex_otel::w3c_trace_context_from_context(&context)),
            parent: Span::current(),
        }
    }

    /// Captures the fallback for an explicitly spawned child task.
    pub fn current() -> Self {
        Self {
            trace: CURRENT_REQUEST_TRACE
                .try_with(|current| current.trace.clone())
                .unwrap_or_default(),
            parent: Span::current(),
        }
    }

    #[tracing::instrument(name = "mcp.request", parent = &self.parent, skip_all)]
    pub async fn scope<F: Future>(self, future: F) -> F::Output {
        CURRENT_REQUEST_TRACE.scope(self, future).await
    }
}
