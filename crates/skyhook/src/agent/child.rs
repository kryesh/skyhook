//! The agent tool: launching a child agent for a call, and restarting or resuming
//! one its owner's job runs again.

use crate::session::TodoItem;
use std::sync::{Arc, OnceLock, Weak};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::config::mode::{Mode, ModeName};
use crate::identity::AgentId;
use crate::tool::catalog::{RegistryError, TARGET, ToolOptions};
use crate::tool::policy::{Capability, CapabilitySet};
use crate::{
    provider::model::{ModelEntry, ModelRef},
    session::{CoreState, Message, Plan, SessionEvent, UserPart},
    tool::{ToolError, ToolRegistryBuilder, catalog},
};
use skyhook_tools::diagnostic::FailureSite;
use skyhook_worker::diagnostic::{Effects, Operation, Subject};
use skyhook_worker::job::JobId;

use super::{
    AgentCommand, AgentLaunch, HarnessError, Invocation, SessionRuntime, TurnFailure,
    launch::ChildAnswer,
    queue::QueuedInput,
    tools::{harness_error, runtime_unavailable},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentArgs {
    /// Task and context for the child
    pub(super) prompt: String,
    /// Initial checklist for multi-step work
    pub(super) todos: Option<Vec<TodoItem>>,
    /// Delegation depth available to the child; must be less than your available_depth
    #[serde(default)]
    pub(super) depth: usize,
    #[schemars(skip)]
    pub(super) model: Option<String>,
    #[schemars(skip)]
    pub(super) mode: Option<String>,
    #[schemars(skip)]
    pub(super) target: Option<skyhook_tools::target::TargetRef>,
    /// Child workspace override: absolute, or relative to the workspace selected by target;
    /// on the session host, created if missing
    pub(super) workspace: Option<skyhook_worker::op::TargetPath>,
}

/// The brief in an agent call's stored arguments, which keep the envelope the executor took
/// off before the handler.
#[derive(Deserialize)]
pub(crate) struct Brief {
    pub(crate) prompt: String,
}

/// Whether an agent holding `capabilities` may give a child this mode: it carries a
/// hint and grants nothing the agent lacks.
fn offers_mode(mode: &Mode, capabilities: &CapabilitySet) -> bool {
    let held = |capability: &Capability| capabilities.contains(capability);
    mode.hint.is_some() && mode.capabilities.iter().all(held)
}

/// A nullable choice among `(name, description)` pairs; None without any.
fn choice_input(summary: &str, choices: &[(&str, String)]) -> Option<Value> {
    if choices.is_empty() {
        return None;
    }
    let names = choices.iter().map(|(name, _)| json!(name));
    let names: Vec<Value> = names.chain([Value::Null]).collect();
    let lines = choices
        .iter()
        .map(|(name, text)| format!("\n- {name}{text}"));
    let description = format!("{summary}{}", lines.collect::<String>());
    Some(json!({"type": ["string", "null"], "enum": names, "description": description}))
}

pub(super) fn register_child_agent(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
    models: &indexmap::IndexMap<ModelRef, ModelEntry>,
    modes: &super::builder::Modes,
) -> Result<(), RegistryError> {
    let hinted = |(name, entry): (&ModelRef, &ModelEntry)| {
        let hint = entry.profile.hint.as_ref()?;
        Some((name.to_string(), format!(": {hint}")))
    };
    let models: Vec<_> = models.iter().filter_map(hinted).collect();
    let modes = (modes.configured()).map_or_else(Default::default, |(modes, _)| modes.clone());
    builder.register_checked(
        crate::tool::builtins::names::AGENT,
        "Start a child agent. Names are unique among your own installed children, including terminal children; other callers may reuse the same names. Send follow-ups or answers (keyed by question header for merged questions) with control({job: id, send: ...}); they arrive at its next model-request boundary. Earlier replies arrive as events; a foreground call's result carries them in `messages` instead. When a child asks, your own foreground call blocked on it moves to the background, so you can answer: an agent call returns the child's job in state waiting_input, a script call the script's job, still running; its question arrives as an event. Sending input to a completed, failed or interrupted child resumes its retained history under the same job ID",
        ToolOptions::default().job_role(crate::job::JobRole::Agent)
            .result::<crate::job::ChildResult>()
            .named()
            .requires(Capability::Agents)
            .conditional_input(TARGET, Capability::Targets, skyhook_tools::target::TargetRef::schema())
            .computed_input("model", move |_| {
                let choices: Vec<_> = models.iter().map(|(name, text)| (name.as_str(), text.clone())).collect();
                choice_input("Model for the child; omitted inherits yours", &choices)
            })
            .computed_input("mode", move |capabilities| {
                let offered = modes.iter().filter(|(_, mode)| offers_mode(mode, capabilities));
                let choices: Vec<_> = offered
                    .map(|(name, mode)| {
                        let granted: Vec<_> = mode.capabilities.iter().map(|capability| capability.as_str()).collect();
                        let granted = if granted.is_empty() { "none".to_owned() } else { granted.join(", ") };
                        let hint = mode.hint.as_ref().map_or("", skyhook_base::newtype::Prose::as_str);
                        (name.as_str(), format!(" [{granted}]: {hint}"))
                    })
                    .collect();
                choice_input("Mode limiting what the child can do; omitted inherits what you can", &choices)
            })
            .background()
            .input()
            .job_location(|input: &AgentArgs| crate::tool::catalog::JobLocation {
                target: input.target.clone(),
                workspace: input.workspace.clone(),
            }),
        Ok,
        move |input: AgentArgs| {
            let runtime = runtime_slot.get().and_then(Weak::upgrade);
            catalog::Invocation::reply(move |context| async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                let owner = context.agent();
                let unstarted = || ToolError::failed("parent agent never started")
                    .operation(Operation::Lookup, Subject::Label(format!("parent agent {owner}"))).effects(Effects::NotStarted);
                let delegation = runtime.held(owner, |contract, depth, _| (contract.profile.name.clone(), depth));
                let (model, depth) = delegation.ok_or_else(unstarted)?;
                let caller = (owner.depth(), context.capabilities(), model, depth);
                let location = context.job_location().clone();
                // The call connected a remote target, approved with it, before this ran.
                let facts = Some(context.job_target()?.facts());
                let (launch, prompt) = child_launch(&runtime, input, caller, location, facts)?;
                let child = runtime.next_child(owner).await.ok_or_else(unstarted)?;
                let created = context.create_job_workspace().await?;
                let job = context.job();
                let call = super::launch::Call { job, prompt };
                let launched = runtime.launch_child(child.clone(), launch, call);
                let launched = launched.await;
                let answer = launched.map_err(|error| {
                    skyhook_tools::builtins::atomic::left_by(created, launch_error(error, &child))
                })?;
                run_child_request(&runtime, &context, &child, answer).await
            })
        },
    )?;
    Ok(())
}

