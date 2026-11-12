//! Calls that run to the end when their client goes away.
//!
//! hyper drops a request's handler future when its connection closes, which would
//! cancel a generation midway: nothing stored, nothing cached and no request log
//! entry, so the next identical call generates again from scratch. [`detached`]
//! runs each request in its own task instead and awaits it, so a client giving up
//! only stops waiting for the answer. The task is bounded as the handler was: by
//! the generation timeout, the concurrency limiter and any injected latency.
//! Streamed answers (SSE, gRPC streams) generate while their body is read, so they
//! still stop when the client does.

use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::Instrument;

/// `router`, each of whose requests runs as its own task.
pub(crate) fn detached(router: Router) -> Router {
    router.layer(axum::middleware::from_fn(finish_on_disconnect))
}

/// Runs `next` for `req` in a spawned task and awaits its answer.
async fn finish_on_disconnect(req: Request, next: Next) -> Response {
    let mut waiting = Waiting(true);
    let joined = tokio::spawn(next.run(req).in_current_span()).await;
    waiting.0 = false;
    match joined {
        Ok(resp) => resp,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        // Only a runtime shutting down cancels the task.
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// Notes a client that stopped waiting for its call, which still runs to the end.
struct Waiting(bool);

impl Drop for Waiting {
    fn drop(&mut self) {
        if self.0 {
            tracing::debug!("client went away; its call runs to the end");
        }
    }
}
