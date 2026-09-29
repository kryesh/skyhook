//! Switching between the open sessions and the workspace's saved ones.
use super::{Item, MenuKind, MenuLoaded};
use crate::text::brief;
use crate::tui::app::{App, Peer, PeerSession, SlotKey, TITLE_CHARS, Work};
use crate::tui::format::{Precision, local_time};
use crate::tui::model::AgentDisplayState;
use chrono::{Local, NaiveDate};
use futures_util::stream::{FuturesUnordered, StreamExt};
use skyhook::identity::SessionId;
use skyhook::session::SessionStore;
use std::path::{Path, PathBuf};

/// A switch-session row: an open session by host key, or one saved on disk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionRef {
    Live(SlotKey),
    Saved(SessionId, SavedState),
}

/// A saved session as the switcher shows it before opening it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SavedState {
    /// Held open when the list was read, by another Skyhook instance or by a
    /// session still closing here; only a hint, as opening decides.
    Locked,
    /// Its root agent's state, as it would resume.
    Closed(AgentDisplayState),
    /// Its state could not be read; it draws no glyph.
    Unknown,
}

/// How recently a row's session was active; a draft is as new as it gets.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Recency {
    At(i64),
    Now,
}

/// A session saved in the workspace, as its row shows it.
struct Saved {
    id: SessionId,
    last: i64,
    label: String,
    events: String,
    mode: String,
    state: SavedState,
}

impl Saved {
    /// Its label and columns, with its last activity shown as `time`.
    fn row(self, time: String) -> (String, Vec<String>) {
        (
            self.label,
            vec![self.id.to_string(), self.events, self.mode, time],
        )
    }
}

/// Working sessions first, then every other open or saved session newest first.
/// Each row's columns, right to left by importance: when it was last active, its
/// mode, how many events it holds, and its id.
async fn load_sessions(
    root: PathBuf,
    peers: Vec<Peer>,
    today: NaiveDate,
) -> Result<Vec<Item<SessionRef>>, String> {
    Ok(session_rows(load_saved(&root).await?, &peers, today))
}

fn session_rows(mut saved: Vec<Saved>, peers: &[Peer], today: NaiveDate) -> Vec<Item<SessionRef>> {
    let time = |last| local_time(last, today, &Local, Precision::Minutes);
    let mut rows: Vec<_> = peers
        .iter()
        .map(|peer| {
            // An open session is listed once, as open, never also as saved.
            let mut take = |id| {
                let index = saved.iter().position(|row: &Saved| row.id == id);
                index.map(|index| saved.remove(index))
            };
            let draft = || {
                let columns = ["", "", "", "draft"].map(String::from);
                ("New session".to_owned(), columns.into(), Recency::Now)
            };
            let (label, columns, recency) = match peer.session {
                PeerSession::Draft => draft(),
                // It reads as a draft until its first record is observed.
                PeerSession::Started(id) => {
                    take(id);
                    draft()
                }
                PeerSession::Open { id, last } => {
                    let (label, columns) = match take(id) {
                        Some(row) => row.row(time(last)),
                        None => {
                            let columns =
                                [id.to_string(), String::new(), String::new(), time(last)];
                            ("New session".to_owned(), columns.into())
                        }
                    };
                    (label, columns, Recency::At(last))
                }
            };
            let item = Item::columned(SessionRef::Live(peer.key), label, "", columns);
            (peer.working, recency, item)
        })
        .collect();
    rows.extend(saved.into_iter().map(|row| {
        let (value, last) = (SessionRef::Saved(row.id, row.state), row.last);
        let (label, columns) = row.row(time(last));
        let item = Item::columned(value, label, "", columns);
        (false, Recency::At(last), item)
    }));
    rows.sort_by_key(|(working, recency, _)| std::cmp::Reverse((*working, *recency)));
    rows.into_iter().map(|(_, _, item)| item).collect()
}

/// Sessions scanned at once.
const CONCURRENT_SCANS: usize = 4;

/// Every saved session in `root`, scanned a few at a time so their reads overlap.
async fn load_saved(root: &Path) -> Result<Vec<Saved>, String> {
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.to_string()),
    };
    let mut ids = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        ids.extend(
            entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<SessionId>().ok()),
        );
    }
    let mut ids = ids.into_iter();
    let mut scans: FuturesUnordered<_> = ids
        .by_ref()
        .take(CONCURRENT_SCANS)
        .map(|id| scan(root, id))
        .collect();
    let mut sessions = Vec::new();
    while let Some(saved) = scans.next().await {
        sessions.extend(saved);
        scans.extend(ids.next().map(|id| scan(root, id)));
    }
    Ok(sessions)
}

