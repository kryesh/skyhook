//! Played sessions: real sessions whose model the test plays turn by turn, and
//! predicates over what they journal.

use super::tests::{attach, draft_fixture};
use super::*;
use crate::interaction::UiInteraction;
use crate::tests::bounded;
use serde_json::json;
use skyhook::job::JobTransition;
use skyhook::provider::protocol::{
    AssistantItem, BlockRef, Completion, ContextId, ItemKind, ModelRequest, ResponseEvent, ToolCall,
};
use skyhook::provider::{Provider, ProviderContext, ProviderError, ResponseStream};
use skyhook::session::{RequestSeq, SessionEvent, UserPart};
use skyhook::tool::policy::{AllowAll, Policy};
use std::sync::Arc;

/// The model a test plays: each request a played session sends waits for the
/// response the test streams back through its [`Turn`].
pub(in crate::tui) struct Model(pub(super) mpsc::UnboundedReceiver<Turn>);

/// One request the played model received, open until the test ends it.
pub(in crate::tui) struct Turn {
    /// The conversation it continues: the requesting agent's.
    pub(in crate::tui) context: ContextId,
    events: mpsc::UnboundedSender<Result<ResponseEvent, ProviderError>>,
}

struct Played(mpsc::UnboundedSender<Turn>);

struct Conversation {
    turns: mpsc::UnboundedSender<Turn>,
    context: ContextId,
}

impl Provider for Played {
    fn open_context(&self, context: ContextId) -> Result<Box<dyn ProviderContext>, ProviderError> {
        let turns = self.0.clone();
        Ok(Box::new(Conversation { turns, context }))
    }
}

impl ProviderContext for Conversation {
    fn invoke(&mut self, _: ModelRequest) -> ResponseStream {
        let (events, received) = mpsc::unbounded_channel();
        let context = self.context.clone();
        let _ = self.turns.send(Turn { context, events });
        Box::pin(futures_util::stream::unfold(
            received,
            async |mut received| Some((received.recv().await?, received)),
        ))
    }
}

impl Model {
    /// The next request the session sends.
    pub(in crate::tui) async fn turn(&mut self) -> Turn {
        bounded(self.0.recv())
            .await
            .expect("the played session is open")
    }
}

/// The next request `agent` sends, answering any other agent's on the way.
pub(in crate::tui) async fn next_turn(model: &mut Model, agent: &AgentId) -> Turn {
    let context = ContextId::from(agent);
    loop {
        let turn = model.turn().await;
        if turn.context == context {
            return turn;
        }
        turn.answer("noted");
    }
}

impl Turn {
    /// Stream `text` into the single-block item `item`.
    pub(in crate::tui) fn delta(&self, item: &str, kind: ItemKind, text: &str) {
        let block = BlockRef::single(item);
        let text = text.into();
        let _ = self
            .events
            .send(Ok(ResponseEvent::Delta { block, kind, text }));
    }

    pub(in crate::tui) fn complete(self, completion: Completion) {
        let _ = self.events.send(Ok(ResponseEvent::End(completion)));
    }

    /// End the response with `items`: a normal finish, which runs its calls.
    pub(in crate::tui) fn end(self, items: Vec<AssistantItem>) {
        self.complete(Completion::finished(items).unwrap());
    }

    pub(in crate::tui) fn answer(self, text: &str) {
        self.end(vec![AssistantItem::text("text", 0, text)]);
    }

    pub(in crate::tui) fn fail(self, error: ProviderError) {
        let _ = self.events.send(Err(error));
    }
}

/// A tool call item at `position`, its call ID also its item ID.
pub(in crate::tui) fn call(
    id: &str,
    tool: &str,
    arguments: serde_json::Value,
    position: u32,
) -> AssistantItem {
    AssistantItem::tool_call(id, position, ToolCall::new(id, tool, arguments).unwrap())
}

/// A new session on `launch`'s model, which the test plays.
pub(in crate::tui) async fn played(launch: &Launch) -> (SessionHandle, Model) {
    played_under(launch, Arc::new(AllowAll)).await
}

