//! Live nodes found by their own text and code locations, a merged node's counted toward the
//! node it was merged into.

use std::fmt;

use ignore::overrides::{Override, OverrideBuilder};
use libsql::Row;
use regex::{Regex, RegexBuilder};
use schemars::JsonSchema;
use serde::Serialize;
use serde_with::SerializeDisplay;
use skyhook::db::{enum_column, params};
use skyhook_base::enums::NamedEnum;
use skyhook_worker::op::REGEX_SIZE_LIMIT;

use super::{super::*, list::summary};

/// The hits a found node lists; it counts the rest.
const HITS: usize = 5;

/// A search its arguments cannot make.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SearchError {
    #[error("search by at least one of `pattern`, `file` and `function`")]
    Unfiltered,
    #[error("`line` is a line of a `file`: give the file")]
    LineWithoutFile,
    #[error("`{0}` is not a regular expression this search compiles")]
    Regex(&'static str),
    #[error("`file` is not a glob")]
    Glob,
}

/// Text matching `pattern` and a code location in `site`, on live nodes of `kinds`.
pub(crate) struct Search {
    pattern: Option<Regex>,
    site: Option<Site>,
    kinds: Vec<NodeKind>,
}

/// A code location in a file matching `file`, in a function matching `function`.
struct Site {
    file: Option<FileSite>,
    function: Option<Regex>,
}

/// A file matching `glob`, at a range holding `line`.
struct FileSite {
    glob: Override,
    line: Option<u32>,
}

fn regex(argument: &'static str, pattern: &str) -> std::result::Result<Regex, SearchError> {
    RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|_| SearchError::Regex(argument))
}

impl Search {
    /// A search by any of `pattern`, the `file` glob with a `line` in it and `function`, at
    /// least one given.
    pub(crate) fn new(
        pattern: Option<&str>,
        file: Option<&str>,
        line: Option<u32>,
        function: Option<&str>,
        kinds: Vec<NodeKind>,
    ) -> std::result::Result<Self, SearchError> {
        let file = match (file, line) {
            (None, Some(_)) => return Err(SearchError::LineWithoutFile),
            (None, None) => None,
            (Some(glob), line) => {
                let mut builder = OverrideBuilder::new("");
                builder.add(glob).map_err(|_| SearchError::Glob)?;
                let glob = builder.build().map_err(|_| SearchError::Glob)?;
                Some(FileSite { glob, line })
            }
        };
        let function = function
            .map(|pattern| regex("function", pattern))
            .transpose()?;
        let site = (file.is_some() || function.is_some()).then_some(Site { file, function });
        let pattern = pattern
            .map(|pattern| regex("pattern", pattern))
            .transpose()?;
        if pattern.is_none() && site.is_none() {
            return Err(SearchError::Unfiltered);
        }
        Ok(Self {
            pattern,
            site,
            kinds,
        })
    }
}

impl Site {
    /// A component's `file` lies in the site when it asks for a file glob alone.
    fn file(&self, file: &str) -> bool {
        self.function.is_none()
            && (self.file.as_ref()).is_some_and(|site| {
                site.line.is_none() && site.glob.matched(file, false).is_whitelist()
            })
    }

    /// A code location lies in the site when it matches each of its parts.
    fn code(&self, location: &Location) -> bool {
        let Location::Code(code) = location else {
            return false;
        };
        let file = self.file.as_ref().is_none_or(|site| {
            site.glob.matched(code.file.as_str(), false).is_whitelist()
                && (site.line).is_none_or(|line| (code.start_line..=code.end_line).contains(&line))
        });
        let function = self.function.as_ref().is_none_or(|function| {
            (code.function.as_ref()).is_some_and(|name| function.is_match(name.as_str()))
        });
        file && function
    }
}

/// Which of a node's texts or locations a row holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
enum Field {
    Title,
    Body,
    Step,
    Evidence,
    Access,
    Accessed,
    Location,
    File,
    Interface,
}

/// Where on a node a hit lies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Within {
    Title,
    Body,
    Step(u32),
    Evidence(EvidenceId),
    /// A path's access of a state.
    Access(Access, StateId),
    /// A state's access by a path.
    Accessed(Access, PathId),
    /// A premise's, flaw's or input state's own location.
    Location,
    File,
    Interface,
}

