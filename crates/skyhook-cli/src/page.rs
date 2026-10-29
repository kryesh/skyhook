//! A structured page an extension view supplies. The host lays it out with its own theme,
//! width, reflow, disclosure and scrolling; a product only describes what it shows.
use std::collections::{BTreeMap, BTreeSet};

/// How an extension view shares the frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewFrame {
    /// Inside the session's chrome: sidebar, live agents and composer stay.
    #[default]
    Session,
    /// The page takes the frame below the tab strip; the footer stays, and so do the
    /// live agents when `agents` is [`AgentPane::Live`].
    Page { agents: AgentPane },
}

/// Whether a page view keeps the session's live agents beside or below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentPane {
    Hidden,
    Live,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// `label value` pairs on one line, wrapping between pairs.
    Facts(Vec<Fact>),
    /// Stages in order, joined left to right, wrapping between stages.
    Pipeline(Vec<Stage>),
    /// Boxed panels side by side as the width allows, stacking when it does not.
    Panels(Vec<Panel>),
    /// A collapsible table of records, each of which opens to its detail.
    Section(Section),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    pub label: String,
    pub value: Cell,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stage {
    pub label: String,
    pub value: String,
    pub state: StageState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageState {
    Done,
    Active,
    Pending,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Panel {
    pub title: String,
    /// Headers of the value columns, right-aligned over them; empty for none.
    pub columns: Vec<String>,
    pub rows: Vec<PanelRow>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelRow {
    pub label: String,
    pub values: Vec<Cell>,
    pub meter: Option<Meter>,
}

/// A bar filled to `value` of `total`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meter {
    pub value: u64,
    pub total: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub key: SectionKey,
    pub title: String,
    pub summary: Option<String>,
    /// Records the section holds; `records` are the first of them while it is open.
    pub total: u64,
    pub columns: Vec<Column>,
    pub records: Vec<Record>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub title: String,
    /// Columns that do not fit are dropped, the highest priority number first; the last
    /// column takes what width remains.
    pub priority: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub key: RecordKey,
    pub cells: Vec<Cell>,
    /// Supplied only while the record is open.
    pub detail: Option<Vec<Detail>>,
}

/// One labelled line of a record's detail; a line without a label continues the one above.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detail {
    pub label: Option<String>,
    pub cells: Vec<Cell>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub tone: Tone,
}

impl Cell {
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

impl<T: Into<String>> From<T> for Cell {
    fn from(text: T) -> Self {
        Self::new(text, Tone::Normal)
    }
}

/// What a cell means; the host picks its colour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tone {
    #[default]
    Normal,
    Muted,
    Accent,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SectionKey(pub &'static str);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordKey(pub String);

/// What the viewer has opened, which the host keeps and passes to every refresh so the
/// product reads only what is shown. A record is opened within its open section, so closing
/// the section closes it and a closed section holds none.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Disclosed(Disclosure);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Disclosure {
    Shown(BTreeMap<SectionKey, Opened>),
    /// Every section open with every record it holds, for a page shown once and whole.
    Whole,
}

impl Default for Disclosure {
    fn default() -> Self {
        Self::Shown(BTreeMap::new())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Opened {
    limit: usize,
    records: BTreeSet<RecordKey>,
}

impl Disclosed {
    /// Records an opened section shows at first, and each "more" adds.
    pub const PAGE: usize = 50;

    /// Every section open with every record it holds, for a page shown once and whole.
    pub fn whole() -> Self {
        Self(Disclosure::Whole)
    }

    /// How many records to supply for an open section.
    pub fn limit(&self, section: SectionKey) -> Option<usize> {
        match &self.0 {
            Disclosure::Shown(sections) => sections.get(&section).map(|opened| opened.limit),
            Disclosure::Whole => Some(usize::MAX),
        }
    }

    pub fn is_open(&self, section: SectionKey, record: &RecordKey) -> bool {
        match &self.0 {
            Disclosure::Shown(sections) => {
                (sections.get(&section)).is_some_and(|opened| opened.records.contains(record))
            }
            Disclosure::Whole => false,
        }
    }

    /// Open or close what `toggle` names; a record or "more" of a closed section is nothing,
    /// and a whole page opens nothing more.
    pub fn apply(&mut self, toggle: &Toggle) {
        let Disclosure::Shown(sections) = &mut self.0 else {
            return;
        };
        match toggle {
            Toggle::Section(section) => {
                if sections.remove(section).is_none() {
                    let opened = Opened {
                        limit: Self::PAGE,
                        records: BTreeSet::new(),
                    };
                    sections.insert(*section, opened);
                }
            }
            Toggle::Record(section, record) => {
                if let Some(opened) = sections.get_mut(section)
                    && !opened.records.remove(record)
                {
                    opened.records.insert(record.clone());
                }
            }
            Toggle::More(section) => {
                if let Some(opened) = sections.get_mut(section) {
                    opened.limit = opened.limit.saturating_add(Self::PAGE);
                }
            }
        }
    }
}

/// Something on a page the viewer can open, close or extend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Toggle {
    Section(SectionKey),
    Record(SectionKey, RecordKey),
    More(SectionKey),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record opens only within its open section, and closing the section closes it.
    #[test]
    fn records_open_only_within_an_open_section() {
        const ITEMS: SectionKey = SectionKey("items");
        let key = RecordKey("1".to_owned());
        let record = Toggle::Record(ITEMS, key.clone());
        let mut open = Disclosed::default();
        open.apply(&record);
        assert_eq!(open, Disclosed::default());
        open.apply(&Toggle::Section(ITEMS));
        open.apply(&record);
        assert!(open.is_open(ITEMS, &key));
        open.apply(&Toggle::Section(ITEMS));
        open.apply(&Toggle::Section(ITEMS));
        assert!(!open.is_open(ITEMS, &key));
    }
}