/// Why a child agent could not start, or start again.
fn launch_error(error: HarnessError, child: &crate::identity::AgentId) -> ToolError {
    let subject = Subject::Label(format!("child agent {child}"));
    let error = match error {
        error if stopped_starting(&error) => (ToolError::interrupted()
            .operation(Operation::Prepare, subject))
        .effects(Effects::NotStarted),
        // A reused name is a follow-up aimed at the wrong tool.
        HarnessError::Job(error @ crate::job::JobError::NameTaken { job, .. }) => {
            (ToolError::invalid_arguments(error).operation(Operation::Validate, Subject::Job(job)))
                .effects(Effects::NotStarted)
        }
        error => {
            (harness_error(error).operation(Operation::Create, subject)).effects(Effects::Unknown)
        }
    };
    error.at(FailureSite::Host)
}

/// Check `input` as the launch its call makes, with the prompt that begins it,
/// against its caller's depth, what it now holds, which model it runs and how deep
/// it may delegate.
fn child_launch(
    runtime: &SessionRuntime,
    input: AgentArgs,
    (depth, caller, model, available_depth): (usize, &CapabilitySet, ModelRef, usize),
    location: skyhook_tools::target::ExecutionLocation,
    facts: Option<skyhook_worker::target::Facts>,
) -> Result<(AgentLaunch, String), ToolError> {
    // Only hinted models were offered; anything else is unknown to the caller.
    let model = match input.model {
        Some(name) => {
            let hinted = |model: &ModelRef| {
                (runtime.harness.models.get(model))
                    .is_some_and(|entry| entry.profile.hint.is_some())
            };
            let model = name.parse::<ModelRef>().ok().filter(hinted);
            model.ok_or_else(|| invalid_argument(["model"], format!("unknown model `{name}`")))?
        }
        None => model,
    };
    let (mode, capabilities) = match &input.mode {
        None => (None, caller.clone()),
        Some(name) => {
            let offered = |(_, mode): &(&ModeName, &Mode)| offers_mode(mode, caller);
            let Some((name, _)) = runtime
                .harness
                .modes
                .configured()
                .and_then(|(modes, _)| modes.get_key_value(name.as_str()))
                .filter(offered)
            else {
                return Err(invalid_argument(["mode"], format!("unknown mode `{name}`")));
            };
            // Interaction follows the caller, as the root's follows the host.
            let ceiling = &runtime.capabilities & caller;
            let applied = runtime.applied(
                name,
                depth + 1,
                &location,
                input.depth,
                &ceiling,
                facts.clone(),
            );
            let (applied, granted) = applied.map_err(harness_error)?;
            (Some(applied), granted)
        }
    };
    if input.depth >= available_depth {
        return Err(invalid_argument(
            ["depth"],
            format!("depth must be less than the caller's available depth of {available_depth}"),
        ));
    }
    let launch = AgentLaunch::New {
        model,
        todos: input.todos,
        available_depth: input.depth,
        location,
        mode,
        capabilities,
        facts: facts.map(Box::new),
    };
    Ok((launch, input.prompt))
}