/// Where a hit lies: on the found node, or on a node merged into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, SerializeDisplay)]
pub(crate) struct Place {
    merged: Option<NodeId>,
    within: Within,
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(merged) = self.merged {
            write!(f, "merged {merged} ")?;
        }
        match self.within {
            Within::Title => f.write_str("title"),
            Within::Body => f.write_str("body"),
            Within::Step(position) => write!(f, "step {position}"),
            Within::Evidence(id) => write!(f, "evidence {id}"),
            Within::Access(access, state) => write!(f, "{access} of state {state}"),
            Within::Accessed(access, path) => write!(f, "{access} by path {path}"),
            Within::Location => f.write_str("location"),
            Within::File => f.write_str("file"),
            Within::Interface => f.write_str("interface"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Hit {
    #[serde(rename = "in")]
    #[schemars(with = "String")]
    pub(crate) place: Place,
    pub(crate) text: String,
}

/// A live node a search found, with the first of its hits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Found {
    #[serde(flatten)]
    pub(crate) node: NodeSummary,
    pub(crate) kind: NodeKind,
    pub(crate) hits: Vec<Hit>,
    /// Hits beyond those listed.
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) more: usize,
}

const fn is_zero(count: &usize) -> bool {
    *count == 0
}

impl Keyed for Found {
    fn key(&self) -> i64 {
        self.node.key()
    }
}

/// Each text and location of the nodes of the kinds in `?2` whose survivor is after `?1`, by
/// survivor: the survivor, the node, its kind, the [`Field`], its step position, evidence id or
/// accessing node, the access, the text and the location.
const SEARCHED: &str = concat!(
    "WITH n AS (SELECT id,coalesce(merged_into,id) AS live,kind,title,body FROM node WHERE \
     coalesce(merged_into,id)>?1 AND kind IN (SELECT value FROM json_each(?2))),\
     f(live,id,kind,field,number,access,text,at) AS (\
     SELECT n.live,n.id,n.kind,'title',NULL,NULL,title,NULL FROM n UNION ALL \
     SELECT n.live,n.id,n.kind,'body',NULL,NULL,body,NULL FROM n UNION ALL \
     SELECT n.live,n.id,n.kind,'step',s.position,NULL,s.note,s.location FROM n JOIN path_step s ON \
     s.path=n.id UNION ALL \
     SELECT n.live,n.id,n.kind,'evidence',e.id,NULL,e.note,e.location FROM n JOIN evidence e ON \
     e.node=n.id UNION ALL \
     SELECT n.live,n.id,n.kind,'access',a.state,a.access,NULL,a.location FROM n JOIN path_state a \
     ON a.path=n.id UNION ALL \
     SELECT n.live,n.id,n.kind,'accessed',a.path,a.access,NULL,a.location FROM n JOIN path_state \
     a ON a.state=n.id UNION ALL \
     SELECT n.live,n.id,n.kind,'location',NULL,NULL,NULL,i.location FROM n JOIN input_state i ON \
     i.state=n.id UNION ALL \
     SELECT n.live,n.id,n.kind,'location',NULL,NULL,NULL,p.location FROM n JOIN premise p ON \
     p.node=n.id UNION ALL \
     SELECT n.live,n.id,n.kind,'location',NULL,NULL,NULL,x.location FROM n JOIN flaw x ON \
     x.node=n.id UNION ALL \
     SELECT n.live,n.id,n.kind,'file',NULL,NULL,c.file,NULL FROM n JOIN component_file c ON \
     c.component=n.id UNION ALL \
     SELECT n.live,n.id,n.kind,'interface',NULL,NULL,c.interface,NULL FROM n JOIN \
     component_interface c ON c.component=n.id) \
     SELECT f.live,f.id,f.kind,f.field,f.number,f.access,f.text,",
    location_columns!(),
    " FROM f",
    location_joins!("f.at"),
    " ORDER BY f.live"
);

/// A node's hits so far, and whether its text matched the pattern and a location the site.
struct Matching {
    id: NodeId,
    kind: NodeKind,
    hits: Vec<Hit>,
    pattern: bool,
    site: bool,
}

impl Search {
    /// The hits `text` makes at `place`, a line each: where it matches the pattern, or on each
    /// line when it lies in the site.
    fn hits(&self, place: Place, text: &str, site: bool, node: &mut Matching) {
        node.site |= site;
        for line in text.lines() {
            let found = (self.pattern.as_ref()).and_then(|pattern| pattern.find(line));
            node.pattern |= found.is_some();
            if found.is_some() || site {
                node.hits.push(Hit {
                    place,
                    text: skyhook_tools::builtins::search::excerpt(
                        line.as_bytes(),
                        found.map(|found| found.range()),
                    ),
                });
            }
        }
    }

