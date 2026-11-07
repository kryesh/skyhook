//! Switching between the open sessions and the workspace's saved ones.
use super::{Emphasis, Item, MenuKind, MenuLoaded};
use crate::Identity;
use crate::text::brief;
use crate::tui::app::{App, Peer, PeerSession, SlotKey, TITLE_CHARS, Work};
use crate::tui::format::{Precision, local_time};
use crate::tui::model::AgentDisplayState;
use chrono::{Local, NaiveDate};
use futures_util::StreamExt;
use skyhook::identity::{HostName, SessionId};
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
    /// Its root agent's state, as its journaled turns leave it.
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

/// Which group a row sorts into, last first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Group {
    /// Viewed, unstarted and saved sessions.
    Rest,
    Activated,
    Current,
}

/// A session saved in the workspace, as its row shows it.
struct Saved {
    id: SessionId,
    host: HostName,
    last: i64,
    label: String,
    events: String,
    mode: String,
    state: SavedState,
}

impl Saved {
    /// Its label and columns, with its last activity shown as `time`.
    fn row(self, time: String, current_host: &HostName) -> (String, Vec<String>) {
        let host = if self.host == *current_host {
            self.host.to_string()
        } else {
            format!("{} · view-only", self.host)
        };
        (
            self.label,
            vec![self.id.to_string(), self.events, self.mode, time, host],
        )
    }
}

/// The current session first, then the other activated sessions, working ones
/// first, then every other open or saved session, each group newest first.
/// Activated rows are emphasized. Each row's columns, right to left by importance: when it was last active, its
/// mode, how many events it holds, and its id. Host identity remains visible longest.
async fn load_sessions(
    root: PathBuf,
    peers: Vec<Peer>,
    today: NaiveDate,
    identity: &Identity,
) -> Result<Vec<Item<SessionRef>>, String> {
    let saved = load_saved(&root, identity).await?;
    Ok(session_rows(saved, &peers, today, &identity.host))
}

fn session_rows(
    mut saved: Vec<Saved>,
    peers: &[Peer],
    today: NaiveDate,
    current_host: &HostName,
) -> Vec<Item<SessionRef>> {
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
                let columns = ["", "", "", "draft", current_host.as_str()].map(String::from);
                ("New session".to_owned(), columns.into(), Recency::Now)
            };
            let (label, columns, recency) = match peer.session {
                PeerSession::Draft => draft(),
                // It reads as a draft until its first record is observed.
                PeerSession::Started { id, .. } => {
                    take(id);
                    draft()
                }
                PeerSession::Open { id, last, .. } => {
                    let (label, columns) = match take(id) {
                        Some(row) => row.row(time(last), current_host),
                        None => {
                            let columns = [
                                id.to_string(),
                                String::new(),
                                String::new(),
                                time(last),
                                String::new(),
                            ];
                            ("New session".to_owned(), columns.into())
                        }
                    };
                    (label, columns, Recency::At(last))
                }
            };
            let activated = peer.session.activated();
            let emphasis = if activated {
                Emphasis::Strong
            } else {
                Emphasis::Normal
            };
            let item = Item::columned(SessionRef::Live(peer.key), label, "", columns);
            let group = match (peer.current, activated) {
                (true, _) => Group::Current,
                (false, true) => Group::Activated,
                (false, false) => Group::Rest,
            };
            ((group, peer.working, recency), item.emphasized(emphasis))
        })
        .collect();
    rows.extend(saved.into_iter().map(|row| {
        let (value, last) = (SessionRef::Saved(row.id, row.state), row.last);
        let (label, columns) = row.row(time(last), current_host);
        let item = Item::columned(value, label, "", columns);
        ((Group::Rest, false, Recency::At(last)), item)
    }));
    rows.sort_by_key(|(order, _)| std::cmp::Reverse(*order));
    rows.into_iter().map(|(_, item)| item).collect()
}

/// Sessions scanned at once.
const CONCURRENT_SCANS: usize = 4;

/// Every saved session in `root` as `identity` sees it, scanned a few at a time so
/// their reads overlap.
async fn load_saved(root: &Path, identity: &Identity) -> Result<Vec<Saved>, String> {
    let ids = SessionStore::ids(root).await.map_err(|e| e.to_string())?;
    let scans = futures_util::stream::iter(ids).map(|id| scan(root, id, identity));
    let sessions: Vec<_> = scans.buffer_unordered(CONCURRENT_SCANS).collect().await;
    Ok(sessions.into_iter().flatten().collect())
}

