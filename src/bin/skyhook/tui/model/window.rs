//! What the viewed agent's tab has loaded of its history: pages read around the view
//! as `Work` and folded here with the updates after them, so drawing reads only memory.
//! Each tab holds one run of its list, loaded where its view is and let go far from it;
//! a view beyond the run reads there instead.

use super::entries::Dep;
use super::transcript::Transcript;
use super::{Gap, Tab};
use skyhook::identity::{AgentId, JobId};
use skyhook::job::{JobEnvelope, JobRole};
use skyhook::session::{
    EventRecord, Fact, Page, RequestRecord, RequestSeq, SessionEvent, TranscriptPage,
};
use std::collections::{HashMap, HashSet};
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;

/// Records a transcript page reads.
pub const PAGE: usize = 64;
/// Requests or jobs a list page reads.
pub const LIST_PAGE: usize = 128;
/// Children a card reads at a time.
pub const CHILD_PAGE: usize = 16;
/// Records, requests or jobs kept loaded besides those in view, half on each side;
/// beyond them, those farthest from the view are let go.
pub const KEPT: usize = 8 * PAGE;

/// Which side of the view a load fills. Each side has at most one load in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

/// A load a window asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    Transcript(Page<u64>),
    Requests(Page<RequestSeq>),
    Roots(Page<JobId>),
    /// Parents' children, each after its key, or its first.
    Children(Vec<(JobId, Option<JobId>)>),
}

/// A page of a parent's children, read after `after`, and whether more follow.
pub struct ChildPage {
    pub parent: JobId,
    pub after: Option<JobId>,
    pub jobs: Vec<JobEnvelope>,
    pub more: bool,
}

/// What a load read: its own part, the jobs it shows, and the first children of each
/// script among them.
pub struct Loaded {
    pub read: Read,
    pub jobs: Vec<JobEnvelope>,
    pub children: Vec<ChildPage>,
}

/// What a load read of its own kind.
pub enum Read {
    Transcript(Page<u64>, Box<TranscriptPage>),
    Requests(Page<RequestSeq>, Vec<(RequestSeq, RequestRecord)>),
    /// The roots are the jobs it read.
    Roots(Page<JobId>),
    Children,
}

/// What is not loaded near the view, as drawing found it, and whether it lies above the
/// view's top.
pub struct Near {
    pub gap: Gap,
    pub above: bool,
}

/// Consecutive keys read a page at a time, those kept, and whether more lie past
/// either end.
pub struct List<K> {
    run: Option<Run<K>>,
    pub earlier: bool,
    pub later: bool,
    /// The newest key the updates made, loaded or not.
    newest: Option<K>,
}

/// The keys read, from `first` to `last`, kept or passed, and those kept.
struct Run<K> {
    first: K,
    last: K,
    kept: Vec<K>,
}

impl<K> Default for List<K> {
    /// Nothing read yet: whatever exists lies past the end.
    fn default() -> Self {
        Self {
            run: None,
            earlier: false,
            later: true,
            newest: None,
        }
    }
}

impl<K: Ord + Copy> List<K> {
    /// None to read: every key will come as it is made.
    fn complete() -> Self {
        Self {
            later: false,
            ..Self::default()
        }
    }

    /// Add what `page` read in order, each key kept or passed, `full` when it read as
    /// many as it asked for, unless it was read for an end that has moved since; a page
    /// from an end reads the list afresh.
    pub(super) fn insert(
        &mut self,
        page: Page<K>,
        read: impl IntoIterator<Item = (K, bool)>,
        full: bool,
    ) -> bool {
        let ends = self.run.as_ref().map(|run| (run.first, run.last));
        let empty = ends.is_none();
        match page {
            Page::First => (self.run, self.earlier, self.later) = (None, false, full),
            Page::Last => (self.run, self.earlier, self.later) = (None, full, false),
            Page::Before(key) if ends.is_none_or(|(first, _)| first == key) => {
                (self.earlier, self.later) = (full, self.later || empty);
            }
            Page::After(key) if ends.is_none_or(|(_, last)| last == key) => {
                (self.earlier, self.later) = (self.earlier || empty, full);
            }
            _ => return false,
        }
        let read: Vec<_> = read.into_iter().collect();
        if let (Some(&(start, _)), Some(&(end, _))) = (read.first(), read.last()) {
            let kept = read.iter().filter_map(|&(key, kept)| kept.then_some(key));
            match &mut self.run {
                None => {
                    self.run = Some(Run {
                        first: start,
                        last: end,
                        kept: kept.collect(),
                    });
                }
                Some(run) => {
                    (run.first, run.last) = (run.first.min(start), run.last.max(end));
                    match page {
                        Page::Before(_) => drop(run.kept.splice(0..0, kept)),
                        _ => run.kept.extend(kept),
                    }
                }
            }
        }
        // A key made after the read lies past what it read, however little it read.
        self.later |= self.newest > self.run.as_ref().map(|run| run.last);
        true
    }