    /// Add the hits of `row`, of the node being matched.
    fn row(&self, row: &Row, node: &mut Matching) -> Result<()> {
        let source = NodeId(row.get(1)?);
        let number = || row.get::<i64>(4);
        let within = match enum_column::<Field>(row, 3)? {
            Field::Title => Within::Title,
            Field::Body => Within::Body,
            Field::Step => Within::Step(row.get(4)?),
            Field::Evidence => Within::Evidence(EvidenceId(number()?)),
            Field::Access => Within::Access(enum_column(row, 5)?, StateId(number()?)),
            Field::Accessed => Within::Accessed(enum_column(row, 5)?, PathId(number()?)),
            Field::Location => Within::Location,
            Field::File => Within::File,
            Field::Interface => Within::Interface,
        };
        let place = Place {
            merged: (source != node.id).then_some(source),
            within,
        };
        let site = self.site.as_ref();
        if let Some(text) = row.get::<Option<String>>(6)? {
            let file = within == Within::File && site.is_some_and(|site| site.file(&text));
            self.hits(place, &text, file, node);
        }
        if let Some(location) = location_column(row, 7)? {
            let code = site.is_some_and(|site| site.code(&location));
            self.hits(place, &location.to_string(), code, node);
        }
        Ok(())
    }

    /// `node` with its first hits and how many more it has, if it matched each of the pattern
    /// and the site given.
    fn found(&self, mut node: Matching) -> Option<(Matching, usize)> {
        if (self.pattern.is_some() && !node.pattern) || (self.site.is_some() && !node.site) {
            return None;
        }
        node.hits.sort_by_key(|hit| hit.place);
        let more = node.hits.len().saturating_sub(HITS);
        node.hits.truncate(HITS);
        Some((node, more))
    }
}

impl Store {
    /// A page of the live nodes `search` finds, oldest first.
    pub(crate) fn search(&self, search: &Search, page: Page) -> Result<Listing<Found>> {
        self.read(|db| {
            let limit = page.limit();
            let mut found = Vec::new();
            let mut current: Option<Matching> = None;
            let kinds = json_list(&search.kinds)?;
            db.query_as(SEARCHED, params![page.after, kinds], |row| {
                if found.len() == limit {
                    return Ok(());
                }
                let id = NodeId(row.get(0)?);
                let node = match current.take() {
                    Some(node) if node.id == id => current.insert(node),
                    done => {
                        found.extend(done.and_then(|done| search.found(done)));
                        if found.len() == limit {
                            return Ok(());
                        }
                        current.insert(Matching {
                            id,
                            kind: enum_column(row, 2)?,
                            hits: Vec::new(),
                            pattern: false,
                            site: false,
                        })
                    }
                };
                search.row(row, node)
            })?;
            if found.len() < limit {
                found.extend(current.and_then(|done| search.found(done)));
            }
            let ids: Vec<NodeId> = found.iter().map(|(node, _)| node.id).collect();
            let summaries = rows(db, SUMMARIES, params![json_list(&ids)?], |row| {
                summary(row, enum_column(row, 3)?)
            })?;
            let items = (found.into_iter().zip(summaries))
                .map(|((node, more), summary)| Found {
                    node: summary,
                    kind: node.kind,
                    hits: node.hits,
                    more,
                })
                .collect();
            Ok(Listing::keyset(items, page))
        })
    }
}

/// The summaries of the nodes in `?1`, by id, with their kinds.
const SUMMARIES: &str = "SELECT n.id,n.title,CASE n.kind WHEN 'component' THEN (SELECT status \
     FROM component WHERE node=n.id) WHEN 'path' THEN (SELECT status FROM path WHERE node=n.id) \
     WHEN 'state' THEN (SELECT status FROM state WHERE node=n.id) WHEN 'premise' THEN (SELECT \
     status FROM premise_standing WHERE node=n.id) WHEN 'flaw' THEN (SELECT status FROM \
     flaw_standing WHERE node=n.id) WHEN 'candidate' THEN (SELECT state FROM candidate_state \
     WHERE candidate=n.id) END,n.kind FROM json_each(?1) j JOIN node n ON n.id=j.value ORDER BY \
     n.id";

#[cfg(test)]
mod tests {
    use skyhook_base::bounded::Bounded;

    use super::*;
    use crate::store::tests::{fixture, path, scenario, text};

    fn search(pattern: Option<&str>, file: Option<&str>, line: Option<u32>) -> Search {
        Search::new(pattern, file, line, None, NodeKind::ALL.to_vec()).unwrap()
    }

    /// Each found node's places, by id.
    fn places(store: &Store, search: &Search) -> Vec<(NodeId, Vec<String>)> {
        let found = store.search(search, Page::default()).unwrap();
        (found.items.into_iter())
            .map(|found| {
                let places = found.hits.iter().map(|hit| hit.place.to_string()).collect();
                (found.node.id, places)
            })
            .collect()
    }