/// Run an agent call's installed child again with owner input, its loop started
/// again under its journaled contract if it parked or a restart left none, for
/// its owner as its journaled contract now lets it act. The runtime reference is
/// weak: jobs must not retain their own manager.
pub(super) fn child_resume_handler(
    runtime: Weak<SessionRuntime>,
    restarting: crate::job::Restarting,
) -> crate::job::ResumeHandler {
    Arc::new(move |value, input| {
        let (runtime, restarting) = (runtime.upgrade(), restarting.clone());
        Box::pin(async move {
            let runtime = runtime.ok_or_else(runtime_unavailable)?;
            let crate::job::Restarting {
                job,
                owner,
                parent,
                location,
                cancellation,
                restart: _,
            } = restarting;
            let cancelled = cancellation.clone();
            let resumed = async {
                let (journaled, (_, capabilities), _) =
                    (runtime.journaled(&owner).await).map_err(|error| not_restarted(job, error))?;
                let authorization = crate::tool::authorization::AuthorizationSubject {
                    agent: owner,
                    job,
                    parent,
                    capabilities,
                    cancellation,
                };
                let context = crate::tool::ToolContext::new(
                    authorization,
                    location,
                    journaled.contract.location,
                    input,
                    runtime.jobs.clone(),
                );
                let child = runtime.jobs.child_agent(job).await.ok_or_else(|| {
                    ToolError::failed("the call has no child to resume").at(FailureSite::Host)
                })?;
                let content = value.iter().map(owner_input).collect();
                let answer = restart_child(&runtime, job, &child, content).await?;
                run_child_request(&runtime, &context, &child, answer).await
            };
            // As dispatch ran the call: a cancellation requested before the restart
            // took its child stops the child starting, and is the outcome.
            let result = resumed.await.map(crate::job::Completion::Reply);
            crate::tool::executor::cancellation_result(result, cancelled.is_cancelled())
        })
    })
}

fn owner_input(value: &serde_json::Value) -> UserPart {
    let text = format!("Owner input: {value}");
    UserPart::ParentInput { text }
}

/// Start child agent `child`'s next invocation with owner input `content`,
/// releasing the answer the last one held as a reply. The receiver resolves with
/// the new invocation's answer.
async fn restart_child(
    runtime: &Arc<SessionRuntime>,
    job: JobId,
    child: &AgentId,
    content: Vec<UserPart>,
) -> Result<ChildAnswer, ToolError> {
    let started = child.clone();
    let restart =
        move |state: &CoreState| restart(state, job, started, content, std::convert::identity);
    let restarted = runtime.with_loops(child, restart).await;
    restarted.map_err(|error| not_restarted(job, error))
}

