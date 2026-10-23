//! Agent command loop and completion coordination.

use super::*;
use recovery::{FailureRecord, TurnError};

// This is only the retained command loop's state; the core owns job liveness,
// pending deliveries and owner forwarding.
enum DriverPhase {
    // The root has no waiter to settle; whether it is parked is its published turn.
    Root,
    Child(ChildPhase),
}

enum ChildPhase {
    Idle,
    Running {
        completion: Option<ChildCompletion>,
    },
    Answered {
        answer: String,
        completion: Option<ChildCompletion>,
    },
}

/// Who took a child's answer when its invocation completed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Handoff {
    /// The waiter has it, so the owning job finishes and wakes the owner itself.
    Waiter,
    /// Consumed callback-free; the owner still needs a wake.
    Consumed,
}

/// Replace a child's phase, yielding the waiter the old phase held.
fn take_completion(phase: &mut ChildPhase, next: ChildPhase) -> Option<ChildCompletion> {
    match std::mem::replace(phase, next) {
        ChildPhase::Running { completion } | ChildPhase::Answered { completion, .. } => completion,
        ChildPhase::Idle => None,
    }
}

impl DriverPhase {
    fn is_child(&self) -> bool {
        matches!(self, Self::Child(_))
    }

    /// A child answered, and its invocation has yet to resolve.
    fn answered_open(&self) -> bool {
        matches!(self, Self::Child(ChildPhase::Answered { .. }))
    }

    /// The waiter the turn's caller settles: the root's, never a child's own.
    fn begin(&mut self, completion: Option<RequestCompletion>) -> Option<RequestCompletion> {
        let phase = match self {
            Self::Root => return completion,
            Self::Child(phase) => phase,
        };
        // A new explicit waiter supersedes the old one; callback-free queued
        // input and descendant turns keep the current invocation's waiter.
        let completion = match completion {
            Some(RequestCompletion::Child(completion)) => Some(completion),
            None => None,
            root => return root,
        };
        let previous = take_completion(phase, ChildPhase::Idle);
        *phase = ChildPhase::Running {
            completion: completion.or(previous),
        };
        None
    }

    fn answered(&mut self, answer: String) {
        if let Self::Child(phase) = self {
            let completion = take_completion(phase, ChildPhase::Idle);
            *phase = ChildPhase::Answered { answer, completion };
        }
    }

    // Whether the failure reached this invocation's waiter, which is what
    // finishes the owning job. Parking is the published turn's, not the phase's.
    fn fail(&mut self, error: TurnFailure) -> bool {
        let phase = match self {
            Self::Root => return false,
            Self::Child(phase) => phase,
        };
        let Some(completion) = take_completion(phase, ChildPhase::Idle) else {
            return false;
        };
        completion.send(Err(error)).is_ok()
    }

    // Consume the answer even without a waiter, so a later wakeup does not
    // publish AgentCompleted again for this invocation.
    // `None` means there was no answer to consume.
    fn complete(&mut self) -> Option<Handoff> {
        let Self::Child(phase @ ChildPhase::Answered { .. }) = self else {
            return None;
        };
        let ChildPhase::Answered { answer, completion } =
            std::mem::replace(phase, ChildPhase::Idle)
        else {
            return None;
        };
        let handed = completion.is_some_and(|completion| completion.send(Ok(answer)).is_ok());
        Some(if handed {
            Handoff::Waiter
        } else {
            Handoff::Consumed
        })
    }
}

/// What a turn begins with: input whose selection and message are still to
/// commit, or work its agent took and committed, for its waiter if it has one:
/// delivered events together with their selection, or queued input. The session
/// may have refused that work, which fails the turn, or an interrupt stopped it
/// first, which the turn then ends.
enum Begins {
    Input(Vec<UserPart>, Option<RequestCompletion>, Selection),
    Taken(Result<(), queue::Refusal>, Option<RequestCompletion>),
}

/// What an agent's loop takes next.
enum Work {
    Command(AgentCommand),
    /// Deliveries its owner has yet to see.
    Jobs,
}

/// What an agent waiting for work is given.
enum Taken {
    /// Work, with the token of the turn it begins, and the turn the agent returns
    /// to if the work runs none. The token is already cancelled when an
    /// interrupt stopped the invocation the work begins.
    Work(Work, CancellationToken, Turn),
    /// Its answered invocation has nothing left to keep it open.
    Settle,
    /// The session refuses new turns, and the commands the agent held with them.
    Refused(crate::session::SessionPhase, Vec<AgentCommand>),
    /// A quiescent child's loop was let go: its job runs it again.
    Parked,
    Gone,
}

/// What an agent waiting for work decided to take: what it is given, or what
/// taking it consumes.
enum Taking {
    Given(Taken),
    Park,
    Refused,
    Command,
    Jobs,
}

/// Whether `id`, between turns, takes the deliveries pending for it: only while
/// the session takes them, and not those an invocation yet to begin owns, as
/// its first turn carries them.
pub(super) fn takes_deliveries(state: &CoreState, id: &AgentId) -> bool {
    let unbegun = state.jobs.unbegun(id).is_some();
    state.agents.flow() == Flow::Open && state.jobs.has_pending(id) && !unbegun
}

/// Whether live jobs, queued input or pending deliveries keep `id`'s answered
/// invocation open.
fn kept_open(state: &CoreState, id: &AgentId) -> bool {
    let queued = state.agents.get(id).is_some_and(agents::AgentFacts::queued);
    queued || state.jobs.has_live(id) || state.jobs.has_pending(id)
}

impl SessionRuntime {
    /// Release a child's held answer to its owner as a reply. Only for paths that
    /// do not finish the owning job: a completing job's answer is its result.
    async fn wake_owner(&self, owner_job: Option<JobId>) {
        if let Some(job) = owner_job {
            // Refused, the answer stays held; ending the job releases it.
            let _ = self.jobs.notify_owner(job).await;
        }
    }

