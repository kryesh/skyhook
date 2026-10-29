//! What one HTML extraction may build: elements while parsing, bytes while
//! parsing and converting. Work stops as soon as the budget is spent.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::mem::size_of;
use std::rc::Rc;

use dom_query::{Document, NodeId, NodeRef, TreeNode};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeBuilderOpts, TreeSink};
use html5ever::{Attribute, ParseOpts, QualName};

pub(super) const MAX_HTML_ELEMENTS: usize = 50_000;
pub(super) const MAX_HTML_DEPTH: usize = 256;
/// What the parsed document holds and the markdown built from it.
const MAX_BUILT_BYTES: usize = 20 * 1024 * 1024;
/// The tree builder's work per token grows with its open elements, so input is
/// fed in chunks small enough that a chunk past the depth limit stays cheap.
const CHUNK_BYTES: usize = 4096;
/// What the document keeps for each node and attribute besides its text.
const NODE: usize = size_of::<TreeNode>();
const ATTRIBUTE: usize = size_of::<Attribute>();

/// The budget ran out: whatever was being built is too large.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Spent;

/// Shared by the parse and the conversion of one document. Once spent it stays
/// spent.
#[derive(Clone, Default)]
pub(super) struct Budget(Rc<Meter>);

#[derive(Default)]
struct Meter {
    elements: Cell<usize>,
    bytes: Cell<usize>,
    spent: Cell<bool>,
}

impl Budget {
    /// Charges `bytes` about to be built.
    pub(super) fn charge(&self, bytes: usize) -> Result<(), Spent> {
        self.add(&self.0.bytes, bytes, MAX_BUILT_BYTES)
    }

    /// Charges one element and the bytes it holds.
    fn element(&self, bytes: usize) -> Result<(), Spent> {
        self.add(&self.0.elements, 1, MAX_HTML_ELEMENTS)?;
        self.charge(bytes)
    }

    fn add(&self, counter: &Cell<usize>, amount: usize, limit: usize) -> Result<(), Spent> {
        counter.set(counter.get().saturating_add(amount));
        if counter.get() > limit {
            self.exhaust();
        }
        if self.spent() { Err(Spent) } else { Ok(()) }
    }

    fn exhaust(&self) {
        self.0.spent.set(true);
    }

    fn spent(&self) -> bool {
        self.0.spent.get()
    }
}

