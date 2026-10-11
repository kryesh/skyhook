//! The context window a request sends, in order: conversation and runtime state.

use serde::{Deserialize, Serialize};

use super::Message;
use crate::named_enum::named_enum;

/// One entry of a request's context.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Entry {
    Message(Message),
    /// Runtime state the harness wrote: never user input or tool output, so each codec
    /// sends it in the role its dialect selects.
    State(State),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct State {
    pub text: String,
    pub lifetime: StateLifetime,
}

named_enum! {
    /// How long a state stays in the model's view.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
    pub enum StateLifetime {
        /// Committed to the conversation: the model reads it in every later request a
        /// response follows it in.
        Persisted = "persisted",
        /// An earlier request's state, which that request's response follows. The model
        /// never reads it: a codec that can hide it resends it so the conversation stays
        /// unchanged for reasoning bound to it, and any other omits it.
        Superseded = "superseded",
        /// This request's own state.
        Current = "current",
    }
}

impl From<Message> for Entry {
    fn from(message: Message) -> Self {
        Self::Message(message)
    }
}

impl Entry {
    #[must_use]
    pub fn message(&self) -> Option<&Message> {
        match self {
            Self::Message(message) => Some(message),
            Self::State(_) => None,
        }
    }

    fn is_current(&self) -> bool {
        matches!(self, Self::State(state) if state.lifetime == StateLifetime::Current)
    }
}

impl State {
    /// Whether the model reads this state when it is sent.
    #[must_use]
    pub fn is_read(&self) -> bool {
        self.lifetime != StateLifetime::Superseded
    }
}

/// The entries of `context` every codec sends, split where the request's own (current)
/// state begins: the prefix later requests extend, then the rest.
///
/// Every message is sent, and each state an assistant message or nothing follows. Any
/// other state was followed by a later state or by input before its request produced a
/// response, so nothing is bound to it.
pub fn sent(
    context: &[Entry],
) -> (
    impl Iterator<Item = &Entry> + '_,
    impl Iterator<Item = &Entry> + '_,
) {
    let split = (context.iter())
        .position(Entry::is_current)
        .unwrap_or(context.len());
    let is_sent = |index: usize| match &context[index] {
        Entry::Message(_) => true,
        Entry::State(_) => matches!(
            context.get(index + 1),
            None | Some(Entry::Message(Message::Assistant(_)))
        ),
    };
    let entries = move |range: std::ops::Range<usize>| {
        range
            .filter(move |index| is_sent(*index))
            .map(|index| &context[index])
    };
    (entries(0..split), entries(split..context.len()))
}

/// A context of `messages` alone.
#[cfg(test)]
pub(crate) fn entries(messages: impl IntoIterator<Item = Message>) -> Vec<Entry> {
    messages.into_iter().map(Entry::from).collect()
}

/// A state entry.
#[cfg(test)]
pub(crate) fn state(text: &str, lifetime: StateLifetime) -> Entry {
    Entry::State(State {
        text: text.into(),
        lifetime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::{AssistantItem, UserContent};

    /// A state is sent only when a response or the end follows it; the split starts at
    /// the first current state.
    #[test]
    fn only_states_a_response_or_the_end_follows_are_sent() {
        let user = Entry::Message(Message::User(vec![UserContent::Text {
            text: "input".into(),
        }]));
        let answer = Entry::Message(Message::Assistant(vec![AssistantItem::text("a", 0, "x")]));
        let context = [
            user.clone(),
            state("answered", StateLifetime::Superseded),
            answer.clone(),
            state("failed", StateLifetime::Persisted),
            user.clone(),
            state("stale", StateLifetime::Persisted),
            state("current", StateLifetime::Current),
        ];
        let text = |entry: &Entry| match entry {
            Entry::State(state) => state.text.clone(),
            Entry::Message(_) => "message".into(),
        };
        let (prefix, rest) = sent(&context);
        let prefix: Vec<_> = prefix.map(text).collect();
        assert_eq!(prefix, ["message", "answered", "message", "message"]);
        assert!(rest.map(text).eq(["current"]));
    }
}