    /// Add a key the updates made, at the end, while the end is loaded: kept, or passed.
    /// Returns whether it was kept.
    pub(super) fn push(&mut self, key: K, kept: bool) -> bool {
        self.newest = self.newest.max(Some(key));
        let added = !self.later && self.run.as_ref().is_none_or(|run| run.last < key);
        if added {
            let run = self.run.get_or_insert_with(|| Run {
                first: key,
                last: key,
                kept: Vec::new(),
            });
            run.last = key;
            if kept {
                run.kept.push(key);
            }
        }
        added && kept
    }

    /// The keys kept, in order.
    pub fn kept(&self) -> &[K] {
        self.run.as_ref().map_or(&[], |run| &run.kept)
    }

    /// The first through the last key read, kept or passed.
    pub fn extent(&self) -> Option<RangeInclusive<K>> {
        self.run.as_ref().map(|run| run.first..=run.last)
    }

    /// Whether `key` lies among the keys read, kept or passed.
    pub(super) fn holds(&self, key: K) -> bool {
        self.extent().is_some_and(|extent| extent.contains(&key))
    }

    /// Where `key` falls among the keys loaded.
    fn at(&self, key: K) -> usize {
        self.kept().partition_point(|k| *k < key)
    }

    /// Whether `key` lies within what is read, or past an end nothing lies beyond.
    fn reaches(&self, key: K) -> bool {
        let Some(run) = &self.run else {
            return !self.earlier && !self.later;
        };
        (!self.earlier || run.first <= key) && (!self.later || key <= run.last)
    }

    /// Let go of the keys beyond what is kept around the view's `shown` ones.
    pub(super) fn trim(&mut self, shown: Range<usize>) -> bool {
        let Some(run) = &mut self.run else {
            return false;
        };
        let before = shown.start.saturating_sub(KEPT / 2);
        let after = (run.kept.len() - shown.end).saturating_sub(KEPT / 2);
        if before + after == 0 {
            return false;
        }
        run.kept.drain(..before);
        run.kept.truncate(run.kept.len() - after);
        // What lies past the keys kept is read again.
        if let (Some(&start), Some(&end)) = (run.kept.first(), run.kept.last()) {
            if before > 0 {
                run.first = start;
            }
            if after > 0 {
                run.last = end;
            }
        }
        self.earlier |= before > 0;
        self.later |= after > 0;
        true
    }

    /// The page of up to `page` keys beyond the end `gap` stands for, while more lie
    /// there and trimming around the view's `shown` ones would keep what it reads.
    fn want(&self, gap: Gap, shown: Range<usize>, page: usize) -> Option<Page<K>> {
        let kept = |beyond: usize| beyond + page <= KEPT / 2;
        let run = self.run.as_ref()?;
        match gap {
            Gap::Earlier if self.earlier && kept(shown.start) => Some(Page::Before(run.first)),
            Gap::Later if self.later && kept(run.kept.len() - shown.end) => {
                Some(Page::After(run.last))
            }
            _ => None,
        }
    }
}

pub enum Content {
    Conversation(Box<Transcript>),
    Requests(List<RequestSeq>),
    Jobs(List<JobId>),
}

impl Content {
    fn of(tab: Tab) -> Self {
        match tab {
            Tab::Conversation => Self::Conversation(Box::default()),
            Tab::Requests => Self::Requests(List::default()),
            Tab::Jobs => Self::Jobs(List::default()),
        }
    }

    /// How many keys are loaded.
    fn len(&self) -> usize {
        match self {
            Self::Conversation(transcript) => transcript.run.kept().len(),
            Self::Requests(list) => list.kept().len(),
            Self::Jobs(list) => list.kept().len(),
        }
    }

