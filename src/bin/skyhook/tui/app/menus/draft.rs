//! The draft's pastes and attachments, and queued follow-ups taken back to edit.
use super::{Item, Menu, MenuKind};
use crate::text::brief;
use crate::tui::app::{App, QueuedInputId};
use skyhook::media::Attachment;

/// A draft item in the attachments menu: pasted text by id, or an attachment by index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftItem {
    Paste(usize),
    Attachment(usize),
}

impl App {
    pub(super) fn attachment_items(&self) -> Vec<Item<DraftItem>> {
        let pastes = self.editor.pastes().map(|(id, text)| {
            let label = format!("Pasted text · {} lines", text.lines().count());
            Item::new(DraftItem::Paste(id), label, brief(text, 60))
        });
        let attachments = self.editor.attachments().iter().enumerate();
        let attachments = attachments.map(|(index, attachment)| {
            let kind = match &**attachment {
                Attachment::Text { .. } => "Text",
                Attachment::Image { .. } => "Image",
            };
            let source = attachment
                .file()
                .map_or_else(|| kind.to_owned(), |file| file.display().to_string());
            Item::new(DraftItem::Attachment(index), source, kind)
        });
        pastes.chain(attachments).collect()
    }
    pub(super) fn inspect_draft_item(&mut self, item: DraftItem) {
        match item {
            DraftItem::Paste(id) => {
                if let Some(text) = self.editor.paste(id) {
                    self.info("Pasted text", text.to_owned());
                }
            }
            DraftItem::Attachment(index) => {
                match self.editor.attachments().get(index).cloned().as_deref() {
                    Some(Attachment::Text { file, content }) => {
                        let title = file.as_ref().map_or_else(
                            || "Text attachment".to_owned(),
                            |file| file.display().to_string(),
                        );
                        self.info(&title, content.clone())
                    }
                    Some(Attachment::Image { file, image }) => {
                        let source = file.as_ref().map_or_else(
                            || "pasted image".to_owned(),
                            |file| file.display().to_string(),
                        );
                        let format = image.format().as_str();
                        self.info("Image attachment", format!("{source} · {format}"))
                    }
                    None => {}
                }
            }
        }
    }
    /// Remove the selected paste, attachment or queued follow-up.
    pub(super) fn delete_draft_item(&mut self) {
        let menu = self.menu();
        let index = menu.and_then(Menu::selected_index);
        match (menu.map(|menu| &menu.kind), index) {
            (Some(MenuKind::Attachments(items)), Some(index)) => match items[index].value {
                DraftItem::Paste(id) => {
                    self.editor.remove_paste(id);
                }
                DraftItem::Attachment(index) => {
                    self.editor.remove_attachment(index);
                }
            },
            (Some(MenuKind::Queue(items)), Some(index)) => {
                let id = items[index].value;
                self.remove_queued(id);
            }
            _ => return,
        }
        self.refresh_menu();
    }
    /// Take a queued follow-up back into the composer, queueing the current draft
    /// in its place.
    pub(super) fn edit_queued(&mut self, id: QueuedInputId) {
        let Some(queued) = self.remove_queued(id) else {
            return;
        };
        self.pause_queue();
        if !self.editor.is_empty() {
            let draft = self.editor.take();
            let draft = self.queued_input(draft);
            self.queue.push_front(draft);
        }
        self.replace_draft(queued.submission);
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::*;
    use crate::tui::composer::Submission;
    use crate::tui::editor::TextField;
    use crate::tui::keys::Command;
    use crossterm::event::{KeyCode, KeyModifiers as M};

    #[tokio::test]
    async fn filtered_queue_selection_pauses_and_preserves_the_current_draft() {
        let (_root, mut app) = draft_fixture().await;
        for text in ["first", "second"] {
            let attachments = vec![png_attachment(text)];
            let text = text.into();
            let queued = app.queued_input(Submission { text, attachments });
            app.queue.push_back(queued);
        }
        app.editor.set("draft".into());
        app.editor.attach(png_attachment("draft.png"));
        app.command(Command::Queue);
        app.menu_mut().unwrap().input.set("second".into());
        app.choose();
        assert!(app.paused);
        assert_eq!(app.editor.text(), "second");
        assert_eq!(app.editor.attachments(), [png_attachment("second").into()]);
        let texts: Vec<_> = app
            .queue
            .iter()
            .map(|input| &input.submission.text)
            .collect();
        assert_eq!(texts, ["draft", "first"]);
        assert_eq!(
            app.queue[0].submission.attachments,
            [png_attachment("draft.png")]
        );
        app.command(Command::Queue);
        app.menu_mut().unwrap().input.set("first".into());
        key(&mut app, KeyCode::Delete, M::NONE);
        assert_eq!(app.queue.len(), 1);
        assert!(app.paused);
        app.command(Command::Resume);
        assert!(!app.paused);
    }
}
