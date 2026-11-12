//! Streamed answers and state change feeds.

use super::*;

/// Reads the target and related resources like [`generate_once`], keeps
/// the session alive, then opens an engine stream. Generated items are
/// never written to state.
#[cfg(any(feature = "sse", feature = "graphql", feature = "grpc"))]
pub(crate) async fn open_stream(
    rt: &std::sync::Arc<MockRuntime>,
    plan: &CallPlan<'_>,
    call: OperationCall,
    request_stream: Vec<Value>,
) -> Result<futures::stream::BoxStream<'static, mb_engine::StreamItem>, EngineError> {
    use std::sync::Arc;

    let op = plan.op;
    let mut state = StateAccess::new(rt);
    state.scope_to(op, &call);
    let target_id = target_id(op, &call);
    let (target, related) = async {
        let (target, mut related, followed) =
            read_context(&mut state, &plan.session, op, target_id.as_deref(), &call).await;
        follow_hops(
            &mut state,
            &plan.session,
            target.as_ref(),
            followed,
            &mut related,
        )
        .await;
        state.touch(&plan.session).await;
        (target, related)
    }
    .instrument(state_span())
    .await;
    let ctx = mb_engine::StreamContext {
        related,
        target,
        scenario: plan.scenario.map(|s| s.spec.clone()),
        request_stream,
        item_interval: plan.scenario.and_then(|s| s.stream_item_interval),
    };
    plan.engine
        .generate_stream(Arc::clone(&rt.bundle), op, call, ctx, plan.stream_items)
}

/// Whether a stored resource can be sent as a stream item, when the
/// caller reads less than the whole item (a GraphQL selection).
#[cfg(any(feature = "sse", feature = "graphql"))]
pub(crate) type ItemCheck = std::sync::Arc<dyn Fn(&Value) -> bool + Send + Sync>;

/// The data of every resource of `op`'s resource type upserted in
/// `session` from now on, when it is a valid stream item of `op` (or
/// passes `check`). Events missed because the receiver lagged are skipped.
/// Empty when `op` has no resource type.
#[cfg(any(feature = "sse", feature = "graphql"))]
pub(crate) fn change_feed(
    rt: &std::sync::Arc<MockRuntime>,
    session: SessionKey,
    op: &Operation,
    check: Option<ItemCheck>,
) -> futures::stream::BoxStream<'static, Value> {
    use futures::StreamExt;
    use futures::stream;
    use mb_state::ChangeKind;
    use std::sync::Arc;
    use tokio::sync::broadcast::error::RecvError;

    let Some(resource_type) = op.resource_type.clone() else {
        return stream::empty().boxed();
    };
    let feed = Feed {
        rt: Arc::clone(rt),
        op: op.clone(),
        session,
        resource_type,
        check,
        rx: rt.state.subscribe(),
    };
    stream::unfold(feed, |mut feed| async move {
        loop {
            let event = match feed.rx.recv().await {
                Ok(e) => e,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            };
            if event.kind != ChangeKind::Upserted
                || event.session != feed.session
                || event.resource_type != feed.resource_type
            {
                continue;
            }
            let found = feed
                .rt
                .state
                .get(&feed.session, &feed.resource_type, &event.id)
                .await;
            let Ok(Some(resource)) = found else { continue };
            let sendable = match &feed.check {
                Some(check) => check(&resource.data),
                None => feed
                    .rt
                    .engine
                    .conform_item(&feed.rt.bundle, &feed.op, &resource.data),
            };
            if sendable {
                return Some((resource.data, feed));
            }
        }
    })
    .boxed()
}

#[cfg(any(feature = "sse", feature = "graphql"))]
pub(super) struct Feed {
    pub(super) rt: std::sync::Arc<MockRuntime>,
    pub(super) op: Operation,
    pub(super) session: SessionKey,
    pub(super) resource_type: String,
    pub(super) check: Option<ItemCheck>,
    pub(super) rx: tokio::sync::broadcast::Receiver<mb_state::StateChanged>,
}