/// One saved session's row, read without opening it; a session whose summary
/// cannot be read is left out.
async fn scan(root: &Path, id: SessionId) -> Option<Saved> {
    let (summary, open) = tokio::join!(
        SessionStore::summary(root, id),
        SessionStore::is_open(root, id)
    );
    let summary = summary.ok()?;
    let state = match open {
        Ok(true) => SavedState::Locked,
        Ok(false) => SavedState::Closed(
            (summary.stopped.as_ref()).map_or(AgentDisplayState::Ready, AgentDisplayState::stopped),
        ),
        Err(_) => SavedState::Unknown,
    };
    let label = (summary.title.as_ref())
        .map_or_else(|| id.to_string(), |title| brief(&title.text, TITLE_CHARS));
    Some(Saved {
        id,
        last: summary.last_millis,
        label,
        events: format!("{} events", summary.entries),
        mode: summary
            .mode
            .map(|mode| mode.to_string())
            .unwrap_or_default(),
        state,
    })
}

impl App {
    pub(super) fn open_sessions(&mut self) {
        let root = self.launch.sessions.clone();
        let (peers, today) = (self.peers.clone(), self.clock.day());
        let tx = self.tx.clone();
        self.open("Switch session", MenuKind::Sessions(vec![]));
        let id = self.menu().unwrap().id;
        tokio::spawn(async move {
            let result = load_sessions(root, peers, today).await;
            let _ = tx.send(Work::MenuLoaded(MenuLoaded::Sessions(id, result)));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn switch_menu_pins_working_sessions_then_orders_by_last_activity() {
        use skyhook::execution::ExecutionLocation;
        use skyhook::identity::AgentId;
        use skyhook::job::JobState;
        use skyhook::session::{Message, SessionEvent, UserPart};
        let root = tempfile::tempdir().unwrap();
        let started = [
            SessionEvent::SessionStarted {
                targets: Vec::new(),
                capabilities: Vec::new(),
            },
            SessionEvent::AgentStarted {
                owner_job: None,
                profile: None,
                available_depth: 0,
                mode: None,
                capabilities: Vec::new(),
                location: ExecutionLocation::root(root.path().to_owned()),
            },
        ];
        let mut stores = vec![];
        for index in 0..6 {
            let store = SessionStore::create(root.path()).await.unwrap();
            let agent = AgentId::root(store.id());
            let mut events = started.to_vec();
            // A prompt no answer followed resumes interrupted.
            if index == 3 {
                let text = "unanswered".into();
                let message = Message::User(vec![UserPart::Text { text }]);
                events.push(SessionEvent::MessageCommitted { message });
            }
            let records = store
                .append_all(
                    events
                        .into_iter()
                        .map(|event| (agent.clone(), event))
                        .collect(),
                )
                .await
                .unwrap();
            let committed = records.last().unwrap().timestamp_millis;
            // Keep one store's original lock held through the scan.
            if index != 4 {
                store.close().await.unwrap();
            }
            stores.push((store, committed));
        }
        let ids: Vec<_> = stores.iter().map(|(store, _)| store.id()).collect();
        let mut saved = load_saved(root.path()).await.unwrap();
        assert_eq!(saved.len(), 6);
        for row in &mut saved {
            let index = ids.iter().position(|id| *id == row.id).unwrap();
            assert_eq!(row.last, stores[index].1);
            // Appends use wall time; row ordering must not depend on clock resolution.
            row.last = [5, 6, 20, 40, 30, 7][index];
        }
        let keys: Vec<_> = std::iter::successors(Some(SlotKey::default()), |key| Some(key.next()))
            .take(4)
            .collect();
        let peer = |key, session, working| Peer {
            key,
            session,
            state: AgentDisplayState::Ready,
            current: false,
            attention: false,
            working,
        };
        let open = |id, last| PeerSession::Open { id, last };
        // Long idle, then working, then fresh drafts, one whose journal exists:
        // working pins, drafts are newest, and the idle session sorts below the
        // saved ones by time. Open sessions are never listed again as saved.
        let peers = vec![
            peer(keys[0], open(ids[0], 1), false),
            peer(keys[1], open(ids[1], 0), true),
            peer(keys[2], PeerSession::Draft, false),
            peer(keys[3], PeerSession::Started(ids[5]), false),
        ];
        let today = Local::now().date_naive();
        let items = session_rows(saved, &peers, today);
        let rows: Vec<_> = items.iter().map(|item| item.value).collect();
        let live = [keys[1], keys[2], keys[3]].map(SessionRef::Live);
        assert!(rows.len() == 7 && rows[..3] == live);
        assert!(rows[6] == SessionRef::Live(keys[0]));
        let saved = [
            SessionRef::Saved(
                ids[3],
                SavedState::Closed(AgentDisplayState::Job(JobState::Interrupted)),
            ),
            SessionRef::Saved(ids[4], SavedState::Locked),
            SessionRef::Saved(ids[2], SavedState::Closed(AgentDisplayState::Ready)),
        ];
        assert!(rows[3..6] == saved);
        let time = |index: usize| items[index].columns.last().unwrap().as_str();
        assert_eq!(time(6), local_time(1, today, &Local, Precision::Minutes));
        assert_eq!(items[6].columns[0], ids[0].to_string());
        assert!(time(1) == "draft" && time(2) == "draft");
        stores[4].0.close().await.unwrap();
    }
}