/// Parse `html` as a browser would, unless it is over the element, nesting or
/// byte limits. Misnested markup makes the tree builder create elements no tag
/// names, so the tree is charged as it is built, not the input.
pub(super) fn parse(html: &str, budget: &Budget) -> Result<Document, Spent> {
    let sink = Metered {
        document: Document::default(),
        budget: budget.clone(),
        hosts: RefCell::default(),
        depths: RefCell::default(),
    };
    let options = ParseOpts {
        tree_builder: TreeBuilderOpts {
            scripting_enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut parser = html5ever::parse_document(sink, options);
    let mut rest = html;
    while !rest.is_empty() && !budget.spent() {
        let mut end = rest.len().min(CHUNK_BYTES);
        while !rest.is_char_boundary(end) {
            end += 1;
        }
        let (chunk, tail) = rest.split_at(end);
        parser.process(StrTendril::from_slice(chunk));
        rest = tail;
    }
    let document = parser.finish().document;
    if budget.spent() {
        Err(Spent)
    } else {
        Ok(document)
    }
}

/// dom_query's document, charging the budget for every node, attribute and text it
/// keeps and for every parse error, and spending it when a node is placed past the depth
/// limit. html5ever cannot stop within a chunk, so once the budget is spent the
/// sink keeps no more text or attributes.
struct Metered {
    document: Document,
    budget: Budget,
    /// Each template's contents, a separate fragment, mapped to its template.
    hosts: RefCell<HashMap<NodeId, NodeId>>,
    /// Depths found so far. A moved node takes its subtree with it, so any move
    /// forgets them all.
    depths: RefCell<HashMap<NodeId, usize>>,
}

impl Metered {
    /// Whether to keep `child` under `parent`, after `before`: text is charged, as
    /// a node of its own unless the document joins it to text `before`, and a
    /// node past the depth limit spends the budget.
    fn place(
        &self,
        parent: Option<NodeId>,
        before: Option<NodeRef<'_>>,
        child: &NodeOrText<NodeId>,
    ) -> bool {
        match child {
            NodeOrText::AppendText(text) => {
                let node = if before.is_some_and(|before| before.is_text()) {
                    0
                } else {
                    NODE
                };
                self.budget.charge(node + text.len()).is_ok()
            }
            NodeOrText::AppendNode(node) => {
                let tree = &self.document.tree;
                if tree.parent_of(node).is_some() || tree.first_child_of(node).is_some() {
                    self.moved();
                }
                if parent.is_some_and(|parent| self.depth(parent) >= MAX_HTML_DEPTH) {
                    self.budget.exhaust();
                }
                true
            }
        }
    }

    fn parent(&self, node: &NodeId) -> Option<NodeId> {
        self.document.tree.parent_of(node).map(|parent| parent.id)
    }

    /// `attrs` once charged, or none once the budget is spent.
    fn keep(
        &self,
        attrs: Vec<Attribute>,
        charge: impl FnOnce(usize) -> Result<(), Spent>,
    ) -> Vec<Attribute> {
        let bytes = attrs
            .iter()
            .map(|attr| ATTRIBUTE + attr.name.local.len() + attr.value.len());
        match charge(bytes.sum()) {
            Ok(()) => attrs,
            Err(Spent) => Vec::new(),
        }
    }

    fn moved(&self) {
        self.depths.borrow_mut().clear();
    }

    /// The node's depth, counted no further than the limit.
    fn depth(&self, node: NodeId) -> usize {
        let (mut at, mut steps) = (node, 0);
        let depth = loop {
            if let Some(known) = self.depths.borrow().get(&at) {
                break (steps + known).min(MAX_HTML_DEPTH);
            }
            if steps == MAX_HTML_DEPTH {
                break steps;
            }
            match self.document.tree.parent_of(&at) {
                Some(parent) => at = parent.id,
                None => match self.hosts.borrow().get(&at) {
                    Some(host) => at = *host,
                    None => break steps,
                },
            }
            steps += 1;
        };
        self.depths.borrow_mut().insert(node, depth);
        depth
    }
}

/// Defines sink methods that only pass their arguments on to the document.
macro_rules! forward {
    ($(fn $name:ident(&self $(, $arg:ident: $type:ty)*) $(-> $output:ty)?;)*) => {
        $(fn $name(&self $(, $arg: $type)*) $(-> $output)? {
            self.document.$name($($arg),*)
        })*
    };
}

impl TreeSink for Metered {
    type Handle = NodeId;
    type Output = Self;
    type ElemName<'a> = <Document as TreeSink>::ElemName<'a>;

    fn finish(self) -> Self {
        self
    }

    forward! {
        fn get_document(&self) -> NodeId;
        fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId;
        fn append_doctype_to_document(&self, name: StrTendril, public: StrTendril, system: StrTendril);
        fn get_template_contents(&self, target: &NodeId) -> NodeId;
        fn same_node(&self, x: &NodeId, y: &NodeId) -> bool;
        fn set_quirks_mode(&self, mode: QuirksMode);
        fn is_mathml_annotation_xml_integration_point(&self, handle: &NodeId) -> bool;
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.moved();
        self.document.remove_from_parent(target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        self.moved();
        self.document.reparent_children(node, new_parent);
    }

    /// Each error costs the tree builder work, so it is counted, but not kept.
    fn parse_error(&self, _: Cow<'static, str>) {
        let _ = self.budget.element(0);
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> Self::ElemName<'a> {
        self.document.elem_name(target)
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
        let attrs = self.keep(attrs, |bytes| self.budget.element(NODE + bytes));
        let template = flags.template;
        let element = self.document.create_element(name, attrs, flags);
        if template {
            let contents = self.document.get_template_contents(&element);
            self.hosts.borrow_mut().insert(contents, element);
        }
        element
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        let kept = self.budget.charge(NODE + text.len()).is_ok();
        self.document
            .create_comment(if kept { text } else { StrTendril::new() })
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        let attrs = self.keep(attrs, |bytes| self.budget.charge(bytes));
        self.document.add_attrs_if_missing(target, attrs);
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        let before = self.document.tree.last_child_of(parent);
        if self.place(Some(*parent), before, &child) {
            self.document.append(parent, child);
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        match self.parent(element) {
            Some(_) => self.append_before_sibling(element, child),
            None => self.append(prev_element, child),
        }
    }

    fn append_before_sibling(&self, sibling: &NodeId, child: NodeOrText<NodeId>) {
        let before = self.document.tree.prev_sibling_of(sibling);
        if self.place(self.parent(sibling), before, &child) {
            self.document.append_before_sibling(sibling, child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each character reference appends text of its own, which the document joins
    /// to the text before it, appended or foster-parented before a table.
    #[test]
    fn text_joined_to_text_is_charged_only_for_its_bytes() {
        let references = "&amp;".repeat(MAX_BUILT_BYTES / NODE + 1);
        for html in [format!("<p>{references}"), format!("<table>{references}")] {
            assert!(parse(&html, &Budget::default()).is_ok());
        }
    }
}
