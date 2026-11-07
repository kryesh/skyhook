//! A structured page an extension view supplies. The host lays it out with its own theme,
//! width, reflow, disclosure and scrolling; a product only describes what it shows.
use std::collections::BTreeMap;

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
    pub detail: Option<RecordDetail>,
}

/// An open record's detail, as far as the viewer has extended its continued entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordDetail {
    pub lines: Vec<Detail>,
    /// Its continued entries go on past those shown; each "more" shows
    /// [`Disclosed::PAGE`] more.
    pub more: bool,
}

impl From<Vec<Detail>> for RecordDetail {
    fn from(lines: Vec<Detail>) -> Self {
        Self { lines, more: false }
    }
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
    pub weight: Weight,
}

impl Cell {
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
            weight: Weight::Regular,
        }
    }

    /// The same cell, drawn strong.
    #[must_use]
    pub fn strong(self) -> Self {
        Self {
            weight: Weight::Strong,
            ..self
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
    /// Between a warning and an error.
    Caution,
    Error,
}

/// How heavily a cell is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Weight {
    #[default]
    Regular,
    Strong,
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
    /// Each open record with how many continued entries its detail shows.
    records: BTreeMap<RecordKey, usize>,
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

    /// How many continued entries an open record's detail shows; none while it is closed.
    pub fn detail(&self, section: SectionKey, record: &RecordKey) -> Option<usize> {
        match &self.0 {
            Disclosure::Shown(sections) => sections.get(&section)?.records.get(record).copied(),
            Disclosure::Whole => None,
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
                        records: BTreeMap::new(),
                    };
                    sections.insert(*section, opened);
                }
            }
            Toggle::Record(section, record) => {
                if let Some(opened) = sections.get_mut(section)
                    && opened.records.remove(record).is_none()
                {
                    opened.records.insert(record.clone(), Self::PAGE);
                }
            }
            Toggle::More(section) => {
                if let Some(opened) = sections.get_mut(section) {
                    opened.limit = opened.limit.saturating_add(Self::PAGE);
                }
            }
            Toggle::MoreDetail(section, record) => {
                if let Some(shown) =
                    (sections.get_mut(section)).and_then(|opened| opened.records.get_mut(record))
                {
                    *shown = shown.saturating_add(Self::PAGE);
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
    /// More of an open record's continued detail entries.
    MoreDetail(SectionKey, RecordKey),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record opens only within its open section, and closing the section closes it; its
    /// detail's continued entries extend apart from the section's records.
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
        assert_eq!(open.detail(ITEMS, &key), Some(Disclosed::PAGE));
        open.apply(&Toggle::MoreDetail(ITEMS, key.clone()));
        assert_eq!(open.detail(ITEMS, &key), Some(2 * Disclosed::PAGE));
        assert_eq!(open.limit(ITEMS), Some(Disclosed::PAGE));
        open.apply(&Toggle::Section(ITEMS));
        open.apply(&Toggle::Section(ITEMS));
        assert_eq!(open.detail(ITEMS, &key), None);
    }
}
