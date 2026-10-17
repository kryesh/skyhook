//! Summarizer response assembly and cancellation-safe usage accounting.

use super::{HarnessError, SessionRuntime, TurnContext, continuation};
use crate::agent::CompactionError;
use crate::provider::{
    ProviderContext,
    protocol::{Completion, CutReason, ModelRequest, Outcome, visible_text},
};
use crate::session::{AttemptRef, RequestSeq, SessionEvent};

impl SessionRuntime {
    /// The summary for the frozen `request`, with the attempt that produced it.
    /// Transient failures do not consume the separate validation budget in
    /// `compact_history`.
    pub(super) async fn summarize(
        &self,
        turn: &TurnContext<'_>,
        provider: &mut dyn ProviderContext,
        request: &ModelRequest,
        sequence: RequestSeq,
        model_attempt: &mut u64,
    ) -> Result<(AttemptRef, continuation::Continuation), HarnessError> {
        let attempted = self.attempt(turn, provider, request, sequence, model_attempt, |_| {});
        let (attempt, completion, usage) = match attempted.await? {
            Ok(response) => response,
            // The round's own failure, which may end the turn, follows it.
            Err((error, record)) => {
                self.record_failure(turn.agent, record).await?;
                return Err(error.into());
            }
        };
        if let Some(usage) = SessionEvent::usage(sequence, usage) {
            self.store.append(turn.agent.clone(), usage).await?;
        }
        Ok((attempt, continuation(&completion)?))
    }
}

