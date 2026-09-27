//! Switching between the open sessions and the workspace's saved ones.
use super::{Item, MenuKind, MenuLoaded};
use crate::text::brief;
use crate::tui::app::{App, Peer, SlotKey, TITLE_CHARS, Work};
use skyhook::identity::SessionId;
use skyhook::session::SessionStore;
use std::path::PathBuf;

/// A switch-session row: an open session by host key, or one saved on disk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionRef {
    Live(SlotKey),
    Saved(SessionId),
}

/// Open sessions first, in host order, then the rest of the workspace's history.
async fn load_sessions(root: PathBuf, peers: Vec<Peer>) -> Result<Vec<Item<SessionRef>>, String> {
    let mut saved = load_saved(root).await?;
    let mut items: Vec<_> = peers
        .iter()
        .map(|peer| {
            let item = peer
                .session
                .and_then(|id| saved.iter().position(|item| item.value == id))
                .map(|index| saved.remove(index));
            let (label, mut detail) = match (item, peer.session) {
                (Some(item), _) => (item.label, item.detail),
                (None, Some(id)) => (id.to_string(), String::new()),
                (None, None) => ("New session".to_owned(), "draft".to_owned()),
            };
            if peer.current {
                detail = format!("current · {detail}");
            }
            Item::new(SessionRef::Live(peer.key), label, detail)
        })
        .collect();
    items.extend(
        saved
            .into_iter()
            .map(|item| Item::new(SessionRef::Saved(item.value), item.label, item.detail)),
    );
    Ok(items)
}
async fn load_saved(root: PathBuf) -> Result<Vec<Item<SessionId>>, String> {
    let mut entries = match tokio::fs::read_dir(&root).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.to_string()),
    };
    let mut sessions = vec![];
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<SessionId>().ok())
        else {
            continue;
        };
        let Ok(summary) = SessionStore::summary(&root, id).await else {
            continue;
        };
        let title = summary
            .title
            .or_else(|| summary.preview.map(|text| brief(&text, TITLE_CHARS)))
            .unwrap_or_else(|| id.to_string());
        let mode = summary
            .mode
            .map(|mode| format!("{mode} · "))
            .unwrap_or_default();
        let detail = format!("{mode}{} events · {id}", summary.entries);
        sessions.push((summary.last_millis, Item::new(id, title, detail)));
    }
    sessions.sort_by_key(|(timestamp, _)| std::cmp::Reverse(*timestamp));
    Ok(sessions.into_iter().map(|(_, item)| item).collect())
}

impl App {
    pub(super) fn open_sessions(&mut self) {
        let root = self.launch.sessions.clone();
        let peers = self.peers.clone();
        let tx = self.tx.clone();
        self.open("Switch session", MenuKind::Sessions(vec![]));
        let id = self.menu().unwrap().id;
        tokio::spawn(async move {
            let result = load_sessions(root, peers).await;
            let _ = tx.send(Work::MenuLoaded(MenuLoaded::Sessions(id, result)));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::*;
    use super::*;
    use crate::tui::model;

    #[tokio::test]
    async fn switch_menu_lists_open_sessions_once_above_saved_ones() {
        let (_root, app) = draft_fixture().await;
        let mut ids = vec![];
        for _ in 0..2 {
            let session = app.launch.create(None).await.unwrap();
            session.shutdown().await.unwrap();
            ids.push(session.id());
        }
        let (first, second) = (SlotKey::default(), SlotKey::default().next());
        let peer = |key, session| Peer {
            key,
            session,
            state: model::AgentDisplayState::Ready,
            current: false,
            attention: false,
            working: false,
        };
        let peers = vec![peer(first, Some(ids[0])), peer(second, None)];
        let items = load_sessions(app.launch.sessions.clone(), peers).await;
        let items: Vec<_> = items.unwrap().into_iter().map(|item| item.value).collect();
        let expected = [
            SessionRef::Live(first),
            SessionRef::Live(second),
            SessionRef::Saved(ids[1]),
        ];
        assert!(items == expected);
    }
}
