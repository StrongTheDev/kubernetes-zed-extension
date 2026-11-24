//! A positioned YAML tree for one document.
//!
//! The line scanner in [`crate::yaml`] tolerates half-typed documents, which
//! completion and hovers need. Validation needs the opposite: exact structure
//! (flow mappings, block scalars, quoting), so it uses a real parser and is
//! skipped while a document doesn't parse.

use std::iter::Peekable;

use saphyr_parser::{Event, Parser, ScalarStyle, Span, StrInput};

use crate::yaml::Doc;

/// Nesting beyond this is not a Kubernetes manifest.
const MAX_DEPTH: usize = 100;

/// A range on one line, in characters (0-based line, absolute in the file).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pos {
    pub line: usize,
    pub start: usize,
    /// `usize::MAX` means "to the end of the line".
    pub end: usize,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub pos: Pos,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub enum Value {
    Map(Vec<(Key, Node)>),
    Seq(Vec<Node>),
    Scalar { text: String, plain: bool },
    /// Aliases and custom tags: present, but we can't say what they hold.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Key {
    pub name: String,
    pub pos: Pos,
}

impl Node {
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.entries().iter().find(|(k, _)| k.name == key).map(|(_, v)| v)
    }

    /// Follows mapping keys, e.g. `at(&["spec", "template"])`.
    pub fn at(&self, path: &[&str]) -> Option<&Node> {
        path.iter().try_fold(self, |node, key| node.get(key))
    }

    pub fn items(&self) -> &[Node] {
        match &self.value {
            Value::Seq(items) => items,
            _ => &[],
        }
    }

    pub fn entries(&self) -> &[(Key, Node)] {
        match &self.value {
            Value::Map(entries) => entries,
            _ => &[],
        }
    }

    pub fn str(&self) -> Option<&str> {
        match &self.value {
            Value::Scalar { text, .. } => Some(text),
            _ => None,
        }
    }

    /// `key: value` pairs of a mapping of scalars.
    pub fn pairs(&self) -> Vec<(String, String)> {
        self.entries().iter().filter_map(|(k, v)| Some((k.name.clone(), v.str()?.to_string()))).collect()
    }
}

/// Parses one document, or `None` if it is not (yet) valid YAML.
pub fn parse(lines: &[&str], doc: &Doc) -> Option<Node> {
    let text = doc.text(lines);
    let mut events = Parser::new_from_str(&text).peekable();
    loop {
        match events.next()?.ok()?.0 {
            Event::StreamStart | Event::DocumentStart(_) => {}
            // Every Kubernetes object is a mapping.
            Event::MappingStart(..) => return mapping(&mut events, doc.start, 0),
            _ => return None,
        }
    }
}

type Events<'a> = Peekable<Parser<'a, StrInput<'a>>>;

fn pos(span: &Span, offset: usize) -> Pos {
    let (a, b) = (span.start, span.end);
    Pos {
        line: a.line().saturating_sub(1) + offset,
        start: a.col(),
        // Multi-line nodes are reported on their first line only.
        end: if b.line() == a.line() { b.col().max(a.col()) } else { usize::MAX },
    }
}

fn node(events: &mut Events<'_>, offset: usize, depth: usize) -> Option<Node> {
    if depth > MAX_DEPTH {
        return None;
    }
    let (event, span) = events.next()?.ok()?;
    let at = pos(&span, offset);
    let value = match event {
        Event::Scalar(text, style, _, tag) => match &tag {
            Some(t) if t.suffix != "str" => Value::Unknown,
            _ => Value::Scalar {
                text: text.into_owned(),
                plain: style == ScalarStyle::Plain && tag.is_none(),
            },
        },
        Event::Alias(_) => Value::Unknown,
        Event::MappingStart(..) => return mapping(events, offset, depth),
        Event::SequenceStart(..) => {
            let mut items = Vec::new();
            while !matches!(events.peek()?, Ok((Event::SequenceEnd, _))) {
                items.push(node(events, offset, depth + 1)?);
            }
            events.next();
            Value::Seq(items)
        }
        _ => return None,
    };
    Some(Node { pos: at, value })
}

fn mapping(events: &mut Events<'_>, offset: usize, depth: usize) -> Option<Node> {
    let mut entries: Vec<(Key, Node)> = Vec::new();
    while !matches!(events.peek()?, Ok((Event::MappingEnd, _))) {
        let key = node(events, offset, depth + 1)?;
        let value = node(events, offset, depth + 1)?;
        let Value::Scalar { text, .. } = key.value else {
            return None; // complex keys don't occur in manifests
        };
        entries.push((Key { name: text, pos: key.pos }, value));
    }
    events.next();
    let pos = entries.first().map_or(Pos { line: offset, start: 0, end: 0 }, |(k, _)| k.pos);
    Some(Node { pos, value: Value::Map(entries) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yaml;

    #[test]
    fn positions_are_absolute() {
        let src = "a: 1\n---\nkind: X\nspec:\n  list:\n    - name: \"q\"\n      n: 5\n  flow: {a: b, c: [1, 2]}\n  text: |\n    not: a key\n";
        let lines = yaml::lines(src);
        let docs = yaml::parse(&lines);
        let root = parse(&lines, &docs[1]).unwrap();
        let item = &root.at(&["spec", "list"]).unwrap().items()[0];
        assert_eq!(item.get("name").unwrap().pos, Pos { line: 5, start: 12, end: 15 });
        assert!(matches!(&item.get("name").unwrap().value, Value::Scalar { plain: false, .. }));
        assert_eq!(item.get("n").unwrap().pos.line, 6);
        assert_eq!(root.at(&["spec", "flow", "a"]).unwrap().str(), Some("b"));
        assert_eq!(root.at(&["spec", "text"]).unwrap().str(), Some("not: a key\n"));
        assert_eq!(root.get("spec").unwrap().entries()[0].0.pos.line, 4);
    }

    #[test]
    fn broken_yaml_is_none() {
        let src = "kind: X\nspec: [1, 2\n";
        let lines = yaml::lines(src);
        assert!(parse(&lines, &yaml::parse(&lines)[0]).is_none());
    }
}