/// Only a cut-off summary is a truncated one, which a fresh summary may complete. A
/// refusal is deterministic for the request, so it fails the round as what it is.
fn continuation(completion: &Completion) -> Result<continuation::Continuation, HarnessError> {
    let text = visible_text(completion.items());
    match completion.outcome() {
        Outcome::Answer => {}
        Outcome::Cut(CutReason::Refusal) => {
            return Err(HarnessError::Refused(super::super::turn::refusal_detail(
                &text,
            )));
        }
        Outcome::Cut(CutReason::MaxTokens | CutReason::Incomplete) => {
            return Err(CompactionError::Truncated.into());
        }
        Outcome::ToolUse => return Err(CompactionError::ToolCall.into()),
    }
    Ok(continuation::continuation(&text)?)
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use crate::agent::runtime::tests::{elapse_backoff, journaled_usage, next_recovery, retrying};
    use crate::{
        agent::TodoStatus,
        agent::runtime::HarnessError,
        session::{Message, RequestPhase, SessionEvent, project_history},
    };

    /// A summary that overflows the context window is retried smaller: with half the
    /// output reserve, then also without the older half of its history, which its
    /// request journals so reconstruction sends what was sent. Dropping history never
    /// raises the reserve again, even when the reported occupancy had constrained it.
    #[tokio::test]
    async fn an_overflowing_summary_is_retried_with_half_reserve_then_half_history() {
        use crate::agent::CompactionFault;
        use crate::provider::ProviderErrorKind::ContextWindowExceeded;
        use crate::session::CompactionTrigger::{Occupancy, Overflow};
        let overflow = || Step::fail(ContextWindowExceeded.error("prompt is too long"));
        for (trigger, reserve) in [(Overflow, 16_384), (Occupancy(127_900), 100)] {
            let steps = [overflow(), overflow(), Step::new(summary(summary_json()))];
            let fixture = Fixture::new(steps).await;
            fixture.add_history(2_000).await;
            fixture.add_history(2_000).await;
            let compacted = retrying(&fixture.session, fixture.compact_for(trigger));
            assert!(compacted.await.is_ok(), "{trigger:?}");
            let requests = fixture.requests();
            let [full, half, trimmed] = &requests[1..] else {
                panic!("three summary attempts: {requests:?}")
            };
            let limit = |request: &crate::provider::protocol::ModelRequest| {
                request.max_output_tokens.unwrap().get()
            };
            let limits = [limit(full), limit(half), limit(trimmed)];
            assert_eq!(limits, [reserve, reserve / 2, reserve / 2], "{trigger:?}");
            assert_eq!(full.messages().count(), half.messages().count());
            assert!(trimmed.messages().count() < half.messages().count());
            let records = fixture.records();
            let faults =
                events!(&records, SessionEvent::CompactionFailed { error, .. } => error.clone());
            assert_eq!(
                faults,
                [CompactionFault::Overflowed, CompactionFault::Overflowed]
            );
            let requested = events!(&records, SessionEvent::ModelRequested { from, .. } => *from);
            assert!(requested.last().unwrap().is_some());
            let last = records
                .iter()
                .rev()
                .find(|record| matches!(record.event, SessionEvent::ModelRequested { .. }));
            let last = last.unwrap().sequence.request();
            let (_, replayed) = crate::session::reconstruct_model_request(&records, last).unwrap();
            assert_eq!(&replayed, trimmed);
            fixture.session.shutdown().await.unwrap();
        }
    }

    /// A completed response that reported a full context leaves the summary no room:
    /// the first stage fails before sending anything, so the next drops history, no
    /// longer reads that occupancy, and its smaller summary is sent.
    #[tokio::test]
    async fn a_full_reported_context_still_sends_the_trimmed_summary() {
        use crate::agent::CompactionFault;
        let fixture = Fixture::new([Step::new(summary(summary_json()))]).await;
        fixture.add_history(2_000).await;
        fixture.add_history(2_000).await;
        let full = crate::session::CompactionTrigger::Occupancy(128_000);
        let compacted = fixture.compact_for(full);
        assert!(retrying(&fixture.session, compacted).await.is_ok());
        let records = fixture.records();
        let faults =
            events!(&records, SessionEvent::CompactionFailed { error, .. } => error.clone());
        assert_eq!(faults, [CompactionFault::Overflowed]);
        let summaries = &fixture.requests()[1..];
        assert_eq!(summaries.len(), 1, "only the trimmed summary is sent");
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn summarizer_retries_frozen_requests_until_success() {
        for streaming in [false, true] {
            let failures = 4;
            let failure = || match streaming {
                true => failing_stream(None),
                false => Step::fail(recoverable()),
            };
            let steps = (0..failures).map(|_| failure());
            let fixture = Fixture::new(steps.chain([Step::new(summary(summary_json()))])).await;
            fixture.add_history(20_000).await;
            retrying(&fixture.session, fixture.compact()).await.unwrap();
            let requests = fixture.requests();
            let summaries = &requests[1..];
            assert_eq!(summaries.len(), failures + 1);
            assert!(summaries.windows(2).all(|pair| pair[0] == pair[1]));
            assert_eq!(fixture.script.remaining(), 0);
            let records = fixture.records();
            assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 1);
            let retries = count!(
                &records,
                SessionEvent::ModelFailed {
                    failure: crate::session::AttemptFailure::Retrying { .. },
                    ..
                }
            );
            assert_eq!(retries, failures);
            fixture.assert_no_tool_execution();
            fixture.session.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn transient_retries_do_not_consume_the_three_validation_attempts() {
        let failures = (0..4).map(|_| Step::fail(recoverable()));
        let invalid = (0..3).map(|_| Step::new(summary("not a valid continuation")));
        let fixture = Fixture::new(failures.chain(invalid)).await;
        fixture.add_history(20_000).await;
        assert!(retrying(&fixture.session, fixture.compact()).await.is_err());
        assert_eq!(fixture.requests().len() - 1, 7);
        fixture.assert_no_tool_execution();
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn observed_usage_is_counted_once_for_each_failed_agent_and_summary_attempt() {
        let observed = usage(11, 7, 3);
        let failures = || (0..4).map(|_| failing_stream(Some(observed)));
        let steps = failures()
            .chain([Step::new(answer("done"))])
            .chain(failures())
            .chain([Step::new(summary(summary_json()))]);
        let fixture = Fixture::new(steps).await;
        let prompt = fixture.session.prompt("Preserve usage.");
        assert!(retrying(&fixture.session, prompt).await.is_ok());
        assert_eq!(journaled_usage(&fixture.session).await, usage(44, 28, 12));
        fixture.add_history(20_000).await;
        let before = journaled_usage(&fixture.session).await;
        assert!(retrying(&fixture.session, fixture.compact()).await.is_ok());
        let expected = usage(
            before.input_tokens.unwrap() + 44,
            before.cached_input_tokens.unwrap() + 28,
            before.output_tokens.unwrap() + 12,
        );
        assert_eq!(journaled_usage(&fixture.session).await, expected);
        let records = fixture.records();
        assert_eq!(count!(&records, SessionEvent::ModelFailed { .. }), 8);
        let observed_usage =
            count!(&records, SessionEvent::Usage { usage, .. } if *usage == observed);
        assert_eq!(observed_usage, 8);
        fixture.assert_no_tool_execution();
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_after_four_summary_failures_preserves_history() {
        let fixture = Fixture::new((0..5).map(|_| failing_stream(None))).await;
        fixture.add_history(20_000).await;
        let root = &fixture.session.root;
        let before = crate::session::project_history(&fixture.records(), root);
        let mut events = fixture.session.runtime.store.observe().updates;
        let (result, ()) = bounded(async {
            tokio::join!(fixture.compact(), async {
                for _ in 0..3 {
                    elapse_backoff(&next_recovery(&mut events).await).await;
                }
                next_recovery(&mut events).await;
                assert_eq!(fixture.session.interrupt().await, 1);
            })
        })
        .await;
        assert!(matches!(result, Err(HarnessError::Interrupted)));
        assert_eq!(fixture.script.remaining(), 1);
        let records = fixture.records();
        let found = crate::session::project_history(&records, root);
        assert_eq!(found, before);
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
        // The interruption during a retry's backoff settles the summary request
        // on its last attempt.
        let ledger = fixture.session.observe().snapshot.ledger;
        let summary = ledger.latest(root).and_then(|request| ledger.get(request));
        assert!(matches!(
            summary.unwrap().phase,
            RequestPhase::Interrupted {
                attempt: Some(4),
                ..
            }
        ));
        fixture.assert_no_tool_execution();
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_journals_observed_usage_once_without_committing_or_executing_tools() {
        for summary in [false, true] {
            let observed = usage(11, 7, 3);
            let fixture = Fixture::new([held_after(observed)]).await;
            let (store, agent) = (&fixture.session.runtime.store, &fixture.session.root);
            let mut kept = None;
            if summary {
                fixture.add_history(20_000).await;
                let todos = vec![todo("Keep on interruption", TodoStatus::InProgress)];
                store
                    .replace_todos(agent, todos.clone(), None)
                    .await
                    .unwrap();
                let history = project_history(&fixture.records(), agent);
                kept = Some((history, todos));
                let (result, ()) = bounded(async {
                    tokio::join!(fixture.compact(), async {
                        fixture.script.held(1).await;
                        assert_eq!(fixture.session.interrupt().await, 1);
                    })
                })
                .await;
                assert!(matches!(result, Err(HarnessError::Interrupted)));
            } else {
                let (result, ()) = bounded(async {
                    tokio::join!(fixture.session.prompt("Interrupt this turn."), async {
                        fixture.script.held(1).await;
                        fixture.session.interrupt().await;
                    })
                })
                .await;
                assert!(result.is_err());
            }
            let records = fixture.records();
            let mut requested = records.iter().rev();
            let requested =
                requested.find(|r| matches!(r.event, SessionEvent::ModelRequested { .. }));
            let requested = requested.unwrap().sequence.request();
            let observed_events = events!(&records, SessionEvent::Usage { request, usage } if *request == requested => *usage);
            assert_eq!(observed_events, vec![observed]);
            let ledger = fixture.session.observe().snapshot.ledger;
            let phase = &ledger.get(requested).unwrap().phase;
            let at = phase.settled_at().unwrap();
            assert_eq!(
                *phase,
                RequestPhase::Interrupted {
                    attempt: Some(1),
                    at
                }
            );
            assert_eq!(journaled_usage(&fixture.session).await, observed);
            let committed = count!(&records, SessionEvent::MessageCommitted { message: Message::Assistant(items) }
                if items.iter().any(|item| item.id().as_str() == "interrupted-tool"));
            assert_eq!(committed, 0);
            if let Some((history, todos)) = kept {
                assert_eq!(project_history(&records, agent), history);
                assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
                assert_eq!(store.inspect_todos(agent, None).unwrap(), todos);
            }
            fixture.assert_no_tool_execution();
            fixture.session.shutdown().await.unwrap();
        }
    }
}
