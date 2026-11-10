//! The agent tree: moving through it, and which agent is viewed.
use super::extensions::Shown;
use super::{App, Pane};
use crate::tui::commands::Command;
use skyhook::identity::AgentId;

impl App {
    /// View the agent above or below the viewed one in the tree, wrapping.
    pub(super) fn step_agent(&mut self, backwards: bool) {
        let step = |rows: &[&AgentId], current| {
            let len = rows.len();
            Some((current + if backwards { len - 1 } else { 1 }) % len)
        };
        self.move_in_tree(step);
    }
    /// Move a page up or down the tree, scrolling it along.
    pub(super) fn page_tree(&mut self, down: bool) {
        let len = self.tree_agents().len();
        let step = self.tree_view.page(down, len);
        self.move_in_tree(|rows, current| {
            Some(current.saturating_add_signed(step).min(rows.len() - 1))
        });
    }
    /// Move to the first listed child of the tree's current row.
    pub(super) fn tree_child(&mut self) {
        self.move_in_tree(|rows, current| {
            let parent = Some(rows[current].clone());
            rows.iter().position(|agent| agent.parent() == parent)
        });
    }
    /// Move to the nearest listed ancestor of the tree's current row.
    pub(super) fn tree_parent(&mut self) {
        self.move_in_tree(|rows, current| {
            std::iter::successors(rows[current].parent(), AgentId::parent)
                .find_map(|ancestor| rows.iter().position(|agent| **agent == ancestor))
        });
    }
    /// Move to the tree row `to` picks from the listed rows and the current one, and keep
    /// the tree focused for the next step. Every move chooses among the rows the tree lists.
    /// Under a conversation the tree's row is the viewed agent, so moving views the agent at
    /// its latest activity; beside a page it is only the cursor, and the conversation to come
    /// back to stays as it was.
    fn move_in_tree(&mut self, to: impl FnOnce(&[&AgentId], usize) -> Option<usize>) {
        let shown = self.tree_agents();
        if shown.is_empty() {
            return;
        }
        let current = match self.tree_viewed() {
            Some(_) => self.tree_row(),
            None => Some(self.tree_cursor.min(shown.len() - 1)),
        };
        let Some(current) = current else {
            return;
        };
        let rows: Vec<&AgentId> = (shown.iter())
            .map(|&i| &self.projection.agents[i].id)
            .collect();
        let Some(next) = to(&rows, current) else {
            return;
        };
        self.tree_cursor = next;
        if self.tree_viewed().is_some() {
            self.select(self.projection.agents[shown[next]].id.clone());
            self.tree_cursor = self.tree_row().unwrap_or(0);
            self.view().top = None;
        }
        self.focus = Pane::Tree;
    }
    /// The viewed agent's row in the tree, where it marks one.
    pub(super) fn tree_row(&self) -> Option<usize> {
        let viewed = self.tree_viewed()?;
        let agents = &self.projection.agents;
        (self.tree_agents().iter()).position(|&i| &agents[i].id == viewed)
    }
    pub(super) fn select(&mut self, agent: AgentId) {
        self.selected = agent;
        self.focus = Pane::Content;
        self.selection = None;
        self.invalidate_content();
    }
    /// The agent the tree marks and keeps the ancestors of: the viewed one under its
    /// conversation, and none beside a page, which shows no conversation.
    pub(crate) fn tree_viewed(&self) -> Option<&AgentId> {
        (!self.page_frame()).then_some(&self.selected)
    }
    /// The agents the tree lists, in order.
    pub(crate) fn tree_agents(&self) -> Vec<usize> {
        self.projection.visible(self.tree_viewed()).collect()
    }
    /// View `agent`, leaving an extension view for its conversation: a view never shows
    /// whom it was chosen for.
    pub(super) fn open_agent(&mut self, agent: AgentId) {
        self.select(agent);
        if matches!(self.panel, Shown::Extension(_)) {
            self.command(Command::Inspect);
        }
    }
}