/// [`played`], its operations authorized by `policy`.
async fn played_under(launch: &Launch, policy: Arc<dyn Policy>) -> (SessionHandle, Model) {
    let (turns, model) = mpsc::unbounded_channel();
    let first = ("first".parse().unwrap(), launch.model.profile().clone());
    let harness = (launch.model.harness_builder(&launch.workspace).unwrap())
        .provider("test".parse().unwrap(), Arc::new(Played(turns)), [first])
        .policy(policy)
        .session_root(&launch.sessions)
        .shim_catalog(launch.catalog.clone())
        .build()
        .await
        .unwrap();
    (harness.new_session().await.unwrap(), Model(model))
}

/// An app open on a played session.
pub(in crate::tui) async fn played_fixture() -> (tempfile::TempDir, App, Model, SessionHandle) {
    played_fixture_under(Arc::new(AllowAll)).await
}

/// [`played_fixture`], its operations needing approval asking the returned prompts.
pub(in crate::tui) async fn attended_fixture() -> (
    tempfile::TempDir,
    App,
    Model,
    SessionHandle,
    mpsc::UnboundedReceiver<Prompt>,
) {
    let (ui, prompts) = UiInteraction::new();
    let policy = crate::interaction::tests::attended(true, ui);
    let (root, app, model, session) = played_fixture_under(Arc::new(policy)).await;
    (root, app, model, session, prompts)
}

async fn played_fixture_under(
    policy: Arc<dyn Policy>,
) -> (tempfile::TempDir, App, Model, SessionHandle) {
    let (root, mut app) = draft_fixture().await;
    let (session, model) = played_under(&app.launch, policy).await;
    attach(&mut app, session.clone());
    (root, app, model, session)
}

/// A call that fails before any job starts: an empty command line.
pub(in crate::tui) fn failing_call(id: &str) -> AssistantItem {
    call(id, "exec", json!({"command": []}), 0)
}

/// Run a host script on `session` that sleeps until cancelled.
pub(in crate::tui) fn sleeping_script(
    session: &SessionHandle,
) -> tokio::task::JoinHandle<Result<skyhook::tool::ToolOutput, skyhook::agent::HarnessError>> {
    let session = session.clone();
    tokio::spawn(async move { session.run_script("await sleep(60000);").await })
}

/// Prompt `session` as a user would; its turn waits on the played model.
pub(in crate::tui) fn ask(
    session: &SessionHandle,
    text: &str,
) -> tokio::task::JoinHandle<Result<String, skyhook::agent::HarnessError>> {
    let (session, text) = (session.clone(), text.to_owned());
    tokio::spawn(async move { session.prompt(text).await })
}

/// Fold the next observed update as the host does, returning it.
pub(in crate::tui) async fn observe_next(app: &mut App) -> RuntimeEvent {
    let event = bounded(app.recv_observation()).await;
    let event = event.expect("observation open");
    app.observe(event.clone());
    event.event
}

/// Fold observed updates as the host does until `done` holds.
pub(in crate::tui) async fn observe_until(app: &mut App, done: impl Fn(&App) -> bool) {
    observe_find(app, |app| done(app).then_some(())).await;
}

/// Fold observed updates as the host does until `find` finds something, returning it.
pub(in crate::tui) async fn observe_find<T>(app: &mut App, find: impl Fn(&App) -> Option<T>) -> T {
    bounded(async {
        loop {
            if let Some(found) = find(app) {
                return found;
            }
            observe_next(app).await;
            app.refresh();
            app.rebuild_content();
        }
    })
    .await
}

/// Fold observed updates as the host does until a record whose event `matches` commits.
pub(in crate::tui) async fn observe_record(app: &mut App, matches: impl Fn(&SessionEvent) -> bool) {
    observe_until(app, |app| recorded(&app.snapshot, &matches)).await;
}

/// Prompt `app`'s played session and end its turn with `items`, observing
/// until the answer commits.
pub(in crate::tui) async fn exchange(
    app: &mut App,
    model: &mut Model,
    prompt: &str,
    items: Vec<AssistantItem>,
) {
    let before = count(&app.snapshot, answered);
    let asked = ask(app.session().unwrap(), prompt);
    model.turn().await.end(items);
    bounded(asked).await.unwrap().unwrap();
    observe_until(app, |app| count(&app.snapshot, answered) > before).await;
}