    /// `id`'s next command, or the deliveries an unparked agent has pending, with
    /// a fresh token for the turn it begins, busy from here on so an interrupt
    /// stops it (a selection alone begins none and leaves the agent as it was);
    /// or the session's refusal of new turns, with everything the agent
    /// held. A paused session gives it no deliveries: they stay pending until
    /// `continue` or new input opens it. An `answered` invocation is settled once
    /// nothing keeps it open, however its last work went. Only the owner's
    /// cancellation abandons what this takes, and that ends the loop, dropping
    /// the inbox with it. A child that `parks` lets its loop go once quiescent,
    /// in the change that decides it.
    async fn take_work(&self, id: &AgentId, answered: bool, parks: bool) -> Taken {
        let (id, supervisor) = (id.clone(), self.jobs.supervisor().clone());
        let (keys, decided) = ([Key::Agent(id.clone())], id.clone());
        let decide = move |state: &CoreState| {
            let id = &decided;
            let Some(facts) = state.agents.get(id) else {
                return Some(Taking::Given(Taken::Gone));
            };
            if state.phase().admits(Command::BeginTurn).is_err() {
                return Some(Taking::Refused);
            }
            // A cancelled turn takes no work: what it was sent waits for its loop
            // to end with its owner's cancellation.
            let cancelled = matches!(facts.turn, Turn::Cancelled(_));
            if !cancelled && !facts.inbox.is_empty() {
                return Some(Taking::Command);
            }
            // A parked turn leaves deliveries pending, as a paused session does:
            // the next input's request carries them.
            if facts.turn == Turn::Idle && takes_deliveries(state, id) {
                return Some(Taking::Jobs);
            }
            if answered {
                return (!kept_open(state, id)).then_some(Taking::Given(Taken::Settle));
            }
            (parks && agents::quiescent(state, id)).then_some(Taking::Park)
        };
        let take = move |state: &mut CoreState, taking| {
            fn facts<'a>(
                agents: &'a mut agents::Agents,
                id: &AgentId,
            ) -> &'a mut agents::AgentFacts {
                agents.get_mut(id).expect("decided on the agent's facts")
            }
            let work = match taking {
                Taking::Given(taken) => return taken,
                Taking::Park => {
                    state.agents.remove(&id);
                    return Taken::Parked;
                }
                Taking::Refused => {
                    let phase = state.phase().clone();
                    return Taken::Refused(
                        phase,
                        facts(&mut state.agents, &id).inbox.drain(..).collect(),
                    );
                }
                Taking::Jobs => Work::Jobs,
                Taking::Command => {
                    let input = facts(&mut state.agents, &id).inbox.pop_front();
                    Work::Command(input.expect("decided on a command"))
                }
            };
            let facts = facts(&mut state.agents, &id);
            // Input that begins an invocation an interrupt stopped first runs as a
            // turn interrupted on taking it: its input commits, and nothing requests.
            // The token is cancelled before anyone holds it.
            let begins = matches!(work, Work::Command(AgentCommand::Input { .. }));
            let token = supervisor.token();
            facts.turn_token = token.clone();
            let resting = match work {
                // A selection alone begins no turn: the agent stays as it was, so
                // neither an interrupt nor `continue` mistakes it for one.
                Work::Command(AgentCommand::Select { .. }) => facts.turn.clone(),
                _ if begins && state.jobs.begin(&id) => {
                    token.cancel();
                    facts.set_turn(Turn::Parked(TurnFailure::Interrupted))
                }
                _ => facts.set_turn(Turn::Busy),
            };
            Taken::Work(work, token, resting)
        };
        let take = Wait::taking(keys, decide, take);
        self.store.until(take).await.unwrap_or(Taken::Gone)
    }

    /// Whether `id`'s answered turn goes on for the work waiting at its answer.
    pub(super) async fn rejoin(&self, id: &AgentId) -> bool {
        let id = id.clone();
        let rejoin = move |state: &mut CoreState| {
            let deliveries = takes_deliveries(state, &id);
            state.agents.rejoin(&id, deliveries)
        };
        self.store.live(rejoin).await
    }

    /// Settle `id`'s turn at `turn`, unless an interrupt stopped it first; whether
    /// it did.
    async fn rest(&self, id: &AgentId, turn: Turn) -> bool {
        let id = id.clone();
        let rest = move |state: &mut CoreState| state.agents.rest(&id, turn);
        self.store.live(rest).await
    }

    /// End `id`'s work that ran no turn, as [`Agents::unwind`](agents::Agents::unwind).
    async fn unwind(&self, id: &AgentId, turn: Turn) {
        let id = id.clone();
        self.store
            .live(move |state| state.agents.unwind(&id, turn))
            .await;
    }

    /// End `id`'s turn as `failure` says. A turn an interrupt or closing already
    /// stopped or held ends interrupted, however it ended: its interruption
    /// stands. One whose tree was cancelled ends as it unwound. An interrupted
    /// end is journaled when it leaves a request pending, such as one retrying,
    /// which it closes, follows the `record` of the failure it ended on, or ends
    /// a child's invocation for good. Otherwise a failure is journaled and parks
    /// the turn, unless it leaves nothing to continue, rejecting the queued input
    /// that would start an orphan turn, as the interrupt did for what it found. A child's invocation it ends
    /// resolves, unless its interruption is final. Calls the turn leaves without
    /// a result are closed with error results, then `record` follows, then the
    /// turn's end, all in one commit: a later request never sends the calls
    /// unanswered, and the end right behind `record` tells that it failed the
    /// turn. Reports how the turn ended, and whether its interruption is final.
    async fn end_turn(
        &self,
        id: &AgentId,
        failure: Option<TurnFailure>,
        record: Option<FailureRecord>,
        owner_cancelled: bool,
    ) -> (Option<TurnFailure>, bool) {
        let (id, unrecorded) = (id.clone(), failure.clone());
        let commit = move |state: &CoreState| {
            let stopped = state.agents.get(&id).is_some_and(|facts| {
                matches!(
                    facts.turn,
                    Turn::Held
                        | Turn::Cancelled(Was::Held)
                        | Turn::Stopping
                        | Turn::Parked(TurnFailure::Interrupted)
                )
            });
            let (events, refused) = match record {
                Some(FailureRecord::Journaled(events)) => (Some(events), None),
                Some(FailureRecord::Refused(batch)) => (None, Some(batch)),
                None => (None, None),
            };
            // A refusal has no record of its own: it is the turn's failure, which
            // stands ahead of an interrupt that overtook it.
            let overtaken = match &failure {
                Some(TurnFailure::Failed(failure)) if stopped && refused.is_some() => Some((
                    id.clone(),
                    SessionEvent::AgentFailed {
                        failure: failure.clone(),
                    },
                )),
                _ => None,
            };
            let failure = stopped.then_some(TurnFailure::Interrupted).or(failure);
            let interrupted = failure == Some(TurnFailure::Interrupted);
            let last = interrupted && (owner_cancelled || !stopped);
            let ends = (last && id.parent().is_some())
                || state.requests.open(&id).is_some()
                || events.is_some()
                || refused.is_some();
            let unanswered = state
                .requests
                .unanswered()
                .filter(|(agent, ..)| **agent == id);
            let error = "no result: the turn ended before the call returned one";
            let mut journaled = dispatch::unrun_results(unanswered, error);
            journaled.extend(
                events
                    .into_iter()
                    .flatten()
                    .map(|event| (id.clone(), event)),
            );
            journaled.extend(overtaken);
            journaled.extend(match &failure {
                Some(TurnFailure::Failed(failure)) => {
                    let failure = failure.clone();
                    Some((id.clone(), SessionEvent::AgentFailed { failure }))
                }
                Some(_) if ends => Some((id.clone(), SessionEvent::AgentInterrupted)),
                _ => None,
            });
            let ended = move |state: &mut CoreState, _: &[Arc<EventRecord>]| {
                rest_ended(
                    state,
                    &id,
                    &failure,
                    stopped,
                    refused.flatten().map(|batch| *batch),
                );
                let facts = state.agents.get_mut(&id)?;
                // Explicit job/tree cancellation is deliberately final. A session-turn
                // interrupt is retained as a retryable Interrupted child job.
                if failure.is_some() && !last {
                    facts.invocation = Invocation::Resolved;
                }
                Some((failure, last))
            };
            Ok::<_, SessionError>(Plan::new(journaled, ended))
        };
        let ended = self.store.commit(Command::Journal, commit).await;
        ended.ok().flatten().unwrap_or((unrecorded, true))
    }

    /// Resolve `id`'s invocation unless something keeps it open, in one step with
    /// owner forwarding.
    async fn resolve_invocation(&self, id: &AgentId) -> bool {
        let id = id.clone();
        let resolve = move |state: &mut CoreState| {
            let resolved = !kept_open(state, &id);
            let facts = state.agents.get_mut(&id)?;
            if resolved {
                facts.invocation = Invocation::Resolved;
            }
            Some(resolved)
        };
        self.store.live(resolve).await.unwrap_or(false)
    }

    /// Complete an answered child's invocation once nothing keeps it open: work
    /// that kept it open may have been rejected, withdrawn or consumed elsewhere
    /// without running a turn. Without a waiter nothing finishes the owning job,
    /// so the owner is woken here instead. One that stays open waits idle for that
    /// work, and its reply must not wait for the work to reach the owner.
    async fn settle_answered(
        &self,
        id: &AgentId,
        owner_job: Option<JobId>,
        phase: &mut DriverPhase,
    ) {
        if !phase.answered_open() {
            return;
        }
        if !self.resolve_invocation(id).await {
            self.wake_owner(owner_job).await;
            return;
        }
        if let Some(handoff) = phase.complete() {
            let _ = (self.store)
                .append(id.clone(), SessionEvent::AgentCompleted)
                .await;
            if handoff == Handoff::Consumed {
                self.wake_owner(owner_job).await;
            }
        }
    }

    /// Run `agent_loop` with the window that releases its bursts, until it
    /// parks or ends: neither outlives the other.
    pub(super) async fn run_agent(self: Arc<Self>, agent_loop: AgentLoop) {
        let (store, window) = (self.store.clone(), self.harness.coalescing.clone());
        let coalesce = wait::coalesce(store, agent_loop.id.clone(), window);
        let coalescing = async {
            coalesce.await;
            std::future::pending().await
        };
        let drive: futures_util::future::BoxFuture<'static, ()> = Box::pin(self.drive(agent_loop));
        tokio::select! {
            () = drive => {}
            () = coalescing => {}
        }
    }

    async fn drive(self: Arc<Self>, agent_loop: AgentLoop) {
        let AgentLoop {
            id,
            owner_job,
            mut context,
            location,
            mut settings,
        } = agent_loop;
        let mut phase =
            owner_job.map_or(DriverPhase::Root, |_| DriverPhase::Child(ChildPhase::Idle));
        let child = phase.is_child();
        let owner_cancellation = match owner_job {
            Some(job) => match self.jobs.cancellation_token(job).await {
                Ok(token) => token,
                Err(_) => return self.exited(&id).await,
            },
            None => self.jobs.supervisor().token(),
        };
        loop {
            let taken = tokio::select! {
                biased;
                () = owner_cancellation.cancelled() => {
                    // The cancelled owning job finishes and wakes the owner for
                    // whatever is still pending; with no waiter nothing finishes
                    // it, so a retained reply needs the explicit wake.
                    if !phase.fail(TurnFailure::Cancelled) {
                        self.wake_owner(owner_job).await;
                    }
                    let _ = self.store.append(id.clone(), SessionEvent::AgentInterrupted).await;
                    break;
                }
                taken = self.take_work(&id, phase.answered_open(), child) => taken,
            };
            let (work, cancellation, resting) = match taken {
                Taken::Work(work, cancellation, resting) => (work, cancellation, resting),
                Taken::Settle => {
                    self.settle_answered(&id, owner_job, &mut phase).await;
                    continue;
                }
                // The phase, not the command, decides: whatever the agent holds is
                // refused with it, and a pending reply stays journaled.
                Taken::Refused(refused, held) => {
                    let refused = held
                        .into_iter()
                        .filter_map(|command| command.refuse(|| refused.refusal().into()));
                    refused.for_each(queue::Admitted::hand_back);
                    let _ = (self.store)
                        .append(id.clone(), SessionEvent::AgentInterrupted)
                        .await;
                    break;
                }
                Taken::Parked => return,
                Taken::Gone => break,
            };
            let _ = self.flush_events(&id, &cancellation).await;
            let mut turn = TurnContext {
                agent: &id,
                owner_job,
                cancellation: &cancellation,
                location: &location,
                capabilities: settings.capabilities.clone(),
            };
            let work = match work {
                Work::Command(AgentCommand::QueuedInputs(inputs)) => {
                    let consumed =
                        self.consume_queued(&mut turn, &mut context, &mut settings, inputs);
                    consumed.await.map(|consumed| Begins::Taken(consumed, None))
                }
                // A `continue` runs as a turn from the moment it claims the root,
                // so an interrupt stops it and a second `continue` leaves it alone;
                // one refused rests as it was.
                Work::Command(AgentCommand::Continue {
                    selection,
                    batch,
                    resting,
                    done,
                }) => {
                    let (context, settings) = (&mut context, &mut settings);
                    let continued = self
                        .continue_root(&mut turn, context, settings, selection, batch, *resting);
                    if let Err(error) = continued.await {
                        done.settle(Err(error));
                        continue;
                    }
                    Some(Begins::Taken(Ok(()), Some(done)))
                }
                Work::Command(AgentCommand::Input {
                    content,
                    done,
                    options,
                }) => Some(Begins::Input(content, done, options)),
                Work::Command(AgentCommand::Select { options, done }) => {
                    let selected =
                        self.select(&mut turn, &mut context, &mut settings, options, false);
                    let adopted = match selected.await {
                        Ok(selected) => {
                            let adopting = queue::Adopting::Taken(None);
                            self.adopt(&mut turn, &mut context, &mut settings, selected, adopting)
                                .await
                        }
                        Err(error) => Err(error),
                    };
                    let _ = done.send(adopted);
                    continue;
                }
                Work::Jobs => {
                    let delivered = self.deliver_selected(&mut turn, &mut context, &mut settings);
                    let delivered = delivered.await;
                    delivered.map(|delivered| Begins::Taken(delivered.map_err(Into::into), None))
                }
            };
            let begins = match work {
                Some(begins) => begins,
                // Work that runs no turn may still have been what kept an answered
                // invocation open.
                None if self.rest(&id, resting.clone()).await => {
                    self.settle_answered(&id, owner_job, &mut phase).await;
                    continue;
                }
                // An interrupt stopped the work first: its turn still begins, and
                // ends interrupted as any stopped turn does. Deliveries it refused
                // stay pending.
                None => Begins::Taken(Ok(()), None),
            };
            let (done, delivered) = match begins {
                Begins::Input(content, done, options) => {
                    let images = content.iter().any(UserPart::is_image);
                    let selected =
                        self.select(&mut turn, &mut context, &mut settings, options, images);
                    let selected = match selected.await {
                        Ok(selected) => selected,
                        Err(error) => {
                            // The phase is untouched: a parked child stays parked until
                            // an input with an admissible selection resumes its task.
                            self.unwind(&id, resting).await;
                            if let Some(done) = done {
                                done.settle(Err(error));
                            }
                            continue;
                        }
                    };
                    let done = phase.begin(done);
                    let taken = (!content.is_empty()).then(|| Message::User(content));
                    let adopting = queue::Adopting::Taken(taken);
                    let adopted =
                        self.adopt(&mut turn, &mut context, &mut settings, selected, adopting);
                    (done, adopted.await.map_err(TurnError::from))
                }
                Begins::Taken(delivered, done) => {
                    (phase.begin(done), delivered.map_err(TurnError::refused))
                }
            };
            // Work refused before its request fails the turn it began, as the
            // request's own failure would.
            let result = match delivered {
                Ok(()) => tokio::select! {
                    biased;
                    () = owner_cancellation.cancelled() => Err(HarnessError::Interrupted.into()),
                    result = self.run_turn(turn, &mut context, &mut settings) => result,
                },
                Err(error) => Err(error),
            };
            // A refusal after the answer is the turn's end; the answer stands.
            let (result, record, refused) = match result {
                Ok(recovery::Answered { text, refused }) => {
                    let (refused, record) = refused
                        .map(TurnError::refused)
                        .map_or((None, None), |TurnError { error, record }| {
                            (Some(error), record)
                        });
                    (Ok(text), record, refused)
                }
                Err(TurnError { error, record }) => (Err(error), record, None),
            };
            let failure = (result.as_ref().err())
                .or(refused.as_ref())
                .map(TurnFailure::from);
            // A terminal turn failure is journaled, not only broadcast: the retry
            // affordance is gated on agent activity, which is otherwise lost when
            // the session is reopened.
            let cancelled = owner_cancellation.is_cancelled();
            let (failure, last) = self.end_turn(&id, failure, record, cancelled).await;
            let result = match failure {
                Some(TurnFailure::Interrupted) => Err(HarnessError::Interrupted),
                _ => result,
            };
            if let Ok(answer) = &result {
                phase.answered(answer.clone());
            }
            // Only the root holds its own waiter; a child's lives in its phase.
            if let Some(done) = done {
                done.settle(result);
                continue;
            }
            if child
                && result.is_err()
                && let Some(failure) = failure
            {
                if last {
                    // An interrupted turn can still have published visible text (a
                    // shielded commit outlives the cancelled turn), so the owner is
                    // woken unless the waiter finishes the job for us.
                    if !phase.fail(TurnFailure::Interrupted) {
                        self.wake_owner(owner_job).await;
                    }
                    break;
                }
                // A provider/turn failure is terminal for this invocation, not for
                // the retained child, which its owning job can restart. The failing
                // job wakes the owner for it, and a parked turn without a waiter
                // needs the explicit wake.
                if !phase.fail(failure) {
                    self.wake_owner(owner_job).await;
                }
                continue;
            }
            // A fresh owner request resumes this same child and its history, never
            // a new agent, whether its loop waits idle or parked meanwhile.
            self.settle_answered(&id, owner_job, &mut phase).await;
        }
        self.exited(&id).await;
    }

    /// The loop is gone: only its job, if that can still run it, starts it again.
    async fn exited(&self, id: &AgentId) {
        let id = id.clone();
        // What its inbox held is dropped: those waiting on it see it stopped.
        self.store
            .live(move |state| drop(state.agents.remove(&id)))
            .await;
    }
}