/// One saved session's row, read without opening it; a session whose summary
/// cannot be read, or that this product could not open as its own, is left out.
async fn scan(root: &Path, id: SessionId, identity: &Identity) -> Option<Saved> {
    let (summary, open) = tokio::join!(
        SessionStore::summary(root, id, Some(identity.owner())),
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
        host: summary.host,
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
        let identity = self.launch.identity.clone();
        let (peers, today) = (self.peers.clone(), self.clock.day());
        let tx = self.tx.clone();
        self.open("Switch session", MenuKind::Sessions(vec![]));
        let id = self.menu.as_ref().unwrap().id;
        tokio::spawn(async move {
            let result = load_sessions(root, peers, today, &identity).await;
            let _ = tx.send(Work::MenuLoaded(MenuLoaded::Sessions(id, result)));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The current session leads even while only viewed, then the other
    /// activated sessions, working first, then viewed, unstarted and saved ones,
    /// each group newest first; activated rows are bold.
    #[tokio::test]
    async fn switch_menu_pins_current_then_activated_sessions_then_orders_by_last_activity() {
        use crate::tui::app::{
            played::{ask, played},
            session::Activation::{Activated, Viewed},
            tests::draft_fixture,
        };
        use skyhook::job::JobState;
        let (_root, app) = draft_fixture().await;
        let root = app.launch.sessions.clone();
        // Only the saved rows' states need real sessions; the rest are open peers,
        // whose saved rows are stand-ins.
        let mut stores = vec![];
        for index in 2..5 {
            let (session, mut model) = played(&app.launch).await;
            // A prompt no answer followed resumes interrupted; its request, sent once
            // the prompt commits, stays open.
            let _unanswered = if index == 3 {
                drop(ask(&session, "unanswered"));
                Some(model.turn().await)
            } else {
                None
            };
            // Keep one session held open through the scan.
            if index != 4 {
                session.shutdown().await.unwrap();
                session.close().await.unwrap();
            }
            let last = session.store().observe().snapshot.last_activity.unwrap();
            stores.push((session, last));
        }
        let mut saved = load_saved(&root, &app.launch.identity).await.unwrap();
        assert_eq!(saved.len(), 3);
        for row in &saved {
            let (_, last) = (stores.iter())
                .find(|(session, _)| session.id() == row.id)
                .unwrap();
            assert_eq!(row.last, *last);
        }
        let ids: Vec<_> = (0..8)
            .map(|index| match index {
                2..5 => stores[index - 2].0.id(),
                _ => SessionId::generate().unwrap(),
            })
            .collect();
        saved.extend([0, 1, 5, 6, 7].map(|index| Saved {
            id: ids[index],
            host: "skyhook".parse().unwrap(),
            last: 0,
            label: String::new(),
            events: String::new(),
            mode: String::new(),
            state: SavedState::Closed(AgentDisplayState::Ready),
        }));
        for row in &mut saved {
            let index = ids.iter().position(|id| *id == row.id).unwrap();
            // Appends use wall time; row ordering must not depend on clock resolution.
            row.last = [5, 6, 20, 40, 30, 7, 8, 9][index];
        }
        // Another host's saved session is listed, labelled view-only.
        let foreign = saved.iter_mut().find(|row| row.id == ids[2]).unwrap();
        foreign.host = "other-host".parse().unwrap();
        let keys: Vec<_> = std::iter::successors(Some(SlotKey::default()), |key| Some(key.next()))
            .take(6)
            .collect();
        let peer = |key, session, working| Peer {
            key,
            session,
            current: key == keys[0],
            state: AgentDisplayState::Ready,
            attention: false,
            working,
        };
        let open = |id, last, activation| PeerSession::Open {
            id,
            last,
            activation,
        };
        // The current session is viewed, idle and oldest; an activated session is
        // older than every viewed and saved one; an unstarted draft is not
        // activated, one whose journal exists is. Open sessions are never
        // listed again as saved.
        let peers = vec![
            peer(keys[0], open(ids[0], 1, Viewed), false),
            peer(keys[1], open(ids[1], 0, Activated), true),
            peer(keys[2], PeerSession::Draft, false),
            peer(
                keys[3],
                PeerSession::Started {
                    id: ids[5],
                    activation: Activated,
                },
                false,
            ),
            peer(keys[4], open(ids[6], 2, Activated), false),
            peer(keys[5], open(ids[7], 50, Viewed), false),
        ];
        let today = Local::now().date_naive();
        let items = session_rows(saved, &peers, today, &"skyhook".parse().unwrap());
        let rows: Vec<_> = items.iter().map(|item| item.value).collect();
        let live = [keys[0], keys[1], keys[3], keys[4], keys[2], keys[5]].map(SessionRef::Live);
        assert_eq!(rows.len(), 9);
        assert!(rows[..6] == live);
        let saved = [
            SessionRef::Saved(
                ids[3],
                SavedState::Closed(AgentDisplayState::Job(JobState::Interrupted)),
            ),
            SessionRef::Saved(ids[4], SavedState::Locked),
            SessionRef::Saved(ids[2], SavedState::Closed(AgentDisplayState::Ready)),
        ];
        assert!(rows[6..] == saved);
        let emphasis: Vec<_> = items.iter().map(|item| item.emphasis).collect();
        let (normal, strong) = (Emphasis::Normal, Emphasis::Strong);
        let expected = [
            normal, strong, strong, strong, normal, normal, normal, normal, normal,
        ];
        assert_eq!(emphasis, expected);
        let time = |index: usize| items[index].columns[3].as_str();
        assert_eq!(time(0), local_time(1, today, &Local, Precision::Minutes));
        assert_eq!(items[0].columns[0], ids[0].to_string());
        assert!(time(2) == "draft" && time(4) == "draft");
        let host = |index: usize| items[index].columns.last().unwrap().as_str();
        assert_eq!(host(8), "other-host · view-only");
        assert_eq!(host(7), "skyhook");

        // Once activated, the current session is bold too.
        let mut peers = peers;
        peers[0].session = open(ids[0], 1, Activated);
        let items = session_rows(vec![], &peers, today, &"skyhook".parse().unwrap());
        assert!(items[0].value == live[0] && items[0].emphasis == strong);
        stores[2].0.shutdown().await.unwrap();
        stores[2].0.close().await.unwrap();
    }
}
