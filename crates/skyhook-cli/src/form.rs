//! A product's scope form: settings for a new session, kept above its conversation
//! while the first message is written and sent with it.
use std::ops::RangeInclusive;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeForm {
    pub title: String,
    pub fields: Vec<Field>,
    /// Sent as the first message when the draft is submitted without text.
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// The label beside the value.
    pub name: String,
    /// One line of help, shown while the field has the keys.
    pub prompt: String,
    pub kind: FieldKind,
}

/// A field and its default value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Choice(Choice),
    Integer {
        value: u16,
        range: RangeInclusive<u16>,
    },
}

/// Options with one of them selected: never empty, the selection always one of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    options: Vec<String>,
    selected: usize,
}

impl Choice {
    /// `None` without options or when `selected` is not one of them.
    pub fn new(options: Vec<String>, selected: usize) -> Option<Self> {
        (selected < options.len()).then_some(Self { options, selected })
    }

    pub fn options(&self) -> &[String] {
        &self.options
    }

    pub fn selected(&self) -> usize {
        self.selected
    }
}

/// One field's answer, at the field's position in the form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldValue {
    Choice(usize),
    Integer(u16),
}

/// Every field's value, in form order.
pub type ScopeReply = Vec<FieldValue>;

impl FieldValue {
    pub fn integer(self) -> Option<u16> {
        match self {
            Self::Integer(value) => Some(value),
            Self::Choice(_) => None,
        }
    }

    pub fn choice(self) -> Option<usize> {
        match self {
            Self::Choice(index) => Some(index),
            Self::Integer(_) => None,
        }
    }
}