    /// Whether more lies before and after what is loaded.
    pub fn ends(&self) -> (bool, bool) {
        match self {
            Self::Conversation(transcript) => (transcript.run.earlier, transcript.run.later),
            Self::Requests(list) => (list.earlier, list.later),
            Self::Jobs(list) => (list.earlier, list.later),
        }
    }
}

/// One of a tab's own entries, by which its window pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Ordinal(u64),
    Request(RequestSeq),
    Job(JobId),
}

/// Where along a tab's own entries something lies: before them all, at one, or past
/// them all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge<T = Focus> {
    Start,
    At(T),
    End,
}

impl<T> Edge<T> {
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Edge<U> {
        match self {
            Self::Start => Edge::Start,
            Self::At(at) => Edge::At(f(at)),
            Self::End => Edge::End,
        }
    }
}

/// An update of the viewed agent that arrived before the first load landed: its
/// ordinal, what the live slice decided of it, and when its agent's open turn began.
type Early = (u64, Arc<EventRecord>, Option<Fact>, Option<i64>);

/// Whether the window has what it shows first.
enum Phase {
    /// The first load is in flight, and the updates since wait for it.
    Opening(Vec<Early>),
    Open,
    /// A load failed: nothing more is asked for.
    Failed,
}

/// The viewed agent's tab, loaded around the view.
pub struct Window {
    pub agent: AgentId,
    pub tab: Tab,
    /// Tells this window's loads from those of one it replaced.
    pub id: u64,
    pub content: Content,
    /// The jobs it shows, as their pages read them.
    jobs: HashMap<JobId, JobEnvelope>,
    /// The requests it shows, as their pages read them or as a newer request of the
    /// agent left them.
    requests: HashMap<RequestSeq, RequestRecord>,
    /// Each loaded parent's children.
    pub children: HashMap<JobId, List<JobId>>,
    phase: Phase,
    /// Whether each side has a load in flight.
    loading: [bool; 2],
    /// The entries the view last showed, from its top to its bottom, around which
    /// what is loaded stays.
    shown: RangeInclusive<Edge>,
    /// Changes whenever the places it holds change.
    pub shape: u64,
    changes: HashSet<Dep>,
    /// Whether what it shows changed since what is kept beside it was last let go.
    unpruned: bool,
}

impl Window {
    /// `agent`'s `tab`, its first load read around `focus`.
    pub fn new(agent: AgentId, tab: Tab, id: u64, focus: Edge) -> Self {
        Self {
            agent,
            tab,
            id,
            content: Content::of(tab),
            jobs: HashMap::new(),
            requests: HashMap::new(),
            children: HashMap::new(),
            phase: Phase::Opening(Vec::new()),
            loading: [false; 2],
            shown: focus..=focus,
            shape: 0,
            changes: HashSet::new(),
            unpruned: false,
        }
    }

    /// A window with nothing to read: a draft's.
    pub fn empty(agent: AgentId, tab: Tab, id: u64) -> Self {
        let mut window = Self::new(agent, tab, id, Edge::End);
        window.phase = Phase::Open;
        match &mut window.content {
            Content::Conversation(transcript) => transcript.run = List::complete(),
            Content::Requests(list) => *list = List::complete(),
            Content::Jobs(list) => *list = List::complete(),
        }
        window
    }

    /// `agent`'s whole transcript as `pages` read it in order, for an export.
    pub fn whole(agent: AgentId, id: u64, pages: Vec<Loaded>) -> Self {
        let mut window = Self::empty(agent, Tab::Conversation, id);
        for loaded in pages {
            window.add(loaded);
        }
        window
    }

    /// The conversation's transcript, while the conversation shows.
    pub fn transcript(&self) -> Option<&Transcript> {
        match &self.content {
            Content::Conversation(transcript) => Some(transcript),
            _ => None,
        }
    }

    /// A job as its page read it.
    pub fn job(&self, job: &JobId) -> Option<&JobEnvelope> {
        self.jobs.get(job)
    }

