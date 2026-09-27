//! The workspace file picker, which attaches a file to the draft.
use super::{Item, MenuKind, MenuLoaded};
use crate::tui::app::{App, Work};
use std::path::PathBuf;

const MAX_FILE_MENU_ITEMS: usize = 10_000;

/// Build output and dependency trees, skipped even where no ignore file lists them.
const SKIPPED_DIRECTORIES: [&str; 2] = ["target", "node_modules"];

/// Workspace files by relative path, as ignore files leave them visible, in a git
/// repository or not.
fn workspace_files(root: &std::path::Path) -> Vec<Item<PathBuf>> {
    let walk = ignore::WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(|entry| {
            let directory = entry.file_type().is_some_and(|kind| kind.is_dir());
            !(directory
                && SKIPPED_DIRECTORIES
                    .iter()
                    .any(|name| entry.file_name() == *name))
        })
        .sort_by_file_path(Ord::cmp)
        .build();
    let files = walk
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()));
    files
        .take(MAX_FILE_MENU_ITEMS)
        .map(|entry| {
            let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
            Item::new(relative.to_path_buf(), relative.display().to_string(), "")
        })
        .collect()
}

impl App {
    /// Open the workspace file picker; `at` follows an `@` typed in the composer.
    pub(in crate::tui::app) fn open_files(&mut self, at: Option<usize>) {
        self.open("Attach workspace file", MenuKind::Files(vec![], at));
        let id = self.menu().unwrap().id;
        let root = self.launch.workspace.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let items = workspace_files(&root);
            let _ = tx.send(Work::MenuLoaded(MenuLoaded::Files(id, items)));
        });
    }
    pub(super) fn attach_file(&mut self, path: &std::path::Path, at: Option<usize>) {
        let root = self.launch.workspace.clone();
        let path = root.join(path);
        let tx = self.tx.clone();
        let draft = self.draft_ticket.clone();
        tokio::spawn(async move {
            let result = crate::launch::read_attachment(&root, &path).await;
            let _ = tx.send(Work::File { draft, at, result });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::*;
    use super::*;
    use crate::tui::editor::TextField;
    use crossterm::event::{KeyCode, KeyModifiers as M};
    use skyhook::media::Attachment;

    #[cfg(unix)]
    #[tokio::test]
    async fn file_menu_preserves_non_unicode_paths_and_attaches_images() {
        use std::os::unix::ffi::OsStringExt;
        let (_root, mut app) = draft_fixture().await;
        let workspace = app.launch.workspace.clone();
        let relative = PathBuf::from(std::ffi::OsString::from_vec(b"file-\xff.txt".to_vec()));
        std::fs::write(workspace.join(&relative), "file contents").unwrap();
        let bytes = b"\x89PNG\r\n\x1a\nfixture";
        std::fs::write(workspace.join("shot.png"), bytes).unwrap();
        let items = workspace_files(&workspace);
        let mut rx = capture_work(&mut app);
        let canonical = |path: &PathBuf| Some(workspace.join(path).canonicalize().unwrap());
        let text = Attachment::Text {
            file: canonical(&relative),
            content: "file contents".into(),
        };
        let image = PathBuf::from("shot.png");
        let png = Attachment::Image {
            file: canonical(&image),
            image: skyhook::media::Image::new(bytes.to_vec()).unwrap(),
        };
        for (path, expected) in [(relative, text), (image, png)] {
            app.open("Files", MenuKind::Files(items.clone(), None));
            let selected = items.iter().position(|item| item.value == path);
            app.menu_mut().unwrap().selected = selected.unwrap();
            app.choose();
            let Work::File { result, .. } = recv(&mut rx).await else {
                panic!("file read")
            };
            assert_eq!(result.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn file_menu_leaves_out_ignored_hidden_and_build_files_outside_git() {
        let (_root, app) = draft_fixture().await;
        let workspace = &app.launch.workspace;
        for (path, text) in [
            (".skyhook/state.json", "{}"),
            (".gitignore", "build/\n"),
            ("build/out.txt", "ignored"),
            ("node_modules/left-pad/index.js", "dependency"),
            ("target/debug/app", "build output"),
            ("src/lib.rs", "shown"),
        ] {
            let path = workspace.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let files = workspace_files(workspace);
        let files: Vec<_> = files.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(files, ["src/lib.rs"]);
    }

    #[tokio::test]
    async fn typed_at_sign_is_kept_unless_a_file_replaces_it() {
        let (_root, mut app) = draft_fixture().await;
        std::fs::write(app.launch.workspace.join("notes.txt"), "notes").unwrap();
        let mut rx = capture_work(&mut app);
        app.editor.set("mail user".into());
        key(&mut app, KeyCode::Char('@'), M::SHIFT);
        assert!(matches!(app.menu().unwrap().kind, MenuKind::Files(..)));
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(app.menu().is_none());
        assert_eq!(app.editor.text(), "mail user@");
        key(&mut app, KeyCode::Char('@'), M::SHIFT);
        app.menu_mut().unwrap().input.set("notes".into());
        while app.menu().unwrap().filtered().is_empty() {
            let work = recv(&mut rx).await;
            app.work(work);
        }
        key(&mut app, KeyCode::Enter, M::NONE);
        while app.editor.attachments().is_empty() {
            let work = recv(&mut rx).await;
            app.work(work);
        }
        // The chosen file consumes only the `@` that opened its picker.
        assert_eq!(app.editor.text(), "mail user@");
        assert!(matches!(
            app.editor.attachments(),
            [a] if matches!(&**a, Attachment::Text { file: Some(_), content } if content == "notes")
        ));
        assert!(!app.editor.has_pastes());
    }
}