/// Rest `id` as its turn's `failure` leaves it. The batch a refusal failed it
/// on is held, with what was queued behind it, unless an interrupt overtook
/// the refusal: then it is handed back, as the interrupt did what it found.
/// Otherwise a turn that failed, however, parks only on something `continue`
/// can try again, as reopening would read it: the turn the journal reads
/// stopped, or deliveries pending. One its owner cancelled stays parked until
/// it unwinds.
fn rest_ended(
    state: &mut CoreState,
    id: &AgentId,
    failure: &Option<TurnFailure>,
    stopped: bool,
    refused: Option<queue::Batch>,
) {
    let mut held = refused;
    if failure.is_some() && !stopped {
        let behind = state.agents.take_queued(id);
        match &mut held {
            Some(held) => held.join(behind),
            None => (behind.into_iter()).for_each(|input| input.reject(HarnessError::Interrupted)),
        }
    }
    let held = match failure {
        Some(TurnFailure::Failed(failure)) => {
            (held.and_then(queue::Batch::held)).map(|held| (failure.clone(), held))
        }
        _ => {
            if let Some(held) = held {
                held.hand_back();
            }
            None
        }
    };
    let open = state
        .stopped_turn(id)
        .or_else(|| super::agents::held_deliveries(state, id))
        .is_some();
    let parks = match failure {
        Some(TurnFailure::Cancelled) => failure.clone(),
        failure => failure.clone().filter(|_| open),
    };
    let Some(facts) = state.agents.get_mut(id) else {
        return;
    };
    match held {
        // One cancelled after its answer stays cancelled.
        Some((_, held)) if facts.turn == Turn::Cancelled(Was::Idle) => held.hand_back(),
        _ if facts.turn == Turn::Cancelled(Was::Idle) => {}
        Some((failure, held)) => facts.park_holding(failure, held),
        None => drop(facts.set_turn(parks.map_or(Turn::Idle, Turn::Parked))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;
    use crate::job::{JobEnd, JobOutcome, JobSpec, JobState};
    use crate::tool::{ToolOptions, ToolOutput, ToolRegistryBuilder};

    #[tokio::test]
    async fn child_completion_waits_for_background_work_and_returns_its_updated_answer() {
        let final_answer = "child work completed\n".repeat(500);
        let work = json!({"prompt":"work"});
        let script = json!({"source":"return await receive();", "bg":true});
        let (_root, requests, session) = scripted_session([
            response(vec![tool_call(0, "agent", "agent", work)]),
            response(vec![tool_call(0, "script", "script", script)]),
            answer("premature child answer"),
            answer(&final_answer),
            answer("root done"),
        ])
        .await;
        let mut events = session.runtime.store.observe().updates;
        let prompt = session.prompt("delegate");
        tokio::pin!(prompt);
        bounded(async {
            loop {
                tokio::select! {
                    result = &mut prompt => panic!("parent returned before child work completed: {result:?}"),
                    event = events.recv() => if matches!(event.unwrap().event, RuntimeEvent::TurnCompleted { agent } if agent == session.root.child(1)) { break; },
                }
            }
        }).await;
        let jobs = &session.runtime.jobs;
        let find = async |agent: &AgentId, tool: &str| {
            let mut listed = jobs.list(agent).await.unwrap().into_iter();
            listed.find(|job| job.tool == tool).unwrap()
        };
        let agent_job = find(&session.root, "agent").await;
        assert!(!agent_job.state.is_terminal());
        let script = find(&session.root.child(1), "script").await;
        jobs.send(script.id, json!("released")).await.unwrap();
        assert_eq!(bounded(prompt).await.unwrap(), "root done");
        let output = jobs.test_snapshot(agent_job.id).await.unwrap().output;
        assert_eq!(output, Some(json!(final_answer)));
        let requests = requests.lock().unwrap();
        let state = request_runtime_state(&requests[2]);
        let mut rows = state.lines().skip(2);
        let fields = rows.next().unwrap().split(' ').collect::<Vec<_>>();
        assert_eq!(fields.len(), 8, "same-location jobs need no overrides");
        assert_eq!(fields[0], script.id.to_string());
        assert!(rows.next().is_none(), "empty todos are omitted");
        let last = requests.last().unwrap();
        let mut results = last.messages().flat_map(|message| match message {
            Sent::Tool(results) => results.as_slice(),
            _ => &[],
        });
        let result = results.find(|result| result.name == "agent").unwrap();
        assert!(!result.is_error);
        // A foreground call returns its answer in the result and nothing else is
        // delivered: no progress replies, and the answer only once.
        let serialized = rendered(last);
        assert_eq!(serialized.matches("premature child answer").count(), 0);
        let in_result = serde_json::to_string(&result.result).unwrap();
        let answered = in_result.matches("child work completed").count();
        assert!(answered > 0, "{in_result}");
        assert_eq!(serialized.matches("child work completed").count(), answered);
        for request in requests.iter() {
            assert!(
                request.system[0]
                    .text
                    .contains("use `jobs` for more output")
            );
        }
    }

    /// An interrupt that lands once an answered child took what its background work
    /// delivered, before that commits, ends the turn the work began: its
    /// invocation stops interrupted with the delivery pending, and `continue`
    /// resumes it with that delivery.
    #[tokio::test]
    async fn an_interrupt_before_an_answered_childs_delivery_commits_ends_its_invocation() {
        let work = json!({"prompt":"work"});
        let script = json!({"source":"return await receive();", "bg":true});
        let requests = Requests::default();
        let provider = scripted_provider(
            &requests,
            [
                response(vec![tool_call(0, "agent", "agent", work)]),
                response(vec![tool_call(0, "script", "script", script)]),
                answer("premature child answer"),
                answer("child done"),
                answer("root done"),
            ],
        );
        let root = tempfile::tempdir().unwrap();
        // The held window keeps the child between taking its delivery and committing it.
        let (window, gate) = wait::Coalescing::held();
        let builder = test_builder(root.path(), &root.path().join("sessions"), provider, false);
        let session = ephemeral_session(&builder.coalescing(window).build().await.unwrap()).await;
        let child = session.root.child(1);
        let mut events = session.runtime.store.observe().updates;
        let prompt = spawn_prompt(&session, "delegate");
        bounded(async {
            while !matches!(events.recv().await.unwrap().event,
                RuntimeEvent::TurnCompleted { agent } if agent == child)
            {}
        })
        .await;
        let script = job_found(&session, &child, |job| job.tool == "script").await;
        session
            .runtime
            .jobs
            .send(script, json!("released"))
            .await
            .unwrap();
        let taking = child.clone();
        found(&session, [Key::Agent(taking.clone())], move |state| {
            (state.agents.get(&taking)?.turn == Turn::Busy).then_some(())
        })
        .await;
        assert_eq!(session.interrupt().await, 2);
        let agent = job_found(&session, &session.root, |job| job.tool == "agent").await;
        until(&session, agent, |job| job.state == JobState::Interrupted).await;
        assert!(pending(&session, &child));
        // From here on every window releases.
        gate.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
        bounded(session.continue_turn()).await.unwrap();
        assert_eq!(bounded(prompt).await.unwrap().unwrap(), "root done");
        assert!(rendered(&requests.lock().unwrap()[3]).contains("released"));
        session.shutdown().await.unwrap();
    }

    /// A turn is busy from the moment its loop takes the input: an interrupt that
    /// lands before the request stops it, and no request is made.
    #[tokio::test]
    async fn an_interrupt_stops_a_turn_that_took_its_input() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("unrequested")]);
        let other = crate::tests::profile("other", false);
        let harness = serving(root.path(), provider, [("other", other)]);
        let session = ephemeral_session(&harness.build().await.unwrap()).await;
        // Its model change journals while the loop holds the input it took.
        let selection = session.selection(Some(&model_ref("other")), None);
        // Queued behind that append, ahead of the loop's next step.
        let interrupted = interrupt_on(&session, &session.root, |_| true).await;
        let prompt = tokio::spawn({
            let session = session.clone();
            async move {
                session
                    .prompt_with_options("task", &[], selection.unwrap())
                    .await
            }
        });
        assert_eq!(bounded(interrupted).await, 1);
        let prompted = bounded(prompt).await.unwrap();
        assert!(
            matches!(prompted, Err(HarnessError::Interrupted)),
            "{prompted:?}"
        );
        assert!(requests.lock().unwrap().is_empty());
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn session_resume_restarts_all_interrupted_children_without_restarting_waiting_parent() {
        let child = |index: u32| {
            let arguments = json!({"prompt":format!("child task {index}"), "depth":0});
            tool_call(index, &format!("agent-{index}"), "agent", arguments)
        };
        // Both children hang mid-stream in their first request, then answer once resumed.
        let hang = || Step::new(Vec::new()).midstream();
        let recovered = || Step::new(answer("child recovered")).gated();
        let steps = [
            Step::new(response((0..2).map(child).collect())),
            hang(),
            hang(),
            recovered(),
            recovered(),
            Step::new(answer("parent done")),
        ];
        let requests = Requests::default();
        let provider = Script::new(steps, &requests);
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let parent = spawn_prompt(&session, "delegate");
        provider.request(2).await;
        root_waiting(&session).await;
        let original_jobs = session.runtime.jobs.list(&session.root).await.unwrap();
        assert_eq!(original_jobs.len(), 2);
        // The children are interrupted; the parent holding them shows interrupted
        // but keeps its wait.
        assert_eq!(session.interrupt().await, 3);
        assert!(!parent.is_finished());
        assert!(!provider.requested_from(3));
        // Resume immediately, even before the child worker journals Interrupted.
        assert_eq!(turn(&session, &session.root), Turn::Held);
        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
        provider.request(4).await;
        // Resume must leave the original parent wait pending, and shows it waiting
        // again rather than interrupted.
        assert!(!parent.is_finished());
        assert_eq!(turn(&session, &session.root), Turn::Busy);
        assert_eq!(
            session.store().observe().snapshot.activity[&session.root].state,
            crate::agent::AgentActivity::WaitingChildren
        );
        let records = session.runtime.store.records();
        let interrupted = count!(&records, SessionEvent::JobFinished { state, .. } if *state == JobEnd::Interrupted);
        assert_eq!(interrupted, 2);
        // Only an interrupted stream, never an interrupted startup, closes an attempt.
        let children = records.iter().filter(|record| record.agent != session.root);
        assert_eq!(
            count!(children, SessionEvent::ModelAttemptInterrupted(_)),
            2
        );
        let root_records = records.iter().filter(|record| record.agent == session.root);
        assert_eq!(count!(root_records, SessionEvent::ModelRequested { .. }), 1);
        // Continuation must not create replacement agents.
        let owned =
            count!(&records, SessionEvent::AgentStarted { owner_job, .. } if owner_job.is_some());
        assert_eq!(owned, 2);
        let resumed_jobs = session.runtime.jobs.list(&session.root).await.unwrap();
        let resumed = resumed_jobs.iter().map(|job| job.id);
        assert!(resumed.eq(original_jobs.iter().map(|job| job.id)));
        assert!(resumed_jobs.iter().all(|j| j.state == JobState::Running));
        let parent_input = |message: &Sent| {
            matches!(message, Sent::User(content)
            if content.iter().any(|part| matches!(part, SentPart::Text { text } if text.starts_with("Owner input:"))))
        };
        for request in &requests.lock().unwrap()[3..5] {
            let text = serde_json::to_string(&request.context).unwrap();
            assert_eq!(text.matches("child task").count(), 1);
            assert!(!request.messages().any(parent_input));
        }
        provider.release(3);
        provider.release(4);
        assert_eq!(bounded(parent).await.unwrap().unwrap(), "parent done");
        session.shutdown().await.unwrap();
    }

    /// A prompt to a parent holding interrupted children breaks the link: its held
    /// turn ends, the prompt starts the next one, and the children stay retained.
    #[tokio::test]
    async fn prompt_redirects_a_parent_holding_interrupted_children() {
        let after = [
            Step::new(answer("redirected")),
            Step::new(answer("child recovered")).gated(),
            Step::new(answer("parent done")),
        ];
        let root = tempfile::tempdir().unwrap();
        let (_harness, session, provider, requests, parent, _) =
            held_on_interrupted_child(root.path(), after).await;
        assert!(!parent.is_finished());
        assert_eq!(
            bounded(session.prompt("new direction")).await.unwrap(),
            "redirected"
        );
        assert!(bounded(parent).await.unwrap().is_err());
        let jobs = session.runtime.jobs.list(&session.root).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, JobState::Interrupted);
        // The redirected request carries the interrupted result and the prompt.
        let history = serde_json::to_string(&requests.lock().unwrap()[2].context).unwrap();
        assert!(
            history.contains("interrupted") && history.contains("new direction"),
            "{history}"
        );
        // The retained child still continues, and its answer reaches the root as
        // an event rather than through the ended wait.
        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
        provider.request(3).await;
        provider.release(3);
        let woken = provider.request(4).await;
        assert!(rendered(&woken).contains("child recovered"));
        assert_eq!(
            count!(
                &session.runtime.store.records(),
                SessionEvent::AgentStarted { .. }
            ),
            2
        );
        session.shutdown().await.unwrap();
    }

    /// Queued input joins the held turn instead of ending it: the parent's next
    /// request carries the child's interrupted result and the instruction together.
    #[tokio::test]
    async fn queued_input_redirects_a_parent_holding_an_interrupted_child() {
        let root = tempfile::tempdir().unwrap();
        let (_harness, session, _, requests, parent, _) =
            held_on_interrupted_child(root.path(), [Step::new(answer("redirected"))]).await;
        let prompt = QueuedPrompt {
            text: "use the right user".into(),
            ..Default::default()
        };
        let receipts = bounded(enqueue_prompts(&session, vec![prompt])).await;
        assert!(receipts[0].is_ok());
        assert_eq!(bounded(parent).await.unwrap().unwrap(), "redirected");
        let history = serde_json::to_string(&requests.lock().unwrap()[2].context).unwrap();
        assert!(
            history.contains("interrupted") && history.contains("use the right user"),
            "{history}"
        );
        assert_eq!(
            session.runtime.jobs.list(&session.root).await.unwrap()[0].state,
            JobState::Interrupted
        );
        session.shutdown().await.unwrap();
    }

    /// A child whose turn failed parks as a finished one does, with no activity yet
    /// still shown as something to continue; `continue` starts it again stopped on that failure,
    /// and it asks again for what failed, on its own context and history.
    #[tokio::test]
    async fn a_failed_child_parks_and_continue_asks_again() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let arguments = json!({"prompt": "child task", "depth": 0});
        let failure = crate::provider::ProviderErrorKind::Protocol.error("not retried");
        let steps = [
            Step::new(response(vec![tool_call(0, "child", "agent", arguments)])),
            Step::fail(failure),
            Step::new(answer("root done")),
            Step::new(answer("child recovered")),
            Step::new(answer("root again")),
        ];
        let provider = Script::new(steps, &requests);
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = ephemeral_session(&harness).await;
        let child = session.root.child(1);
        let mut observed = session.store().observe();
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");
        until_parked(&session, &child).await;
        // Parked, it has no activity, as its observers are told.
        while let Ok(update) = observed.updates.try_recv() {
            observed.snapshot.apply(update);
        }
        assert!(!observed.snapshot.activity.contains_key(&child));
        let jobs = session.runtime.jobs.list(&session.root).await.unwrap();
        assert!(
            jobs.iter()
                .any(|job| job.tool == "agent" && job.retryable())
        );
        let continued = bounded(session.continue_turn_with(Selection::default())).await;
        assert_eq!(continued.unwrap().children_resumed, 1);
        provider.request(4).await;
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured[3].context_id, ContextId::from(&child));
        assert_eq!(captured[1], captured[3]);
        until_parked(&session, &child).await;
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_child_send_preserves_the_parents_pending_wait() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let gate = release.clone();
        let wait_fixture = move |_, _| {
            let gate = gate.clone();
            async move {
                gate.acquire().await.unwrap().forget();
                Ok(ToolOutput::new(json!("released")))
            }
        };
        let mut tools = ToolRegistryBuilder::default();
        let (description, options) = ("Wait for the test gate", ToolOptions::default());
        let registered = tools
            .register_product::<crate::tool::registry::Arguments, serde_json::Value, _, _>(
                "wait_fixture",
                description,
                options,
                wait_fixture,
            );
        registered.unwrap();
        let wait = response(vec![tool_call(0, "wait", "wait_fixture", json!({}))]);
        // The child's first invocation fails outright.
        let kind = crate::provider::ProviderErrorKind::Authentication;
        let failed = Step::fail(kind.error("fixture permanent failure"));
        let answers = ["child recovered", "parent done", "parent done"];
        let answers = answers.map(|text| Step::new(answer(text)));
        let steps = [Step::new(wait), failed].into_iter().chain(answers);
        let provider = Script::new(steps, &requests);
        let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
            .tools(tools.build())
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let jobs = &session.runtime.jobs;
        let parent = spawn_prompt(&session, "wait for input");
        let running = |job: &crate::job::JobEnvelope| {
            job.tool == "wait_fixture" && job.state == JobState::Running
        };
        let wait_job = job_found(&session, &session.root, running).await;
        let arguments = json!({"prompt":"retain my task", "bg":true});
        let (executor, agent) = (&session.runtime.executor, session.root.clone());
        let job = executor
            .execute(agent, "agent", arguments, None)
            .await
            .unwrap()
            .job;
        let wait = async || bounded(jobs.wait(job, true)).await.unwrap();
        let failed = wait().await;
        assert_eq!(failed.state, JobState::Failed);
        let child = session.root.child(1);
        assert_eq!(requests.lock().unwrap().len(), 2, "no automatic replay");
        let retry = format!("return (await tool.job({job}).send({{value:'try again'}})).unwrap();");
        let resumed = session.run_script(retry).await.unwrap();
        assert_eq!(resumed.value["value"], serde_json::Value::Null);
        let recovered = wait().await;
        assert_eq!(recovered.state, JobState::Completed);
        assert_eq!(recovered.output, Some(json!("child recovered")));
        // A child retry must not complete the parent's wait.
        assert!(!parent.is_finished());
        let state = jobs.snapshot(wait_job).await.unwrap().state;
        assert_eq!(state, JobState::Running);
        let records = session.runtime.store.records();
        let root_records = records.iter().filter(|record| record.agent == session.root);
        // No new parent request before its pending wait is released.
        assert_eq!(count!(root_records, SessionEvent::ModelRequested { .. }), 1);
        let child_records = records.iter().filter(|record| record.agent == child);
        assert_eq!(count!(child_records, SessionEvent::AgentStarted { .. }), 1);
        let history = serde_json::to_string(&requests.lock().unwrap()[2].context).unwrap();
        assert_eq!(history.matches("retain my task").count(), 1);
        assert_eq!(history.matches("try again").count(), 1);
        release.add_permits(1);
        assert_eq!(bounded(parent).await.unwrap().unwrap(), "parent done");
        session.shutdown().await.unwrap();
    }

    /// Between follow-ups the completed child parks; each starts its loop again
    /// on the same context and history, under the same job.
    #[tokio::test]
    async fn completed_child_resumes_same_history_and_job_repeatedly() {
        let answers = ["first answer", "second answer", "third answer"].map(answer);
        let (_root, requests, session) = scripted_session(answers).await;
        let root_inbox = quiet_root(&session);
        let jobs = &session.runtime.jobs;
        let arguments = json!({"prompt":"remember the initial task", "depth":0});
        let (executor, agent) = (&session.runtime.executor, session.root.clone());
        let first = executor
            .execute(agent, "agent", arguments, None)
            .await
            .unwrap();
        assert_eq!(first.output.value, "first answer");
        let job = first.job;
        let child = session.root.child(1);
        let follow_ups = [
            ("follow-up one", "second answer"),
            ("follow-up two", "third answer"),
        ];
        for (instruction, answer) in follow_ups {
            until_parked(&session, &child).await;
            let value = json!(instruction);
            let send = format!("return tool.job({job}).send({{value:{value}}});");
            session.run_script(send).await.unwrap();
            jobs.wait(job, true).await.unwrap();
            let output = format!("return tool.jobs({{job:{job}}});");
            let sent = session.run_script(output).await.unwrap();
            assert_eq!(sent.value["value"]["id"], job.get());
            assert_eq!(sent.value["value"]["state"], "completed");
            assert_eq!(sent.value["value"]["result"], answer);
            assert_eq!(jobs.metadata(job).await.unwrap().state, JobState::Completed);
        }
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            let child_id = ContextId::from(&child);
            assert!(
                requests
                    .iter()
                    .all(|request| request.context_id == child_id)
            );
            let text = serde_json::to_string(&requests[2].context).unwrap();
            let expected = ["remember the initial task", "first answer", "follow-up one"];
            for expected in expected
                .into_iter()
                .chain(["second answer", "follow-up two"])
            {
                assert!(text.contains(expected), "missing {expected}: {text}");
            }
            assert!(
                matches!(requests[2].messages().last(), Some(Sent::User(content)) if matches!(&content[0], SentPart::Text {text} if text.contains("follow-up two")))
            );
        }
        let records = session.runtime.store.records();
        let started =
            count!(&records, SessionEvent::AgentStarted { owner_job: Some(id), .. } if *id == job);
        assert_eq!(started, 1);
        let restored = session.runtime.jobs.test_replay();
        let output = restored.test_snapshot(job).await.unwrap().output;
        assert_eq!(output, Some(json!("third answer")));
        // Shutdown waits for agent receivers to close: restore the real root loop.
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }

    /// Input to a parked child starts its loop again on a context of its own,
    /// under its job: cancelling the job ends the woken turn, and the child, whose
    /// job can no longer run it, is never started again.
    #[tokio::test]
    async fn a_woken_child_ends_with_its_cancelled_job() {
        let steps = [
            Step::new(answer("first")),
            Step::new(Vec::new()).midstream(),
        ];
        let requests = Requests::default();
        let provider = Script::new(steps, &requests);
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = ephemeral_session(&harness).await;
        let root_inbox = quiet_root(&session);
        let (jobs, child) = (&session.runtime.jobs, session.root.child(1));
        let arguments = json!({"prompt": "work", "depth": 0});
        let executor = &session.runtime.executor;
        let first = executor.execute(session.root.clone(), "agent", arguments, None);
        let job = first.await.unwrap().job;
        until_parked(&session, &child).await;
        provider.released(&ContextId::from(&child)).await;
        let opened = provider.opened.load(std::sync::atomic::Ordering::SeqCst);
        jobs.send(job, json!("again")).await.unwrap();
        provider.held(1).await;
        let reopened = provider.opened.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(reopened, opened + 1);
        let woken = requests.lock().unwrap().last().unwrap().clone();
        assert_eq!(woken.context_id, ContextId::from(&child));
        assert!(rendered(&woken).contains("first"));
        jobs.cancel(job).await.unwrap();
        assert_eq!(terminal(&session, job).await.state, JobState::Cancelled);
        until_stopped(&session, &child).await;
        let refused = jobs.send(job, json!("once more")).await;
        assert!(matches!(
            refused,
            Err(crate::job::JobError::InputUnavailable {
                reason: crate::job::InputUnavailableReason::State(JobState::Cancelled),
                ..
            })
        ));
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }

    /// A cancellation requested of a parked child's job right behind the restart
    /// input commits ends the child cancelled before it requests again.
    #[tokio::test]
    async fn a_cancel_requested_of_a_parked_childs_job_stops_it() {
        let (_root, requests, session) = scripted_session([answer("first")]).await;
        let root_inbox = quiet_root(&session);
        let (jobs, executor) = (&session.runtime.jobs, &session.runtime.executor);
        let arguments = json!({"prompt": "work", "depth": 0});
        let launched = executor.execute(session.root.clone(), "agent", arguments, None);
        let restarted = launched.await.unwrap().job;
        until_parked(&session, &session.root.child(1)).await;
        let restart: fn(&SessionEvent) -> bool =
            |event| matches!(event, SessionEvent::JobStateChanged { .. });
        let cancelling = jobs.clone();
        let store = &session.runtime.store;
        let cancelled =
            crate::session::tests::on_commit(store, &session.root, restart, move || async move {
                cancelling.cancel(restarted).await.map(drop)
            });
        let cancelled = cancelled.await;
        jobs.send(restarted, json!("again")).await.unwrap();
        bounded(cancelled).await.unwrap();
        assert_eq!(
            terminal(&session, restarted).await.state,
            JobState::Cancelled
        );
        until_stopped(&session, &session.root.child(1)).await;
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "a cancelled child requested"
        );
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }

    /// A failed child whose own work still runs keeps its loop; cancelling its
    /// job, which records the cancel, ends that loop before it takes another turn.
    #[tokio::test]
    async fn a_cancel_of_a_failed_childs_job_ends_its_running_loop() {
        let script = json!({"source":"return await receive();", "bg":true});
        let failure = crate::provider::ProviderErrorKind::Protocol.error("not retried");
        let steps = [
            Step::new(response(vec![tool_call(0, "script", "script", script)])),
            Step::fail(failure),
            Step::new(answer("unrequested")),
        ];
        let requests = Requests::default();
        let provider = Script::new(steps, &requests);
        let root = tempfile::tempdir().unwrap();
        let harness = test_harness(root.path(), &root.path().join("sessions"), provider).await;
        let session = ephemeral_session(&harness).await;
        let root_inbox = quiet_root(&session);
        let (jobs, child) = (&session.runtime.jobs, session.root.child(1));
        let arguments = json!({"prompt": "work", "bg": true});
        let executor = &session.runtime.executor;
        let launched = executor.execute(session.root.clone(), "agent", arguments, None);
        let job = launched.await.unwrap().job;
        until(&session, job, |job| job.state == JobState::Failed).await;
        job_found(&session, &child, |job| job.tool == "script").await;
        let read = |state: &CoreState| agents::child_loop(state, &child);
        assert_eq!(session.runtime.store.read(read), agents::ChildLoop::Running);
        jobs.cancel(job).await.unwrap();
        until_stopped(&session, &child).await;
        let records = session.runtime.store.records();
        let interrupted = records.iter().filter(|record| record.agent == child);
        assert_eq!(count!(interrupted, SessionEvent::AgentInterrupted), 1);
        assert_eq!(
            requests.lock().unwrap().len(),
            2,
            "a cancelled child requested"
        );
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }

    /// Input racing a completed child's parking, and racing itself, runs one loop
    /// at a time: each input either starts it or joins the invocation another
    /// started, all on one context and history, and the child parks once nothing
    /// is left.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn inputs_racing_a_childs_parking_run_one_loop() {
        let answers = ["first", "second", "third", "fourth"].map(answer);
        let (_root, requests, session) = scripted_session(answers).await;
        let root_inbox = quiet_root(&session);
        let (jobs, child) = (&session.runtime.jobs, session.root.child(1));
        let arguments = json!({"prompt": "work", "depth": 0});
        let executor = &session.runtime.executor;
        let first = executor.execute(session.root.clone(), "agent", arguments, None);
        let job = first.await.unwrap().job;
        let (a, b) = tokio::join!(
            jobs.send(job, json!("follow-up a")),
            jobs.send(job, json!("follow-up b"))
        );
        a.unwrap();
        b.unwrap();
        // Both were taken, so the job completes only once it answered both.
        until(&session, job, |job| job.state == JobState::Completed).await;
        until_parked(&session, &child).await;
        let requests = requests.lock().unwrap().clone();
        let context = ContextId::from(&child);
        assert!(requests.iter().all(|request| request.context_id == context));
        let last = rendered(requests.last().unwrap());
        assert!(
            last.contains("follow-up a") && last.contains("follow-up b"),
            "{last}"
        );
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }

    /// A background child's answer, released early while it still held work, is
    /// its result once: completion withdraws the undelivered reply, as replay does.
    #[tokio::test]
    async fn child_finishes_once_its_last_work_ends() {
        for background in [false, true] {
            let (_root, _requests, session) = scripted_session([answer("initial")]).await;
            let root_inbox = quiet_root(&session);
            let (owner, child, job) = retained_child(&session, background).await;
            let (done, received) = oneshot::channel();
            let mut events = session.runtime.store.observe().updates;
            child_input(&session, &child, Some(done)).await;
            wait_for_child_answer(&mut events, &child).await;
            let jobs = &session.runtime.jobs;
            if background {
                // The loop tail releases the answer: the child still holds work.
                until_pending(&session, &session.root).await;
            }
            claim_completed(&session, job).await;
            let answer = bounded(received).await.unwrap().unwrap();
            assert_eq!(answer, "initial");
            let result = JobOutcome::Completed(ToolOutput::new(answer.into()));
            jobs.finish(owner, result).await.unwrap();
            let delivery = jobs.pending_delivery(&session.root).await.unwrap();
            assert!(delivery.messages().is_empty());
            drop((delivery, root_inbox));
            session.shutdown().await.unwrap();
        }
    }

    /// A retained child with an owner job, plus one job of its own that keeps an
    /// invocation it answered open until it ends.
    async fn retained_child(session: &SessionHandle, background: bool) -> (JobId, AgentId, JobId) {
        let child = session.root.child(1);
        let owner = owner(session, background).await;
        let launch = child_launch(session, None);
        let spawned = (session.runtime).spawn_agent(child.clone(), Some(owner), launch);
        spawned.await.unwrap();
        let spec = JobSpec::test(child.clone(), "manual");
        let job = session.runtime.jobs.test_create(spec).await;
        (owner, child, job)
    }

    async fn claim_completed(session: &SessionHandle, job: JobId) {
        let outcome = JobOutcome::Completed(ToolOutput::default());
        session.runtime.jobs.finish(job, outcome).await.unwrap();
        session.runtime.jobs.test_claim(job).await.unwrap();
    }

    // A rejected selection (a mode, which only the root can enter) acknowledges all
    // earlier inbox commands without invoking a provider, replacing a child
    // waiter or starting another turn.
    async fn mailbox_barrier(session: &SessionHandle, child: &AgentId) {
        let (done, received) = oneshot::channel();
        let mode = SessionMode {
            runtime: session.runtime.instance,
            name: "driver-test-mode".parse().unwrap(),
        };
        let selection = Selection {
            mode: Some(mode),
            ..Default::default()
        };
        send(session, child, input("", selection, Some(done))).await;
        assert!(bounded(received).await.unwrap().is_err());
    }

    async fn wait_for_child_answer(
        events: &mut tokio::sync::broadcast::Receiver<crate::agent::ObservedEvent>,
        child: &AgentId,
    ) {
        bounded(async {
            loop {
                if matches!(events.recv().await.unwrap().event, RuntimeEvent::TurnCompleted { agent, .. } if &agent == child) {
                    break;
                }
            }
        }).await;
    }

    fn completion_count(session: &SessionHandle, child: &AgentId) -> usize {
        let records = session.runtime.store.records();
        let child_records = records.iter().filter(|record| &record.agent == child);
        count!(child_records, SessionEvent::AgentCompleted)
    }

    /// Rejected input does not keep an answered invocation open: once the job that
    /// did ends, the invocation completes with its latest answer, without another
    /// turn. The completed child parks, so a later interrupt leaves it alone, and
    /// it resumes with a fresh completion rather than the consumed one.
    #[tokio::test]
    async fn rejected_input_leaves_the_latest_child_answer_to_complete() {
        let steps = [
            answer("old answer"),
            answer("latest answer"),
            answer("resumed"),
        ];
        let (_root, requests, session) = scripted_session(steps).await;
        let root_inbox = quiet_root(&session);
        let (_, child, work) = retained_child(&session, false).await;
        let (done, received) = oneshot::channel();
        let mut events = session.runtime.store.observe().updates;
        child_input(&session, &child, Some(done)).await;
        wait_for_child_answer(&mut events, &child).await;
        mailbox_barrier(&session, &child).await;
        assert_eq!(completion_count(&session, &child), 0);
        child_input(&session, &child, None).await;
        wait_for_child_answer(&mut events, &child).await;
        // Input its sender already withdrew is rejected.
        let cancellation = QueuedPromptCancellation::default();
        assert!(cancellation.cancel());
        let (committed, rejected) = oneshot::channel();
        let text = "cancelled update".to_owned();
        let queued = queue::QueuedInput {
            content: vec![UserPart::Text { text }],
            attachments: Vec::new(),
            options: Default::default(),
            cancellation,
            committed,
        };
        send(&session, &child, AgentCommand::QueuedInputs(vec![queued])).await;
        assert!(bounded(rejected).await.unwrap().is_err());
        mailbox_barrier(&session, &child).await;
        assert_eq!(completion_count(&session, &child), 0);
        claim_completed(&session, work).await;
        assert_eq!(bounded(received).await.unwrap().unwrap(), "latest answer");
        // It parks once its completion is journaled.
        until_parked(&session, &child).await;
        assert_eq!(completion_count(&session, &child), 1);
        assert_eq!(session.interrupt().await, 0);
        // Rejected input must not run another model turn.
        assert_eq!(requests.lock().unwrap().len(), 2);
        // Parked stays resumable: input without a waiter completes afresh, once.
        unpark(&session, &child, input("task", Selection::default(), None)).await;
        wait_for_child_answer(&mut events, &child).await;
        until_parked(&session, &child).await;
        assert_eq!(completion_count(&session, &child), 2);
        assert_eq!(requests.lock().unwrap().len(), 3);
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }

    /// A host call answers no model call, so a redirect lets go of its retained
    /// child through the release record alone: the child goes on in the
    /// background, live and on replay, and its later completion wakes the root.
    #[tokio::test]
    async fn redirect_releases_a_host_called_child_to_deliver_its_completion() {
        let steps = [
            Step::new(Vec::new()).midstream(),
            Step::new(answer("redirected")),
            Step::new(answer("child recovered")).gated(),
        ];
        let requests = Requests::default();
        let provider = Script::new(steps, &requests);
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let (executor, owner) = (session.runtime.executor.clone(), session.root.clone());
        let arguments = json!({"prompt":"child task", "depth":0});
        let host =
            tokio::spawn(async move { executor.execute(owner, "agent", arguments, None).await });
        provider.held(0).await;
        let jobs = &session.runtime.jobs;
        let child = jobs.list(&session.root).await.unwrap()[0].id;
        assert!(jobs.metadata(child).await.unwrap().origin.is_none());
        assert_eq!(session.interrupt().await, 1);
        assert_eq!(
            bounded(session.prompt("new direction")).await.unwrap(),
            "redirected"
        );
        assert!(bounded(host).await.unwrap().is_err());
        assert_eq!(
            jobs.snapshot(child).await.unwrap().state,
            JobState::Interrupted
        );
        let replayed = jobs.test_replay();
        assert!(replayed.is_background(child).await.unwrap());

        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
        provider.request(2).await;
        let root_inbox = quiet_root(&session);
        provider.release(2);
        bounded(jobs.wait_settled(child)).await.unwrap();
        let completed = jobs.test_snapshot(child).await.unwrap();
        assert_eq!(
            (completed.state, completed.output),
            (JobState::Completed, Some(json!("child recovered")))
        );
        assert!(pending(&session, &session.root));
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }

    /// A turn that reaches its request once closing stopped it ends interrupted,
    /// as closing left it, not failed by the refusal.
    #[tokio::test]
    async fn a_turn_closing_stopped_before_its_request_ends_interrupted() {
        let (_root, _requests, session) = scripted_session([answer("never")]).await;
        let store = &session.runtime.store;
        let context = |event: &SessionEvent| matches!(event, SessionEvent::ModelContext { .. });
        // Closing commits ahead of the request the turn makes once its context publishes.
        let closing = session.clone();
        let shutdown = on_commit(store, &session.root, context, move || async move {
            closing.shutdown().await
        });
        let shutdown = shutdown.await;
        let prompt = spawn_prompt(&session, "hello");
        let (prompted, shut) = bounded(async { tokio::join!(prompt, shutdown) }).await;
        assert!(matches!(prompted.unwrap(), Err(HarnessError::Interrupted)));
        shut.unwrap();
        assert_eq!(
            count!(&store.records(), SessionEvent::AgentFailed { .. }),
            0
        );
        let stopped = store.read(|state| state.stopped_turn(&session.root));
        assert_eq!(stopped, Some(TurnFailure::Interrupted));
        session.close().await.unwrap();
    }

    /// A turn's final model failure commits with the turn's end, so an interrupt
    /// lands wholly before or after both. Landing after, it finds the turn failed,
    /// and the request's failure is the one report of it. Landing first, once the
    /// refusal is in, the turn ends interrupted right behind the failure's record.
    /// Either way the next turn, failing before any request, reports its own
    /// failure, live and once reopened.
    #[tokio::test]
    async fn a_final_failure_commits_with_its_turns_end_either_side_of_an_interrupt() {
        use crate::provider::protocol::CutReason;
        use crate::session::{Fact, RequestPhase, tests::Folded};
        // Whether a request's failure reports each failed turn of the root.
        let reports = |ledger: &Folded, records: &[Arc<EventRecord>]| {
            let failed = records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::AgentFailed { .. }));
            let reported = |record: &Arc<EventRecord>| ledger.facts.get(&record.sequence);
            failed
                .map(|record| matches!(reported(record), Some(Fact::EndedOn)))
                .collect::<Vec<_>>()
        };
        for overtaken in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let sessions = root.path().join("sessions");
            let requests = Requests::default();
            let refused = Step::new(cut(Vec::new(), CutReason::Refusal)).gated();
            let provider = Script::new([refused], &requests);
            let harness = test_harness(root.path(), &sessions, provider.clone()).await;
            let session = harness.new_session().await.unwrap();
            let (store, agent) = (&session.runtime.store, session.root.clone());
            let mut updates = session.store().observe().updates;
            let prompt = spawn_prompt(&session, "refused");
            provider.request(0).await;
            // An image the root's model cannot take fails its next turn before
            // that turn's request.
            let image = crate::media::Attachment::Image {
                file: None,
                image: crate::tests::png(b"").await,
            };
            let attachment = store.store_attachment(&image).await.unwrap();
            let image = Message::User(vec![UserPart::Attachment { attachment }]);
            let seeded = session.runtime.commit(&agent, image);
            if overtaken {
                // The core stalls on the seed's commit, with the interrupt queued
                // behind it, and the turn's end behind that once the refusal is in.
                let held = crate::session::tests::hold_connection(store);
                tokio::pin!(seeded);
                assert!(futures_util::poll!(seeded.as_mut()).is_pending());
                let interrupt = session.interrupt();
                tokio::pin!(interrupt);
                assert!(futures_util::poll!(interrupt.as_mut()).is_pending());
                provider.release(0);
                bounded(async {
                    while !matches!(
                        updates.recv().await.unwrap().event,
                        RuntimeEvent::ResponseSettled {
                            settlement: Settlement::Failed(_),
                            ..
                        }
                    ) {}
                })
                .await;
                drop(held);
                bounded(seeded).await.unwrap();
                assert_eq!(bounded(interrupt).await, 1);
            } else {
                seeded.await.unwrap();
                provider.release(0);
            }
            let refusal = bounded(prompt).await.unwrap();
            match overtaken {
                true => assert!(matches!(refusal, Err(HarnessError::Interrupted))),
                false => {
                    assert!(matches!(refusal, Err(HarnessError::Refused(_))));
                    assert_eq!(session.interrupt().await, 0);
                }
            }
            let records = store.records();
            let failed = (records.iter())
                .position(|record| matches!(record.event, SessionEvent::ModelFailed { .. }))
                .unwrap();
            let SessionEvent::ModelFailed { attempt, .. } = &records[failed].event else {
                unreachable!()
            };
            let request = attempt.request;
            match overtaken {
                true => assert_eq!(records[failed + 1].event, SessionEvent::AgentInterrupted),
                false => assert!(matches!(
                    records[failed + 1].event,
                    SessionEvent::AgentFailed { .. }
                )),
            }
            let next = bounded(session.prompt("again")).await;
            assert!(matches!(next, Err(HarnessError::ImagesUnsupported(_))));
            let expected = match overtaken {
                true => vec![false],
                false => vec![true, false],
            };
            let ledger = crate::session::tests::ledger(store);
            assert_eq!(reports(&ledger, &store.records()), expected);
            assert!(matches!(
                ledger.requests[&request].phase,
                RequestPhase::Failed { .. }
            ));
            assert_eq!(requests.lock().unwrap().len(), 1);
            let id = session.id();
            shutdown_session(session).await;

            let resumed = harness.resume_session(id).await.unwrap();
            let ledger = crate::session::tests::ledger(&resumed.runtime.store);
            let records = resumed.runtime.store.records();
            assert_eq!(reports(&ledger, &records), expected, "{overtaken}");
            shutdown_session(resumed).await;
        }
    }

    /// A refusal is its turn's failure. It holds the refused batch, with input
    /// that reached the inbox as it ended the turn, until a change of turn hands
    /// them back; overtaken by an interrupt, it is journaled ahead of the
    /// interrupt and its batch handed back, and the turn, with nothing left to
    /// continue, rests as reopening would read it.
    #[tokio::test]
    async fn a_refusal_holds_its_batch_and_stands_ahead_of_an_interrupt() {
        let (_root, _requests, session) = scripted_session([]).await;
        let (runtime, root) = (session.runtime.clone(), session.root.clone());
        let input = |text: &str| {
            let (committed, receipt) = oneshot::channel();
            let input = queue::QueuedInput {
                content: vec![UserPart::Text { text: text.into() }],
                attachments: Vec::new(),
                options: Selection::default(),
                cancellation: Default::default(),
                committed,
            };
            (input, receipt)
        };
        let batch = |input| {
            let mut batch = queue::Batch::default();
            batch.join([input]);
            batch
        };
        let failure = crate::agent::Failure::Other("refused".into());
        let failed = Some(TurnFailure::Failed(failure.clone()));
        let ((refused, refused_receipt), (behind, behind_receipt)) =
            (input("refused"), input("behind"));
        let behind_held = behind.cancellation.clone();
        let rest = {
            let (root, failed, refused) = (root.clone(), failed.clone(), batch(refused));
            move |state: &mut CoreState| {
                let facts = state.agents.get_mut(&root).unwrap();
                facts.set_turn(Turn::Busy);
                state
                    .agents
                    .send(&root, AgentCommand::QueuedInputs(vec![behind]));
                rest_ended(state, &root, &failed, false, Some(refused));
            }
        };
        runtime.store.live(rest).await;
        assert_eq!(turn(&session, &root), Turn::Parked(failed.clone().unwrap()));
        assert!(behind_held.is_retained());
        set_root_turn(&session, Turn::Parked(TurnFailure::Interrupted));
        for receipt in [refused_receipt, behind_receipt] {
            let receipt = bounded(receipt).await.unwrap();
            assert!(matches!(receipt, Err(HarnessError::Interrupted)));
        }
        let (overtaken, receipt) = input("overtaken");
        let record = Some(FailureRecord::Refused(Some(Box::new(batch(overtaken)))));
        let ended = runtime.end_turn(&root, failed, record, false).await;
        assert_eq!(ended.0, Some(TurnFailure::Interrupted));
        let records = runtime.store.records();
        let ending: Vec<_> = records[records.len() - 2..]
            .iter()
            .map(|r| &r.event)
            .collect();
        assert_eq!(
            ending,
            [
                &SessionEvent::AgentFailed { failure },
                &SessionEvent::AgentInterrupted
            ]
        );
        let receipt = bounded(receipt).await.unwrap();
        assert!(matches!(receipt, Err(HarnessError::Interrupted)));
        assert_eq!(turn(&session, &root), Turn::Idle);
        let read = runtime.store.read(|state| state.stopped_turn(&root));
        assert_eq!(read, None);
        let outcome = bounded(session.continue_turn_with(Selection::default())).await;
        assert_eq!(outcome.unwrap().answer, None);
        session.shutdown().await.unwrap();
    }
}