    /// Every job its pages read.
    pub fn jobs(&self) -> impl Iterator<Item = JobId> + '_ {
        self.jobs.keys().copied()
    }

    /// A request as its page read it, or as a newer one left it.
    pub fn request(&self, request: RequestSeq) -> Option<&RequestRecord> {
        self.requests.get(&request)
    }

    /// What changed of what it shows since the last call.
    pub fn take_changes(&mut self) -> HashSet<Dep> {
        std::mem::take(&mut self.changes)
    }

    /// Whether its first load is still in flight.
    pub fn opening(&self) -> bool {
        matches!(self.phase, Phase::Opening(_))
    }

    /// Whether a load is in flight.
    pub fn loading(&self) -> bool {
        self.loading.contains(&true)
    }

    /// Let go of the jobs, children, requests and facts nothing kept shows, once what
    /// it shows or what is kept beside it (`grown`) changed and no load in flight may
    /// yet show them; returns the jobs it shows, to which what is kept for jobs beside
    /// it may be let go of.
    pub fn take_pruned(&mut self, grown: bool) -> Option<HashSet<JobId>> {
        self.unpruned |= grown;
        if !self.unpruned || self.loading() {
            return None;
        }
        self.unpruned = false;
        let shown = self.shown();
        self.children.retain(|parent, _| shown.contains(parent));
        self.jobs.retain(|job, _| shown.contains(job));
        let requests: HashSet<_> = match &mut self.content {
            Content::Conversation(transcript) => {
                transcript.retain_facts();
                transcript.shown_requests()
            }
            Content::Requests(list) => list.kept().iter().copied().collect(),
            Content::Jobs(_) => HashSet::new(),
        };
        self.requests
            .retain(|request, _| requests.contains(request));
        Some(shown)
    }

    /// The loads what is not loaded `near` the view needs: for each side with none in
    /// flight, the nearest the view, within what is kept around the entries `shown`.
    /// The first load comes before any other, and reads from the focus the window was
    /// made for.
    pub fn plan(&mut self, near: &[Near], shown: RangeInclusive<Edge>) -> Vec<(Direction, Want)> {
        match self.phase {
            Phase::Failed => return Vec::new(),
            Phase::Opening(_) => {
                if self.loading() {
                    return Vec::new();
                }
                let want = match (self.tab, *self.shown.start()) {
                    (Tab::Conversation, Edge::At(Focus::Ordinal(ordinal))) => {
                        Want::Transcript(following(ordinal.checked_sub(1)))
                    }
                    (Tab::Requests, Edge::At(Focus::Request(request))) => {
                        Want::Requests(following(request.preceding()))
                    }
                    (Tab::Jobs, Edge::At(Focus::Job(job))) => {
                        Want::Roots(following(JobId::new(job.get() - 1).ok()))
                    }
                    (Tab::Conversation, Edge::Start) => Want::Transcript(Page::First),
                    (Tab::Requests, Edge::Start) => Want::Requests(Page::First),
                    (Tab::Jobs, Edge::Start) => Want::Roots(Page::First),
                    (Tab::Conversation, _) => Want::Transcript(Page::Last),
                    (Tab::Requests, _) => Want::Requests(Page::Last),
                    (Tab::Jobs, _) => Want::Roots(Page::Last),
                };
                self.loading[Direction::Down as usize] = true;
                return vec![(Direction::Down, want)];
            }
            Phase::Open => self.shown = shown,
        }
        let mut wants = Vec::new();
        for direction in [Direction::Up, Direction::Down] {
            if self.loading[direction as usize] {
                continue;
            }
            let mut side: Vec<_> = (near.iter())
                .filter(|near| near.above == (direction == Direction::Up))
                .collect();
            // Nearest the view first: the last gap above it, the first below.
            if direction == Direction::Up {
                side.reverse();
            }
            let mut side = side.into_iter();
            let mut want = side.find_map(|near| self.want(near.gap));
            // Every card on this side with children left to read reads them in one load.
            if let Some(Want::Children(parents)) = &mut want {
                let more = side.filter_map(|near| match near.gap {
                    Gap::More(parent) => Some((parent, self.children_after(parent)?)),
                    _ => None,
                });
                parents.extend(more);
            }
            if let Some(want) = want {
                self.loading[direction as usize] = true;
                wants.push((direction, want));
            }
        }
        wants
    }

    fn want(&self, gap: Gap) -> Option<Want> {
        let at = self.in_view();
        Some(match (gap, &self.content) {
            (Gap::More(parent), _) => Want::Children(vec![(parent, self.children_after(parent)?)]),
            (gap, Content::Conversation(transcript)) => {
                Want::Transcript(transcript.run.want(gap, at, PAGE)?)
            }
            (gap, Content::Requests(list)) => Want::Requests(list.want(gap, at, LIST_PAGE)?),
            (gap, Content::Jobs(list)) => Want::Roots(list.want(gap, at, LIST_PAGE)?),
        })
    }

    /// Where what the view shows falls on what is loaded.
    fn in_view(&self) -> Range<usize> {
        let start = self.at(*self.shown.start());
        let end = match *self.shown.end() {
            Edge::At(bottom) => self.at(Edge::At(bottom)) + 1,
            edge => self.at(edge),
        };
        start..end.clamp(start, self.content.len())
    }

    /// Where `edge` falls on what is loaded.
    fn at(&self, edge: Edge) -> usize {
        match (&self.content, edge) {
            (Content::Conversation(transcript), Edge::At(Focus::Ordinal(ordinal))) => {
                transcript.run.at(ordinal)
            }
            (Content::Requests(list), Edge::At(Focus::Request(request))) => list.at(request),
            (Content::Jobs(list), Edge::At(Focus::Job(job))) => list.at(job),
            (_, Edge::Start) => 0,
            (content, _) => content.len(),
        }
    }

    /// Whether the window read `focus`, whether it shows anything or not.
    pub fn holds(&self, focus: Focus) -> bool {
        match (&self.content, focus) {
            (Content::Conversation(transcript), Focus::Ordinal(ordinal)) => {
                transcript.run.holds(ordinal)
            }
            (Content::Requests(list), Focus::Request(request)) => list.holds(request),
            (Content::Jobs(list), Focus::Job(job)) => list.holds(job),
            _ => false,
        }
    }

    /// Whether `focus` lies beyond what an open window loaded, whatever it has in
    /// flight: a window made there reads it instead. An opening window's extent is not
    /// known yet.
    pub fn beyond(&self, focus: Edge) -> bool {
        if !matches!(self.phase, Phase::Open) {
            return false;
        }
        match (&self.content, focus) {
            (content, Edge::Start) => content.ends().0,
            (content, Edge::End) => content.ends().1,
            (Content::Conversation(transcript), Edge::At(Focus::Ordinal(ordinal))) => {
                !transcript.run.reaches(ordinal)
            }
            (Content::Requests(list), Edge::At(Focus::Request(request))) => !list.reaches(request),
            (Content::Jobs(list), Edge::At(Focus::Job(job))) => !list.reaches(job),
            _ => false,
        }
    }

    /// Where `parent`'s children are left to read from, if any are.
    fn children_after(&self, parent: JobId) -> Option<Option<JobId>> {
        match self.children.get(&parent) {
            Some(children) if !children.later => None,
            children => Some(children.and_then(|children| children.kept().last().copied())),
        }
    }

    /// A load on `direction` failed: nothing more is asked for.
    pub fn fail(&mut self, direction: Direction) {
        self.loading[direction as usize] = false;
        self.phase = Phase::Failed;
        self.unpruned = true;
    }

    /// Fold what the load on `direction` read, then the updates that arrived before
    /// the first load, and let go of what lies far from the view. A first load that
    /// found nothing from its focus on reads the end instead.
    pub fn loaded(&mut self, direction: Direction, loaded: Loaded) {
        self.loading[direction as usize] = false;
        self.add(loaded);
        if let Phase::Opening(early) = &mut self.phase {
            let empty = match &self.content {
                Content::Conversation(transcript) => transcript.run.extent().is_none(),
                Content::Requests(list) => list.extent().is_none(),
                Content::Jobs(list) => list.extent().is_none(),
            };
            if *self.shown.start() != Edge::End && empty {
                self.shown = Edge::End..=Edge::End;
                return;
            }
            let early = std::mem::take(early);
            self.phase = Phase::Open;
            for (ordinal, record, fact, turn) in early {
                self.observe(ordinal, &record, fact, turn);
            }
        }
        self.trim();
        self.unpruned = true;
        self.shape += 1;
    }

    fn add(&mut self, loaded: Loaded) {
        let Loaded {
            read,
            jobs,
            children,
        } = loaded;
        // What the updates since the read left stays.
        let read_jobs = |jobs: Vec<JobEnvelope>, held: &mut HashMap<_, _>| {
            for job in jobs {
                held.entry(job.id).or_insert(job);
            }
        };
        match (&mut self.content, read) {
            (Content::Conversation(transcript), Read::Transcript(page, read)) => {
                for (request, record) in transcript.place(page, *read) {
                    self.requests.entry(request).or_insert(record);
                }
            }
            (Content::Requests(list), Read::Requests(page, read)) => {
                let full = read.len() == LIST_PAGE;
                list.insert(page, read.iter().map(|(request, _)| (*request, true)), full);
                for (request, record) in read {
                    self.requests.entry(request).or_insert(record);
                }
            }
            (Content::Jobs(list), Read::Roots(page)) => {
                let full = jobs.len() == LIST_PAGE;
                list.insert(page, jobs.iter().map(|job| (job.id, true)), full);
            }
            _ => {}
        }
        read_jobs(jobs, &mut self.jobs);
        for page in children {
            let ChildPage {
                parent,
                after,
                jobs,
                more,
            } = page;
            let list = self.children.entry(parent).or_default();
            list.insert(
                after.map_or(Page::First, Page::After),
                jobs.iter().map(|job| (job.id, true)),
                more,
            );
            read_jobs(jobs, &mut self.jobs);
        }
    }

    /// Let go of what lies farthest from the view beyond what is kept.
    fn trim(&mut self) {
        let at = self.in_view();
        let trimmed = match &mut self.content {
            Content::Conversation(transcript) => transcript.trim(at),
            Content::Requests(list) => list.trim(at),
            Content::Jobs(list) => list.trim(at),
        };
        self.unpruned |= trimmed;
    }

    /// The jobs its places show, with every child of a parent shown, and theirs.
    fn shown(&self) -> HashSet<JobId> {
        let mut shown = match &self.content {
            Content::Conversation(transcript) => transcript.shown_jobs(),
            Content::Jobs(list) => list.kept().iter().copied().collect(),
            Content::Requests(_) => HashSet::new(),
        };
        let mut parents: Vec<_> = shown.iter().copied().collect();
        while let Some(parent) = parents.pop() {
            for child in self
                .children
                .get(&parent)
                .into_iter()
                .flat_map(|c| c.kept())
            {
                if shown.insert(*child) {
                    parents.push(*child);
                }
            }
        }
        shown
    }

    /// Fold an update's record of the viewed agent, its entry `ordinal`, which the live
    /// slice decided `fact` of, its open turn having begun at `turn`.
    pub fn observe(
        &mut self,
        ordinal: u64,
        record: &Arc<EventRecord>,
        fact: Option<Fact>,
        turn: Option<i64>,
    ) {
        if let Phase::Opening(early) = &mut self.phase {
            early.push((ordinal, record.clone(), fact, turn));
            return;
        }
        // A load in flight may read the records this one concerns.
        let reading = self.loading();
        let shaped = match (&mut self.content, &record.event) {
            (Content::Conversation(transcript), _) => {
                let (placed, call) = transcript.observe(ordinal, record, (fact, turn), reading);
                self.changes.extend(call);
                placed
            }
            (Content::Requests(list), SessionEvent::ModelRequested { .. }) => {
                list.push(record.sequence.request(), true)
            }
            (
                Content::Jobs(list),
                SessionEvent::JobCreated {
                    job,
                    parent,
                    origin,
                    ..
                },
            ) if !skyhook::job::nests(*parent, origin) => list.push(*job, true),
            _ => false,
        };
        let mut nested = false;
        if let SessionEvent::JobCreated {
            job,
            parent,
            origin,
            role,
            ..
        } = &record.event
        {
            if skyhook::job::nests(*parent, origin)
                && let Some(parent) = parent
            {
                // A load in flight may have read its parent's children before it was
                // made: a list for it, left to read past them, survives the landing.
                let children = match reading {
                    true => Some(self.children.entry(*parent).or_default()),
                    false => self.children.get_mut(parent),
                };
                nested = children.is_some_and(|children| children.push(*job, true));
            }
            // A new script whose card is loaded has no children it has not issued since.
            let card = match &self.content {
                Content::Conversation(transcript) => transcript.record(ordinal).is_some(),
                _ => shaped,
            };
            if *role == JobRole::Script && (card || nested) {
                self.children.entry(*job).or_insert_with(List::complete);
            }
        }
        if shaped {
            self.trim();
        }
        if shaped || nested {
            self.shape += 1;
        }
    }

    /// Keep the request a newer one of the agent replaced as it left it, while it
    /// shows it.
    pub fn superseded(&mut self, request: RequestSeq, record: &RequestRecord) {
        self.requests.insert(request, record.clone());
        self.changes.insert(Dep::Request(request));
        self.unpruned = true;
    }
}