/// Give child agent `child` owner input: it joins the open invocation at its
/// next request boundary, or, once that resolved, starts the next as
/// [`restart_child`] does.
async fn owner_update(
    runtime: &Arc<SessionRuntime>,
    job: JobId,
    child: &AgentId,
    content: Vec<UserPart>,
) -> Result<Option<ChildAnswer>, ToolError> {
    let updated = child.clone();
    // A parked child has no facts until the change that starts its loop.
    let input = move |state: &CoreState| {
        let child = updated;
        let facts = state.agents.get(&child);
        if facts.is_none_or(|facts| facts.invocation == Invocation::Resolved) {
            return restart(state, job, child, content, Some);
        }
        let input = QueuedInput {
            options: Default::default(),
            content,
            attachments: Vec::new(),
            cancellation: Default::default(),
            committed: oneshot::channel().0,
        };
        let queued = AgentCommand::QueuedInputs(vec![input]);
        Ok(Plan::new(Vec::new(), move |state, _| {
            state.agents.send(&child, queued);
            None
        }))
    };
    let updated = runtime.with_loops(child, input).await;
    updated.map_err(|error| not_restarted(job, error))
}

/// What [`restart_child`] commits once `child`'s loop runs, its answer's
/// receiver passed `into` its result.
fn restart<T: Send + 'static>(
    state: &CoreState,
    job: JobId,
    child: AgentId,
    content: Vec<UserPart>,
    into: fn(ChildAnswer) -> T,
) -> Result<Plan<T>, HarnessError> {
    let mut events = crate::job::Jobs::release_answer(state, job)?;
    // The owner input commits as the invocation opens, as a launch's prompt does.
    let message = (!content.is_empty()).then(|| SessionEvent::MessageCommitted {
        message: Message::User(content),
    });
    events.extend(message.map(|message| (child.clone(), message)));
    Ok(Plan::new(events, move |state, _| {
        crate::job::woken(state, job);
        let (done, received) = oneshot::channel();
        let facts = (state.agents.get_mut(&child)).expect("the change runs its loop");
        facts.invocation = Invocation::Open;
        state.agents.begin(&child, done);
        into(received)
    }))
}

/// Wait for `child`'s answer to the invocation whose answer arrives on `done_rx`,
/// passing on what its owner sends meanwhile.
async fn run_child_request(
    runtime: &Arc<SessionRuntime>,
    context: &crate::tool::ToolContext,
    child: &crate::identity::AgentId,
    mut done_rx: super::launch::ChildAnswer,
) -> Result<String, ToolError> {
    let job = context.job();
    // Resumption runs this outside dispatch, which would otherwise bind the host site.
    let failure = |operation, effects| {
        move |error: ToolError| {
            error
                .operation(operation, Subject::Job(job))
                .at(FailureSite::Host)
                .effects(effects)
        }
    };
    loop {
        tokio::select! {
            result = &mut done_rx => {
                let reply = result
                    .map_err(|_| failure(Operation::Receive, Effects::Unknown)(ToolError::failed("child agent stopped before returning a result")))?
                    .map_err(|error| failure(Operation::Wait, Effects::Unknown)(match error {
                        TurnFailure::Interrupted => ToolError::interrupted(),
                        other => ToolError::failed(other),
                    }))?;
                let inputs = context.drain_input_or_close().await;
                if inputs.is_empty() { return Ok(reply); }
                // Owner input continues this invocation instead of finishing the job, so
                // the answer above, held for its result, is released as a reply.
                let content = inputs.iter().map(owner_input).collect();
                done_rx = restart_child(runtime, job, child, content).await
                    .map_err(|error| error.with_result(crate::job::Completion::Reply(reply).into()))?;
            }
            value = context.receive() => {
                let value = match value {
                    Ok(value) => value,
                    Err(error) => {
                        runtime.interrupt_tree(child).await;
                        return Err(failure(Operation::Receive, Effects::Unknown)(error));
                    }
                };
                if !runtime.jobs.answer(job, value.clone()).await
                    .map_err(|error| failure(Operation::Send, Effects::Unknown)(error.into()))?
                {
                    // Owner updates join the open turn at its next request boundary, as
                    // queued root prompts do; once it resolved, they restart it.
                    let sent = owner_update(runtime, job, child, vec![owner_input(&value)]).await;
                    if let Some(restarted) = sent? {
                        done_rx = restarted;
                    }
                }
            }
        }
    }
}

