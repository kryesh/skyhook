//! The standard types configuration is written in.

use std::{
    collections::{BTreeMap, BTreeSet},
    hash::Hash,
    num::{NonZeroU16, NonZeroU64},
    path::PathBuf,
};

use indexmap::{IndexMap, IndexSet};

use super::{Context, Decode, Node, Number, Segment, Value};

impl Decode for bool {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        match node.value {
            Value::Bool(value) => Some(value),
            _ => {
                cx.expected(node, "a boolean");
                None
            }
        }
    }
}

impl Decode for String {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        match node.as_str() {
            Some(text) => Some(text.to_owned()),
            None => {
                cx.expected(node, "a string");
                None
            }
        }
    }
}

impl Decode for PathBuf {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        String::decode(node, cx).map(Self::from)
    }
}

fn unsigned(node: &Node, cx: &mut Context, expected: &str) -> Option<u64> {
    match node.value {
        Value::Number(Number::Unsigned(value)) => Some(value),
        _ => {
            cx.expected(node, expected);
            None
        }
    }
}

macro_rules! unsigned {
    ($($type:ty),+) => {$(
        impl Decode for $type {
            fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
                let value = unsigned(node, cx, "a nonnegative integer")?;
                let refused = |cx: &mut Context| {
                    cx.error(node, format_args!("expected an integer no larger than {}", Self::MAX))
                };
                Self::try_from(value).map_err(|_| refused(cx)).ok()
            }
        }
    )+};
}

unsigned!(u8, u16, u32, u64, usize);

macro_rules! nonzero {
    ($($type:ty),+) => {$(
        impl Decode for $type {
            fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
                let value = unsigned(node, cx, "a positive integer")?;
                if value == 0 {
                    cx.error(node, "expected a positive integer, found 0");
                    return None;
                }
                let value = value.try_into().ok().and_then(<$type>::new);
                value.or_else(|| {
                    let max = <$type>::MAX;
                    cx.error(node, format_args!("expected an integer no larger than {max}"));
                    None
                })
            }
        }
    )+};
}

nonzero!(NonZeroU16, NonZeroU64);

impl Decode for i64 {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        match node.value {
            Value::Number(Number::Signed(value)) => Some(value),
            Value::Number(Number::Unsigned(value)) if i64::try_from(value).is_ok() => {
                Some(value as i64)
            }
            _ => {
                cx.expected(node, "an integer");
                None
            }
        }
    }
}

impl Decode for f64 {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        match node.value {
            Value::Number(Number::Float(value)) => Some(value),
            Value::Number(Number::Signed(value)) => Some(value as f64),
            Value::Number(Number::Unsigned(value)) => Some(value as f64),
            _ => {
                cx.expected(node, "a number");
                None
            }
        }
    }
}

/// Null decodes as nothing, as leaving the field out does.
impl<T: Decode> Decode for Option<T> {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        match node.value {
            Value::Null => Some(None),
            _ => T::decode(node, cx).map(Some),
        }
    }

    fn absent() -> Option<Self> {
        Some(None)
    }
}

/// Each item of a list, every refused one reported and left out, so the walk goes on
/// with the rest.
fn items<T: Decode>(node: &Node, cx: &mut Context) -> Option<Vec<T>> {
    let Value::Seq(items) = &node.value else {
        cx.expected(node, "a list");
        return None;
    };
    let decoded = items
        .iter()
        .enumerate()
        .map(|(index, item)| cx.within(Segment::Index(index), |cx| T::decode(item, cx)));
    Some(decoded.flatten().collect())
}

impl<T: Decode> Decode for Vec<T> {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        items(node, cx)
    }
}

impl<T: Decode + Ord> Decode for BTreeSet<T> {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        items(node, cx).map(Vec::into_iter).map(Iterator::collect)
    }
}

impl<T: Decode + Hash + Eq> Decode for IndexSet<T> {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        items(node, cx).map(Vec::into_iter).map(Iterator::collect)
    }
}

/// Each entry of a mapping, its key decoded where the key is written; a refused entry is
/// reported and left out.
fn entries<K: Decode, V: Decode>(node: &Node, cx: &mut Context) -> Option<Vec<(K, V)>> {
    let Some(map) = node.as_map() else {
        cx.expected(node, "a mapping");
        return None;
    };
    let decoded = map.iter().map(|entry| {
        cx.within(Segment::Key(entry.key.clone()), |cx| {
            let key = Node {
                value: Value::Str(entry.key.clone()),
                at: entry.at.clone(),
            };
            let key = K::decode(&key, cx);
            let value = V::decode(&entry.value, cx);
            key.zip(value)
        })
    });
    Some(decoded.flatten().collect())
}

impl<K: Decode + Hash + Eq, V: Decode> Decode for IndexMap<K, V> {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        entries(node, cx).map(Vec::into_iter).map(Iterator::collect)
    }
}

impl<K: Decode + Ord, V: Decode> Decode for BTreeMap<K, V> {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        entries(node, cx).map(Vec::into_iter).map(Iterator::collect)
    }
}