/// The page from the key after `before`, or the first.
fn following<K>(before: Option<K>) -> Page<K> {
    before.map_or(Page::First, Page::After)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::bounded;
    use crate::tui::app::tests::fixture;
    use skyhook::agent::SessionHandle;

    /// What the root's transcript `page` reads.
    async fn read(session: &SessionHandle, page: Page<u64>) -> Loaded {
        let transcript = session.store().transcript(session.root_agent(), page, PAGE);
        Loaded {
            read: Read::Transcript(page, Box::new(bounded(transcript).await.unwrap())),
            jobs: Vec::new(),
            children: Vec::new(),
        }
    }

    /// The transcript page `window`, viewed at `focus`, asks for first or past `gap`.
    fn want(window: &mut Window, gap: Option<Gap>, focus: Edge) -> (Direction, Page<u64>) {
        let near: Vec<_> = (gap.into_iter())
            .map(|gap| Near {
                gap,
                above: gap == Gap::Earlier,
            })
            .collect();
        match window.plan(&near, focus..=focus).pop() {
            Some((direction, Want::Transcript(page))) => (direction, page),
            want => panic!("{want:?}"),
        }
    }

    /// The ordinals loaded, and whether they follow one another.
    fn run(window: &Window) -> (&[u64], bool) {
        let keys = window.transcript().unwrap().run.kept();
        (keys, keys.windows(2).all(|pair| pair[1] == pair[0] + 1))
    }

    /// A page read while the first of a parent's children is made leaves them to read,
    /// however few it read.
    #[test]
    fn a_page_read_while_a_child_is_made_leaves_it_to_read() {
        let mut children = List::default();
        children.push(JobId::new(2).unwrap(), true);
        children.insert(Page::First, [], false);
        assert!(children.later);
    }

    /// Pages landing late never leave a hole in the run: one read for an end that a trim
    /// has moved since is dropped, and a record committed between a page's read and
    /// its landing is read next.
    #[tokio::test]
    async fn pages_landing_late_keep_the_run_whole() {
        let (_root, app) = fixture().await;
        let session = app.session().unwrap().clone();
        let agent = session.root_agent().clone();
        let status = async |text: String| {
            bounded(session.record_status(agent.clone(), text))
                .await
                .unwrap();
            let last = session.store().transcript(&agent, Page::Last, 1);
            let last = bounded(last).await.unwrap();
            (last.first, last.records[0].clone())
        };
        let statuses = (0..KEPT + 3 * PAGE)
            .map(|status| session.record_status(agent.clone(), format!("status {status}")));
        bounded(futures_util::future::try_join_all(statuses))
            .await
            .unwrap();
        let mut focus = Edge::At(Focus::Ordinal(PAGE as u64));
        let mut window = Window::new(agent.clone(), Tab::Conversation, 1, focus);
        let (direction, first) = want(&mut window, None, focus);
        window.loaded(direction, read(&session, first).await);
        let (up, earlier) = want(&mut window, Some(Gap::Earlier), focus);
        let stale = read(&session, earlier).await;
        // The view moves down until the run's start is let go, then back up to it.
        while run(&window).0[0] == PAGE as u64 {
            focus = Edge::At(Focus::Ordinal(run(&window).0.last().unwrap() - 10));
            let (down, later) = want(&mut window, Some(Gap::Later), focus);
            window.loaded(down, read(&session, later).await);
        }
        let top = Edge::At(Focus::Ordinal(run(&window).0[0] + 6));
        window.plan(&[], top..=top);
        window.loaded(up, stale);
        assert!(run(&window).1, "{:?}", run(&window).0);
        // At the end, a record commits after the last page's read, before it lands.
        let (down, last) = loop {
            focus = Edge::At(Focus::Ordinal(run(&window).0.last().unwrap() - 10));
            let (down, later) = want(&mut window, Some(Gap::Later), focus);
            let page = read(&session, later).await;
            match &page.read {
                Read::Transcript(_, read) if read.records.len() < PAGE => break (down, page),
                _ => window.loaded(down, page),
            }
        };
        let (ordinal, record) = status("after the read".into()).await;
        window.observe(ordinal, &record, None, None);
        window.loaded(down, last);
        let (down, rest) = want(&mut window, Some(Gap::Later), focus);
        window.loaded(down, read(&session, rest).await);
        let (keys, whole) = run(&window);
        assert!(whole && keys.last() == Some(&ordinal), "{keys:?}");
        session.shutdown().await.unwrap();
    }
}