/// Have the root of `session` start a child for each of `tasks`, its agent
/// call's arguments: each child's first request, in the order it arrived.
pub(in crate::tui) async fn delegate(
    session: &SessionHandle,
    model: &mut Model,
    tasks: &[serde_json::Value],
) -> Vec<Turn> {
    let _delegating = ask(session, "delegate");
    let calls = tasks.iter().enumerate().map(|(index, task)| {
        call(
            &format!("agent-{index}"),
            "agent",
            task.clone(),
            index as u32,
        )
    });
    model.turn().await.end(calls.collect());
    let mut turns = Vec::new();
    for _ in tasks {
        turns.push(model.turn().await);
    }
    turns
}

/// The call by which a child asks its parent which one.
pub(in crate::tui) fn asking() -> AssistantItem {
    let question = json!({"id": "pick", "prompt": "Which one?"});
    call("question", "ask", question, 0)
}

/// Have the root of `session` start a child for `task` that asks it a
/// question, and the root end its turn unanswered: the child's agent job
/// waits for its input.
pub(in crate::tui) async fn asking_child(
    session: &SessionHandle,
    model: &mut Model,
    task: serde_json::Value,
) {
    let mut turns = delegate(session, model, &[task]).await;
    turns.remove(0).end(vec![asking()]);
    model.turn().await.answer("later");
}

/// The agents `snapshot` saw start other than the root, in id order, as the
/// projection lists them: concurrent launches start in any order.
pub(in crate::tui) fn children(snapshot: &ObservationSnapshot) -> Vec<AgentId> {
    let records = snapshot.records.values();
    let started = records.filter(|record| {
        record.agent.parent().is_some() && matches!(record.event, SessionEvent::AgentStarted { .. })
    });
    let mut children: Vec<_> = started.map(|record| record.agent.clone()).collect();
    children.sort_by(|a, b| a.path().cmp(b.path()));
    children
}

/// Have the root of `app`'s played session start `count` children, each
/// holding its first request open: the children, and those requests.
pub(in crate::tui) async fn spawn_children(
    app: &mut App,
    model: &mut Model,
    count: u32,
) -> (Vec<AgentId>, Vec<Turn>) {
    let tasks: Vec<_> = (0..count)
        .map(|index| json!({"prompt": format!("child task {index}")}))
        .collect();
    let turns = delegate(app.session().unwrap(), model, &tasks).await;
    observe_until(app, |app| children(&app.snapshot).len() == tasks.len()).await;
    (children(&app.snapshot), turns)
}

/// Whether `snapshot` holds a record whose event `matches`.
pub(in crate::tui) fn recorded(
    snapshot: &ObservationSnapshot,
    matches: impl Fn(&SessionEvent) -> bool,
) -> bool {
    snapshot
        .records
        .values()
        .any(|record| matches(&record.event))
}

/// How many records `snapshot` holds whose event `matches`.
pub(in crate::tui) fn count(
    snapshot: &ObservationSnapshot,
    matches: impl Fn(&SessionEvent) -> bool,
) -> usize {
    let records = snapshot.records.values();
    records.filter(|record| matches(&record.event)).count()
}

/// The timestamp of the `nth` record whose event `matches`.
pub(in crate::tui) fn at(
    snapshot: &ObservationSnapshot,
    matches: impl Fn(&SessionEvent) -> bool,
    nth: usize,
) -> i64 {
    let mut records = (snapshot.records.values()).filter(|record| matches(&record.event));
    records.nth(nth).unwrap().timestamp_millis
}

/// Whether an event moves `job` to `state`.
pub(in crate::tui) fn job_state(
    job: JobId,
    state: JobTransition,
) -> impl Fn(&SessionEvent) -> bool {
    move |event| {
        matches!(event, SessionEvent::JobStateChanged { job: changed, state: to }
            if *changed == job && *to == state)
    }
}

/// Observe until `job` journals `state`.
pub(in crate::tui) async fn reached(app: &mut App, job: JobId, state: JobTransition) {
    observe_record(app, job_state(job, state)).await;
}

/// The job started for the call `id`, once there is one.
pub(in crate::tui) fn job_of_call(snapshot: &ObservationSnapshot, id: &str) -> Option<JobId> {
    snapshot
        .records
        .values()
        .find_map(|record| match &record.event {
            SessionEvent::JobCreated {
                job,
                origin: Some(origin),
                ..
            } if origin.call_id == id => Some(*job),
            _ => None,
        })
}