    /// A pattern matches each kind's own text and locations, of the kinds asked for; a file
    /// glob, a line it holds and a function match code locations; a search names at least one
    /// of them, and a line its file.
    #[test]
    fn a_search_matches_own_text_and_code_locations() {
        let (_session, store, actor) = fixture();
        let decoder = scenario(&store, &actor, "decoder");
        let NodeView::Path(view) = store.node(decoder.path.into()).unwrap() else {
            panic!("a path")
        };
        let input = view.states[0].id;
        let ring = Location::Code(CodeSite {
            file: text("decoder/ring.c"),
            start_line: 10,
            end_line: 20,
            function: Some(text("RingWrite")),
        });
        let evidence = NewEvidence {
            location: Some(ring),
            environment: None,
            stance: Stance::Context,
            note: text("the ring buffer wraps\nwithout a bound"),
        };
        let [noted] = store
            .evidence(&actor, decoder.flaw.into(), &[evidence])
            .unwrap()[..]
        else {
            panic!("one evidence")
        };
        let strings = |places: &[&str]| places.iter().map(|place| place.to_string()).collect();
        assert_eq!(
            places(&store, &search(Some("decoder"), None, None)),
            [
                (
                    decoder.component.into(),
                    strings(&["title", "file", "interface"])
                ),
                (
                    decoder.path.into(),
                    strings(&["title", &format!("read of state {input}")])
                ),
                (
                    input.into(),
                    strings(&[
                        "title",
                        &format!("read by path {}", decoder.path),
                        "location"
                    ])
                ),
                (
                    decoder.flaw.into(),
                    strings(&[&format!("evidence {noted}")])
                ),
            ]
        );
        let ring = Search::new(Some("(?i)RING"), None, None, None, vec![NodeKind::Flaw]).unwrap();
        let found = store.search(&ring, Page::default()).unwrap();
        let [found] = &found.items[..] else {
            panic!("{found:?}")
        };
        assert_eq!(
            found
                .hits
                .iter()
                .map(|hit| &hit.text[..])
                .collect::<Vec<_>>(),
            ["the ring buffer wraps", "decoder/ring.c:10-20 in RingWrite"]
        );
        let flaw = || {
            vec![(
                decoder.flaw.into(),
                strings(&[&format!("evidence {noted}")]),
            )]
        };
        assert_eq!(
            places(&store, &search(None, Some("decoder/*.c"), Some(15))),
            flaw()
        );
        assert!(places(&store, &search(None, Some("decoder/*.c"), Some(21))).is_empty());
        let function = Search::new(None, None, None, Some("^Ring"), NodeKind::ALL.to_vec());
        assert_eq!(places(&store, &function.unwrap()), flaw());
        let files = places(&store, &search(None, Some("*.c"), None));
        assert_eq!(
            files.len(),
            4,
            "the component, path, input and flaw: {files:?}"
        );

        let refused =
            |pattern, file, line| Search::new(pattern, file, line, None, Vec::new()).err();
        assert_eq!(refused(None, None, None), Some(SearchError::Unfiltered));
        assert_eq!(
            refused(None, None, Some(3)),
            Some(SearchError::LineWithoutFile)
        );
        assert_eq!(
            refused(Some("("), None, None),
            Some(SearchError::Regex("pattern"))
        );
    }

    /// A merged node's own text is found under the node it was merged into, which pages as one
    /// node.
    #[test]
    fn a_merged_nodes_text_is_found_under_its_survivor() {
        let (_session, store, actor) = fixture();
        let [kept, merged] = ["kept", "merged"].map(|name| path(&store, &actor, name, &[]));
        let merge = Merge {
            node: merged.into(),
            into: kept.into(),
            note: text("one flow"),
        };
        store.merge(&actor, &merge).unwrap();
        let paths = Search::new(Some("merged"), None, None, None, vec![NodeKind::Path]).unwrap();
        let NodeView::Path(view) = store.node(kept.into()).unwrap() else {
            panic!("a path")
        };
        let moved = (view.states.iter())
            .find(|state| state.title == "merged")
            .unwrap()
            .id;
        assert_eq!(
            places(&store, &paths),
            [(
                kept.into(),
                vec![
                    format!("read of state {moved}"),
                    format!("merged {merged} title")
                ]
            )]
        );
        let everything = search(Some("."), None, None);
        let mut page = Page::new(0, Bounded::new::<1>());
        let mut paged = Vec::new();
        loop {
            let listing = store.search(&everything, page).unwrap();
            paged.extend(listing.items.iter().map(|found| found.node.id));
            match listing.next_after {
                Some(after) => page.after = after,
                None => break,
            }
        }
        let live: Vec<NodeId> = (store.search(&everything, Page::default()).unwrap().items)
            .iter()
            .map(|found| found.node.id)
            .collect();
        assert_eq!(paged, live);
    }
}
