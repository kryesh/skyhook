//! The per-turn session token some services issue and expect back.

use std::sync::{Arc, Mutex};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::provider::protocol::{Message, ModelRequest, UserContent};

/// State the service issues during a turn and expects back within it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Session {
    None,
    /// A header received on a turn's first response and replayed on that turn's
    /// later requests only; cleared when a new turn opens.
    StickyTurn {
        header: HeaderName,
    },
}

/// The held per-turn token of one provider context.
#[derive(Clone, Default)]
pub(crate) struct SessionState {
    token: Arc<Mutex<Option<HeaderValue>>>,
}

impl SessionState {
    /// The header `request` carries back, if its turn holds a token.
    pub(crate) fn prepare(
        &self,
        session: &Session,
        request: &ModelRequest,
    ) -> Option<(HeaderName, HeaderValue)> {
        let Session::StickyTurn { header } = session else {
            return None;
        };
        let mut token = self.token.lock().expect("session lock");
        if !continues_turn(request) {
            *token = None;
        }
        Some((header.clone(), token.clone()?))
    }

    pub(crate) fn observe(&self, session: &Session, response: &HeaderMap) {
        let Session::StickyTurn { header } = session else {
            return;
        };
        if let Some(received) = response.get(header) {
            let mut token = self.token.lock().expect("session lock");
            token.get_or_insert_with(|| received.clone());
        }
    }
}

/// Whether the request answers tool calls, rather than opening a turn with user
/// input or a wake. Runtime state after the results is part of the same request.
fn continues_turn(request: &ModelRequest) -> bool {
    let runtime_only = |message: &&Message| matches!(message, Message::User(parts) if parts.iter().all(UserContent::is_runtime));
    let mut history = request.history.iter().rev().skip_while(runtime_only);
    matches!(history.next(), Some(Message::Tool(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        codec::common::tests::{reasoning_tool_request, scope},
        protocol::{AssistantItem, UserContent},
    };

    #[test]
    fn turn_state_is_replayed_within_its_turn_only() {
        const HEADER: &str = "x-turn-state";
        let session = Session::StickyTurn {
            header: HeaderName::from_static(HEADER),
        };
        let state = SessionState::default();
        let exchange = reasoning_tool_request(&scope()).history;
        let user = |part| Message::User(vec![part]);
        let runtime = || {
            user(UserContent::Runtime {
                text: "<skyhook_state>".into(),
            })
        };
        // The opening request, a tool-loop request with persisted state, a wake by a
        // runtime event alone, that new turn's own tool loop, then queued user input.
        let steps: [(Vec<Message>, &str, Option<&str>); 5] = [
            (
                vec![user(UserContent::Text {
                    text: "start".into(),
                })],
                "first",
                None,
            ),
            (
                [exchange.clone(), vec![runtime()]].concat(),
                "ignored",
                Some("first"),
            ),
            (
                vec![
                    Message::Assistant(vec![AssistantItem::text("t", 0, "done")]),
                    runtime(),
                ],
                "second",
                None,
            ),
            (exchange, "unused", Some("second")),
            (
                vec![user(UserContent::Text {
                    text: "queued".into(),
                })],
                "unused",
                None,
            ),
        ];
        let mut request = reasoning_tool_request(&scope());
        request.history.clear();
        for (step, received, expected) in steps {
            request.history.extend(step);
            let sent = state.prepare(&session, &request);
            assert_eq!(
                sent.map(|(_, value)| value.to_str().unwrap().to_owned()),
                expected.map(str::to_owned)
            );
            let mut response = HeaderMap::new();
            response.insert(HEADER, HeaderValue::from_static(received));
            state.observe(&session, &response);
        }
        // Without a sticky header nothing is held or sent.
        assert!(state.prepare(&Session::None, &request).is_none());
    }
}
