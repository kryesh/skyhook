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
    use crate::agent::runtime::tests::{journaled_usage, retrying};
    use crate::{
        agent::TodoStatus,
        agent::runtime::HarnessError,
        session::{Message, RequestPhase, SessionEvent},
    };
    use std::assert_matches;

    /// A summary that overflows the context window is retried smaller: with half the
    /// output reserve, then also without the older half of its history, which its
    /// request journals so reconstruction sends what was sent. Dropping history never
    /// raises the reserve again, even when the reported occupancy had constrained it.
    /// A completed response that reported a full context leaves the summary no room:
    /// the first stage fails before sending anything, so the next drops history, no
    /// longer reads that occupancy, and its smaller summary is sent.
    #[tokio::test]
    async fn an_overflowing_summary_is_retried_with_half_reserve_then_half_history() {
        use crate::agent::CompactionFault;
        use crate::provider::ProviderErrorKind::ContextWindowExceeded;
        use crate::session::CompactionTrigger::{Occupancy, Overflow};
        let overflow = || Step::fail(ContextWindowExceeded.error("prompt is too long"));
        // The overflows the provider reports, the output limits of the summaries
        // sent, and the stages that failed.
        for (trigger, overflows, limits, failed) in [
            (Overflow, 2, &[16_384, 8_192, 8_192][..], 2),
            (Occupancy(127_900), 2, &[100, 50, 50], 2),
            (Occupancy(128_000), 0, &[8_192], 1),
        ] {
            let steps = (0..overflows).map(|_| overflow());
            let fixture = Fixture::new(steps.chain([Step::new(summary(summary_json()))])).await;
            fixture.add_history(2_000).await;
            fixture.add_history(2_000).await;
            let compacted = retrying(&fixture.session, fixture.compact_for(trigger));
            assert!(compacted.await.is_ok(), "{trigger:?}");
            let requests = fixture.requests();
            let sent = &requests[1..];
            let limit = |request: &crate::provider::protocol::ModelRequest| {
                request.max_output_tokens.unwrap().get()
            };
            assert_eq!(
                sent.iter().map(limit).collect::<Vec<_>>(),
                limits,
                "{trigger:?}"
            );
            let (trimmed, untrimmed) = sent.split_last().unwrap();
            if let Some(half) = untrimmed.last() {
                let count = half.messages().count();
                assert!(
                    untrimmed
                        .iter()
                        .all(|sent| sent.messages().count() == count)
                );
                assert!(trimmed.messages().count() < count);
            }
            let records = fixture.records();
            let faults =
                events!(&records, SessionEvent::CompactionFailed { error, .. } => error.clone());
            assert_eq!(
                faults,
                vec![CompactionFault::Overflowed; failed],
                "{trigger:?}"
            );
            let requested = events!(&records, SessionEvent::ModelRequested { from, .. } => *from);
            assert!(requested.last().unwrap().is_some());
            let last = records
                .iter()
                .rev()
                .find(|record| matches!(record.event, SessionEvent::ModelRequested { .. }));
            let last = last.unwrap().sequence.request();
            let replayed = fixture
                .session
                .runtime
                .store
                .reconstruct_model_request(last);
            let (_, replayed) = replayed.await.unwrap();
            assert_eq!(&replayed, trimmed);
            fixture.session.shutdown().await.unwrap();
        }
    }

    /// Transient failures resend the frozen request until it succeeds, and do not
    /// consume the three validation attempts.
    #[tokio::test]
    async fn summarizer_retries_frozen_requests_until_success() {
        let failures = 4;
        let transient = (0..failures).map(|_| Step::fail(recoverable()));
        let invalid = (0..2).map(|_| Step::new(summary("not a valid continuation")));
        let valid = Step::new(summary(summary_json()));
        let fixture = Fixture::new(transient.chain(invalid).chain([valid])).await;
        fixture.add_history(20_000).await;
        retrying(&fixture.session, fixture.compact()).await.unwrap();
        let requests = fixture.requests();
        let summaries = &requests[1..];
        assert_eq!(summaries.len(), failures + 3);
        assert!(summaries[..=failures].array_windows().all(|[a, b]| a == b));
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

    #[tokio::test]
    async fn observed_usage_is_counted_once_for_each_failed_summary_attempt() {
        let observed = usage(11, 7, 3);
        let failures = (0..4).map(|_| failing_stream(Some(observed)));
        let fixture = Fixture::new(failures.chain([Step::new(summary(summary_json()))])).await;
        fixture.add_history(20_000).await;
        assert!(retrying(&fixture.session, fixture.compact()).await.is_ok());
        assert_eq!(journaled_usage(&fixture.session).await, usage(44, 28, 12));
        let records = fixture.records();
        assert_eq!(count!(&records, SessionEvent::ModelFailed { .. }), 4);
        let observed_usage =
            count!(&records, SessionEvent::Usage { usage, .. } if *usage == observed);
        assert_eq!(observed_usage, 4);
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
                let history = fixture.history(agent).await;
                kept = Some((history, todos));
                let (result, ()) = bounded(async {
                    tokio::join!(fixture.compact(), async {
                        fixture.script.held(1).await;
                        assert_eq!(fixture.session.interrupt().await, 1);
                    })
                })
                .await;
                assert_matches!(result, Err(HarnessError::Interrupted));
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
            let ledger = crate::session::tests::ledger(&fixture.session.runtime.store);
            let phase = &ledger.requests[&requested].phase;
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
                assert_eq!(fixture.history(agent).await, history);
                assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
                assert_eq!(store.inspect_todos(agent, None).unwrap(), todos);
            }
            fixture.assert_no_tool_execution();
            fixture.session.shutdown().await.unwrap();
        }
    }
}
