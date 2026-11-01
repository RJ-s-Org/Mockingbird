//! One LLM call for all root fields of a query.
//!
//! Root fields resolve concurrently and used to be generated separately, so
//! they could contradict each other: GitHub's `user(login: "vercel")` and
//! `organization(login: "vercel")` both answered. A [`Batch`] collects the
//! parts each root resolver is about to generate and lets the first of them
//! (the leader) answer all of them with [`Engine::generate_joint`].
//!
//! The leader waits until every root field has either joined or withdrawn
//! (it will not generate: planning failed, a status is forced), or at most
//! [`GRACE`]; a field that joins after the leader started is generated on
//! its own.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_graphql::Context;
use async_graphql::parser::types::{OperationType, Selection};
use mb_engine::{
    Engine, GeneratedResponse, GenerationContext, JointPart, OperationCall, ScenarioSpec,
};
use mb_state::StoredResource;
use serde_json::Value;
use tokio::sync::{Notify, watch};

use crate::MockRuntime;

/// How long the leader waits for the other root fields to join.
pub(crate) const GRACE: Duration = Duration::from_secs(2);

/// What one root field asks the batch to generate.
pub(crate) struct Part {
    pub key: String,
    pub op_id: String,
    pub call: OperationCall,
    pub schema: Option<Arc<Value>>,
    pub target: Option<StoredResource>,
    pub related: Vec<StoredResource>,
    pub scenario: Option<ScenarioSpec>,
    /// The engine the provider policy chose for the call (the same for
    /// every root field of a query: they share its tenancy).
    pub engine: Arc<Engine>,
}

type Answers = Arc<HashMap<String, Result<GeneratedResponse, String>>>;

pub(crate) struct Batch {
    expected: usize,
    state: Mutex<State>,
    complete: Notify,
    done: watch::Sender<Option<Answers>>,
}

#[derive(Default)]
struct State {
    parts: Vec<Part>,
    settled: usize,
    started: bool,
}

impl Batch {
    /// A batch for the root fields of the operation `ctx` executes: `None`
    /// unless it is a query with two or more plain root fields (no
    /// fragments or directives, which could leave a field unresolved).
    pub(crate) fn for_query(ctx: &Context<'_>) -> Option<Arc<Self>> {
        let operation = &ctx.query_env.operation.node;
        if operation.ty != OperationType::Query {
            return None;
        }
        let mut expected = 0;
        for item in &operation.selection_set.node.items {
            match &item.node {
                Selection::Field(f) if f.node.directives.is_empty() => {
                    if !f.node.name.node.starts_with("__") {
                        expected += 1;
                    }
                }
                _ => return None,
            }
        }
        (expected >= 2).then(|| {
            Arc::new(Self {
                expected,
                state: Mutex::new(State::default()),
                complete: Notify::new(),
                done: watch::channel(None).0,
            })
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A root field that will not generate gives up its place.
    pub(crate) fn withdraw(&self) {
        let mut state = self.lock();
        state.settled += 1;
        if state.settled >= self.expected {
            self.complete.notify_one();
        }
    }

    /// Generates `part` with the other root fields of the query; `None` when
    /// the leader had already started (the caller generates it alone).
    pub(crate) async fn join(
        &self,
        rt: &MockRuntime,
        part: Part,
    ) -> Option<Result<GeneratedResponse, String>> {
        let key = part.key.clone();
        let mut done = self.done.subscribe();
        let leader = {
            let mut state = self.lock();
            if state.started {
                return None;
            }
            state.parts.push(part);
            state.settled += 1;
            if state.settled >= self.expected {
                self.complete.notify_one();
            }
            state.parts.len() == 1
        };
        if leader {
            let all_in = self.lock().settled >= self.expected;
            if !all_in {
                let _ = tokio::time::timeout(GRACE, self.complete.notified()).await;
            }
            let parts = {
                let mut state = self.lock();
                state.started = true;
                std::mem::take(&mut state.parts)
            };
            let _ = self.done.send(Some(Arc::new(generate(rt, parts).await)));
        }
        let answers = loop {
            if let Some(answers) = done.borrow_and_update().clone() {
                break answers;
            }
            if done.changed().await.is_err() {
                return Some(Err("the batch leader stopped".into()));
            }
        };
        Some(match answers.get(&key) {
            Some(result) => result.clone(),
            None => Err(format!("no answer for root field {key}")),
        })
    }
}

async fn generate(
    rt: &MockRuntime,
    parts: Vec<Part>,
) -> HashMap<String, Result<GeneratedResponse, String>> {
    let mut joint = Vec::with_capacity(parts.len());
    let mut unknown = Vec::new();
    for part in &parts {
        match rt.operation(&part.op_id) {
            Some(op) => joint.push(JointPart {
                key: &part.key,
                op,
                call: &part.call,
                ctx: GenerationContext {
                    related: &part.related,
                    target: part.target.as_ref(),
                    forced_status: None,
                    scenario: part.scenario.as_ref(),
                    response_schema: part.schema.as_deref(),
                    earlier_requests: &[],
                    earlier_own_body: None,
                    earlier_answers: &[],
                    earlier_writes: &[],
                    update_guard: None,
                },
            }),
            None => unknown.push(part.key.clone()),
        }
    }
    let engine = parts.first().map_or(&rt.engine, |p| &p.engine);
    let results = if joint.len() == 1 {
        let p = &joint[0];
        vec![engine.generate(&rt.bundle, p.op, p.call, p.ctx).await]
    } else {
        engine.generate_joint(&rt.bundle, &joint).await
    };
    let mut answers: HashMap<String, Result<GeneratedResponse, String>> = joint
        .iter()
        .zip(results)
        .map(|(p, r)| (p.key.to_string(), r.map_err(|e| e.to_string())))
        .collect();
    for key in unknown {
        answers.insert(key, Err("unknown operation".into()));
    }
    answers
}