/// The sequence of the latest `ModelRequested` record.
pub(in crate::tui) fn latest_request(snapshot: &ObservationSnapshot) -> Option<RequestSeq> {
    let mut records = snapshot.records.values().rev();
    let request = records.find(|record| requested(&record.event));
    request.map(|record| record.sequence.request())
}

/// The error text of the first tool result `snapshot` holds.
pub(in crate::tui) fn tool_error(snapshot: &ObservationSnapshot) -> &str {
    let error = snapshot
        .records
        .values()
        .find_map(|record| match &record.event {
            SessionEvent::MessageCommitted {
                message: Message::Tool(results),
            } => results[0].result["error"].as_str(),
            _ => None,
        });
    error.unwrap()
}

/// Whether a response in `snapshot` streamed a block reading `text`.
pub(in crate::tui) fn streamed(snapshot: &ObservationSnapshot, text: &str) -> bool {
    let mut responses = snapshot.responses.values();
    responses.any(|response| response.blocks().iter().any(|block| block.text == text))
}

pub(in crate::tui) fn requested(event: &SessionEvent) -> bool {
    matches!(event, SessionEvent::ModelRequested { .. })
}

pub(in crate::tui) fn completed(event: &SessionEvent) -> bool {
    matches!(event, SessionEvent::ResponseCompleted { .. })
}

pub(in crate::tui) fn answered(event: &SessionEvent) -> bool {
    matches!(event, SessionEvent::ResponseCompleted { outcome, .. }
        if *outcome == skyhook::session::CompletedOutcome::Answer)
}

pub(in crate::tui) fn model_failed(event: &SessionEvent) -> bool {
    matches!(event, SessionEvent::ModelFailed { .. })
}

pub(in crate::tui) fn status(event: &SessionEvent) -> bool {
    matches!(event, SessionEvent::Status { .. })
}

/// Whether an event commits a message that `kind` accepts.
pub(in crate::tui) fn committed(kind: fn(&Message) -> bool) -> impl Fn(&SessionEvent) -> bool {
    move |event| matches!(event, SessionEvent::MessageCommitted { message } if kind(message))
}

pub(in crate::tui) fn user(message: &Message) -> bool {
    matches!(message, Message::User(_))
}

pub(in crate::tui) fn assistant(message: &Message) -> bool {
    matches!(message, Message::Assistant(_))
}

pub(in crate::tui) fn results(message: &Message) -> bool {
    matches!(message, Message::Tool(_))
}

/// Whether `message` delivers job events.
pub(in crate::tui) fn job_events(message: &Message) -> bool {
    let part = |part: &UserPart| matches!(part, UserPart::JobEvents { .. });
    matches!(message, Message::User(parts) if parts.iter().any(part))
}

pub(in crate::tui) fn job_ended(event: &SessionEvent) -> bool {
    matches!(event, SessionEvent::JobFinished { .. })
}

/// Whether an event ends `job`.
pub(in crate::tui) fn job_finished(job: JobId) -> impl Fn(&SessionEvent) -> bool {
    move |event| {
        matches!(event, SessionEvent::JobFinished { job: finished, .. }
            if *finished == job)
    }
}

/// What `snapshot` shows `agent` doing.
pub(in crate::tui) fn activity<'a>(
    snapshot: &'a ObservationSnapshot,
    agent: &AgentId,
) -> Option<&'a AgentActivity> {
    snapshot.activity.get(agent).map(|activity| &activity.state)
}

/// Whether `snapshot` shows `agent` stopped.
pub(in crate::tui) fn stopped(snapshot: &ObservationSnapshot, agent: &AgentId) -> bool {
    matches!(activity(snapshot, agent), Some(AgentActivity::Stopped(_)))
}

/// Send the agent job `job` the input `value`, a script expression, as its
/// owner would.
pub(in crate::tui) async fn answer_child(session: &SessionHandle, job: JobId, value: &str) {
    let answer = format!("(await tool.job({job}).send().set('value', {value})).unwrap();");
    session.run_script(&answer).await.unwrap();
}