fn invalid_argument<S: AsRef<str>>(
    path: impl IntoIterator<Item = S>,
    message: impl std::fmt::Display,
) -> ToolError {
    ToolError::invalid_arguments(message)
        .operation(Operation::Validate, Subject::argument(path))
        .effects(Effects::NotStarted)
}

/// How job `job` reports that what runs it again could not start: an agent call
/// whose child or owner could not be started, as either restart kind finds it.
pub(super) fn not_restarted(job: JobId, error: crate::agent::HarnessError) -> ToolError {
    let error = match error {
        error if stopped_starting(&error) => ToolError::interrupted(),
        error => harness_error(error),
    };
    error
        .operation(Operation::Send, Subject::Job(job))
        .at(FailureSite::Host)
        .effects(Effects::NotStarted)
}

/// A session that no longer starts agents refused it, which interrupts the call.
fn stopped_starting(error: &HarnessError) -> bool {
    matches!(
        error,
        HarnessError::Session(crate::session::SessionError::Refused(_))
    )
}

#[cfg(test)]
mod tests {
    use crate::agent::tests::*;
    use crate::provider::model::ModelProfile;
    use crate::tests::bounded;
    use skyhook_tools::target::ExecutionLocation;

    /// An agent may name only hinted models, and hinted modes within what it holds
    /// itself; with none to name, the input is absent. A chosen mode is the child's.
    #[tokio::test]
    async fn children_take_only_hinted_models_and_modes_their_parent_could_hold() {
        use crate::config::mode::Mode;
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let agent_input = |index: usize, name: &str| {
            let requests = requests.lock().unwrap();
            let agent = requests[index]
                .tools
                .iter()
                .find(|tool| tool.name == "agent");
            agent.unwrap().input_schema["properties"][name].clone()
        };
        let plain = builder(root.path(), requests.clone())
            .build()
            .await
            .unwrap();
        let session = ephemeral_session(&plain).await;
        session.prompt("root request").await.unwrap();
        assert!(agent_input(0, "model").is_null() && agent_input(0, "mode").is_null());
        shutdown_session(session).await;
        requests.lock().unwrap().clear();

        let mode = |capabilities: &[Capability], hint: Option<&str>| Mode {
            capabilities: listed(capabilities),
            instructions: Some(format!("Holds {}.", capabilities.len()).parse().unwrap()),
            hint: hint.map(|hint| hint.parse().unwrap()),
        };
        let (read, agents) = (Capability::Read, Capability::Agents);
        let modes = [
            (
                "work",
                mode(
                    &[read.clone(), Capability::Write, agents.clone()],
                    Some("Works"),
                ),
            ),
            (
                "scout",
                mode(&[read.clone(), agents.clone()], Some("Delegates reading")),
            ),
            ("idle", mode(&[], Some("Thinks"))),
            (
                "online",
                mode(
                    &[read.clone(), Capability::Network],
                    Some("Looks things up"),
                ),
            ),
            ("secret", mode(std::slice::from_ref(&read), None)),
        ];
        let cheap = ModelProfile {
            hint: Some("Cheap".parse().unwrap()),
            ..crate::tests::profile("cheap", false)
        };
        let provider = scripted_provider(&requests, (0..3).map(|_| answer("done")));
        let harness = serving(root.path(), provider, [("cheap", cheap)])
            .session_root(root.path().join("hinted"))
            .max_child_depth(2)
            .modes(
                modes
                    .map(|(name, mode)| (name.parse().unwrap(), mode))
                    .into(),
            )
            .build()
            .await
            .unwrap();
        let session = ephemeral_session(&harness).await;
        session.prompt("root request").await.unwrap();
        // `work` holds no network, and `secret` has no hint.
        assert_eq!(agent_input(0, "model")["enum"], json!(["test/cheap", null]));
        let offered = agent_input(0, "mode");
        assert_eq!(offered["enum"], json!(["work", "scout", "idle", null]));
        let description = offered["description"].as_str().unwrap();
        assert!(description.contains("\n- scout [read, agents]: Delegates reading"));
        assert!(description.contains("\n- idle [none]: Thinks"));
        assert!(!description.contains("interactive"));

        // Choices outside the offered enums are refused at admission.
        for refused in ["{mode:'secret'}", "{mode:'online'}", "{model:'test'}"] {
            let script =
                format!("return (await tool.agent({{prompt:'no', ...{refused}}})).unwrap();");
            assert!(session.run_script(script).await.is_err(), "{refused}");
        }
        let child =
            "return await tool.agent({prompt:'go', depth:1, mode:'scout', model:'test/cheap'});";
        assert_eq!(
            session.run_script(child).await.unwrap().value["value"]["result"]["reply"],
            "done"
        );
        // The child holds less than its parent, so it is offered less.
        assert_eq!(
            agent_input(1, "mode")["enum"],
            json!(["scout", "idle", null])
        );
        let child = requests.lock().unwrap()[1].clone();
        assert_eq!(child.model.as_str(), "cheap");
        assert!(
            child.system[0]
                .text
                .contains("<mode name=\"scout\">\nHolds 2.")
        );
        let records = session.runtime.store.records();
        let started = events!(&records, SessionEvent::AgentStarted { mode, capabilities, .. } => (mode.clone().map(|mode| mode.name), capabilities.clone()));
        // Interaction follows the parent; the mode grants the rest.
        let held = [read.clone(), agents.clone(), Capability::Interactive]
            .into_iter()
            .collect();
        assert_eq!(started[1], (Some(mode_name("scout")), held));
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn child_target_lookup_preserves_safe_admission_diagnostics() {
        use skyhook_tools::diagnostic::Cause;
        use skyhook_worker::diagnostic::{Effects, Operation, Subject};

        let root = tempfile::tempdir().unwrap();
        let harness = builder(root.path(), Requests::default())
            .capabilities(Capability::ALL.into_iter().collect())
            .build()
            .await
            .unwrap();
        let session = ephemeral_session(&harness).await;
        for inherited in [false, true] {
            let mut executor = session.runtime.executor.clone();
            let mut args = json!({"prompt":"no"});
            if inherited {
                executor = executor
                    .with_capabilities(CapabilitySet::default())
                    .with_location(ExecutionLocation::named(
                        "private-missing-target".parse().unwrap(),
                        crate::tests::text(root.path()),
                    ));
            } else {
                executor = executor.with_capabilities(Capability::ALL.into_iter().collect());
                args["target"] = json!("private-missing-target");
            }
            let error = bounded(executor.execute(session.root.clone(), "agent", args, None))
                .await
                .unwrap_err();
            let diagnostic = error.diagnostic();
            assert_eq!(
                diagnostic.cause,
                Cause::InvalidArguments("unknown target".into())
            );
            assert_eq!(diagnostic.context.operation, Operation::Lookup);
            assert_eq!(diagnostic.context.subject, Subject::argument(["target"]));
            assert_eq!(diagnostic.context.effects, Effects::NotStarted);
            assert!(
                !diagnostic
                    .render(CapabilitySet::default().target_view())
                    .contains("private-missing-target")
            );
        }
        shutdown_session(session).await;
    }

    /// A child in a mode is held to it: depth still withholds `agents`, a mode it could
    /// not hold is unknown to it, and a restart returns it to the mode it started in.
    #[tokio::test]
    async fn a_child_keeps_its_mode_through_depth_delegation_and_restart() {
        use crate::config::mode::Mode;
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let mode = |capabilities: &[Capability], instructions: &str| Mode {
            capabilities: listed(capabilities),
            instructions: Some(instructions.parse().unwrap()),
            hint: Some("Hinted".parse().unwrap()),
        };
        let (read, write, agents) = (Capability::Read, Capability::Write, Capability::Agents);
        let build = |scout: Mode, responses: Vec<Vec<ResponseEvent>>| {
            let provider = scripted_provider(&requests, responses);
            let modes = [
                (
                    "work",
                    mode(&[read.clone(), write.clone(), agents.clone()], "Work."),
                ),
                ("scout", scout),
            ];
            // No turn is scripted for the completions: the held window keeps them.
            test_builder(root.path(), &sessions, provider, false)
                .coalescing(inbox::Coalescing::held().0)
                .max_child_depth(2)
                .modes(
                    modes
                        .map(|(name, mode)| (name.parse().unwrap(), mode))
                        .into(),
                )
                .build()
        };
        let scout = mode(&[read.clone(), agents.clone()], "Scout.");
        let widen = tool_call(0, "widen", "agent", json!({"prompt": "no", "mode": "work"}));
        let responses = vec![answer("leaf"), response(vec![widen]), answer("held")];
        let session = build(scout.clone(), responses).await.unwrap();
        let session = session.new_session().await.unwrap();
        let spawn = |depth: usize| {
            format!("return await tool.agent({{prompt:'go', mode:'scout', depth:{depth}}});")
        };
        assert_eq!(
            session.run_script(spawn(0)).await.unwrap().value["value"]["result"]["reply"],
            "leaf"
        );
        assert_eq!(
            session.run_script(spawn(1)).await.unwrap().value["value"]["result"]["reply"],
            "held"
        );
        let records = session.runtime.store.records();
        let started = events!(&records, SessionEvent::AgentStarted { mode: Some(mode), capabilities, .. } if mode.name.as_str() == "scout" => capabilities.clone());
        let interactive = Capability::Interactive;
        assert_eq!(
            started,
            [
                CapabilitySet::from_iter([read.clone(), interactive.clone()]),
                CapabilitySet::from_iter([read.clone(), agents.clone(), interactive.clone()])
            ]
        );
        let job = events!(&records, SessionEvent::JobCreated { job, tool, .. } if tool == "agent" => *job)
            [1];
        {
            let captured = requests.lock().unwrap();
            let offers = |index: usize| {
                captured[index]
                    .tools
                    .iter()
                    .any(|tool| tool.name == "agent")
            };
            assert_eq!((offers(0), offers(1)), (false, true));
            let Some(crate::provider::protocol::Message::Tool(results)) =
                captured[2].messages().last()
            else {
                panic!("expected the refused delegation");
            };
            assert!(results[0].is_error, "{}", results[0].result);
        }
        let id = session.id();
        shutdown_session(session).await;

        // The configuration now widens `scout`; the retained child keeps the edition
        // it started in.
        let widened = mode(&[read.clone(), write.clone(), agents.clone()], "Changed.");
        let responses = vec![answer("again"), answer("fresh")];
        let harness = build(widened.clone(), responses).await.unwrap();
        let resumed = harness.resume_session(id).await.unwrap();
        resumed.running().await.unwrap();
        resumed.runtime.jobs.send(job, json!("more")).await.unwrap();
        assert_eq!(
            terminal(&resumed, job).await.state,
            crate::job::JobState::Completed
        );
        let child = requests.lock().unwrap().last().unwrap().clone();
        assert!(
            child.system[0].text.contains("Scout.") && !child.system[0].text.contains("Changed.")
        );
        assert!(!child.tools.iter().any(|tool| tool.name == "write"));
        let agent = child
            .tools
            .iter()
            .find(|tool| tool.name == "agent")
            .unwrap();
        assert_eq!(
            agent.input_schema["properties"]["mode"]["enum"],
            json!(["scout", null])
        );
        // A child starting now runs the configured definition.
        let fresh = resumed.run_script(spawn(0)).await.unwrap();
        assert_eq!(fresh.value["value"]["result"]["reply"], "fresh");
        let child = requests.lock().unwrap().last().unwrap().clone();
        assert!(child.system[0].text.contains("Changed."));
        shutdown_session(resumed).await;
        // As decoded from the journal: the first child recorded `scout`'s first
        // edition, with its hint, and the new child its second.
        let opened = crate::session::SessionStore::open(&sessions, id, None).await;
        let records = opened.unwrap().records();
        let recorded = events!(&records, SessionEvent::AgentStarted { mode: Some(mode), .. } if mode.name.as_str() == "scout" => mode.definition.clone());
        assert_eq!(
            recorded,
            [Some(edition(&scout)), None, Some(edition(&widened))]
        );
    }

    /// Children starting at once in a mode the session has not used record its
    /// edition once.
    #[tokio::test(flavor = "multi_thread")]
    async fn children_starting_together_record_their_mode_once() {
        use crate::config::mode::Mode;
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let mode = |capabilities: &[Capability]| Mode {
            capabilities: listed(capabilities),
            instructions: None,
            hint: Some("Hinted".parse().unwrap()),
        };
        let modes = [
            ("work", mode(&[Capability::Read, Capability::Agents])),
            ("look", mode(&[Capability::Read])),
        ];
        let provider = scripted_provider(&requests, (0..8).map(|_| answer("done")));
        let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
            .max_child_depth(1)
            .modes(
                modes
                    .map(|(name, mode)| (name.parse().unwrap(), mode))
                    .into(),
            )
            .build()
            .await
            .unwrap();
        let session = ephemeral_session(&harness).await;
        let script = "const all = [...Array(8)].map(() => tool.agent({prompt:'go', mode:'look'})); \
                      return (await Promise.all(all)).length;";
        assert_eq!(session.run_script(script).await.unwrap().value["value"], 8);
        let records = session.runtime.store.records();
        let looks = events!(&records, SessionEvent::AgentStarted { mode: Some(mode), .. } if mode.name.as_str() == "look" => mode.definition.is_some());
        assert_eq!(
            (
                looks.len(),
                looks.iter().filter(|recorded| **recorded).count()
            ),
            (8, 1)
        );
        shutdown_session(session).await;
    }

    /// A background handle reports the child's workspace as written, not where
    /// the tool runs; the child runs where it resolves.
    #[tokio::test]
    async fn background_child_handle_reports_the_childs_workspace() {
        use skyhook_worker::diagnostic::Effects;
        let root = tempfile::tempdir().unwrap();
        let provider = scripted_provider(&Requests::default(), [answer("child done")]);
        // No turn is scripted for the completion: the held window keeps it.
        let builder = test_builder(root.path(), &root.path().join("sessions"), provider, false);
        let harness = builder.coalescing(inbox::Coalescing::held().0).build();
        let session = ephemeral_session(&harness.await.unwrap()).await;
        let workspace = &session.runtime.harness.workspace;
        std::fs::create_dir(workspace.join("sub")).unwrap();
        std::os::unix::fs::symlink("sub", workspace.join("alias")).unwrap();
        let arguments = json!({"prompt":"work", "workspace":"alias", "bg":true, "name":"worker"});
        let executor = &session.runtime.executor;
        let launched = executor.execute(session.root.clone(), "agent", arguments, None);
        let launched = launched.await.unwrap();
        let written = workspace.join("alias");
        let handle = launched.output.value;
        assert_eq!(
            handle["meta"]["workspace"],
            written.to_string_lossy().as_ref()
        );
        let job = terminal(&session, launched.job).await;
        assert_eq!(job.location.workspace, crate::tests::text(&written));
        let records = session.runtime.store.records();
        let started = events!(&records, SessionEvent::AgentStarted { owner_job: Some(_), location, .. } => location.workspace.clone());
        let resolved = std::fs::canonicalize(workspace.join("sub")).unwrap();
        assert_eq!(started, [crate::tests::text(resolved)]);
        // A launch its checks refuse creates no workspace; one refused once
        // the workspace is created reports it.
        let fresh = workspace.join("fresh");
        for (arguments, effects) in [
            (json!({"depth": 99}), Effects::NotStarted),
            (json!({"name": "worker"}), Effects::PartialChange),
        ] {
            let mut launch = json!({"prompt":"work", "workspace":"fresh"});
            launch
                .as_object_mut()
                .unwrap()
                .extend(arguments.as_object().unwrap().clone());
            let refused = executor.execute(session.root.clone(), "agent", launch, None);
            let refused = bounded(refused).await.unwrap_err();
            assert_eq!(refused.diagnostic().context.effects, effects, "{refused:?}");
            assert_eq!(fresh.is_dir(), effects == Effects::PartialChange);
        }
        shutdown_session(session).await;
    }
}
