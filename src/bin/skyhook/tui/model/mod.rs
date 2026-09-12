//! Presentation state and the stable interface to journal-derived content.

mod cache;
mod entries;
mod jobs;
mod live;
mod notifications;
mod projection;
mod requests;
mod retry;

pub use super::format::{clean, footer, number, pretty};
pub use cache::{ContentCache, ContentChanges};
pub use entries::entries;
pub use jobs::{state_name, target_suffix};
pub use projection::{AgentInfo, JobInfo, Projection, agent_footer, agent_footer_stats};
pub use requests::RequestRow;

use super::tool_view::{Document, Run};
use skyhook::identity::{AgentId, JobId};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Conversation,
    Requests,
    Jobs,
}
impl Tab {
    pub fn next(self, backwards: bool) -> Self {
        let tabs = [Self::Conversation, Self::Requests, Self::Jobs];
        let n = tabs.iter().position(|t| *t == self).unwrap();
        tabs[(n + if backwards { tabs.len() - 1 } else { 1 }) % tabs.len()]
    }
}
#[derive(Default)]
pub struct View {
    pub tab: Tab,
    pub scroll: Option<usize>,
    pub row: usize,
    pub expanded: HashSet<String>,
    pub collapsed: HashSet<String>,
    pub query: String,
}
impl View {
    pub fn is_expanded(&self, key: &str, all: bool) -> bool {
        (all || self.expanded.contains(key)) && !self.collapsed.contains(key)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    User,
    Agent,
    Reasoning,
    Tool,
    Muted,
    Status,
    Error,
}
#[derive(Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub text: String,
    pub surface: Surface,
    pub expandable: bool,
    pub default_open: bool,
    pub running: bool,
    pub footer: Option<String>,
    /// Metadata-only request row, laid out with shared columns.
    pub request: Option<RequestRow>,
    pub indent: u16,
    pub job: Option<JobId>,
    pub document: Option<Document>,
    /// Structured presentation-only header, shared by collapsed and expanded cards.
    pub header: Option<Vec<Run>>,
    /// Omit the separator before a related sibling tool or this script's first child.
    pub compact_after: bool,
}
impl Entry {
    fn new(key: String, text: String, surface: Surface) -> Self {
        Self {
            key,
            text,
            surface,
            expandable: false,
            default_open: false,
            running: false,
            footer: None,
            request: None,
            indent: 0,
            job: None,
            document: None,
            header: None,
            compact_after: false,
        }
    }
}
#[derive(Clone, Copy)]
pub struct EntryView<'a> {
    pub agent: &'a AgentId,
    pub view: &'a View,
    pub thinking: bool,
    pub all_details: bool,
}
