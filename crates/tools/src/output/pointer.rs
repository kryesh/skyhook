//! JSON Pointers into a job's saved output, and the text fields tools capture.

use std::io;

use skyhook_base::string_newtype;
use skyhook_worker::diagnostic::escape_pointer_segment;

string_newtype! {
    /// A JSON Pointer into a job's saved output: the whole output (`""`) or `/`-led
    /// segments with `~0`/`~1` escapes. Ordered and compared by spelling.
    #[derive(Default, PartialOrd, Ord)]
    pub struct FieldPointer(InvalidFieldPointer) = |field| {
        let mut characters = field.chars();
        let valid_root = field.is_empty() || field.starts_with('/');
        let mut valid_escapes = true;
        while let Some(character) = characters.next() {
            if character == '~' && !matches!(characters.next(), Some('0' | '1')) {
                valid_escapes = false;
                break;
            }
        }
        (valid_root && valid_escapes)
            .then_some(())
            .ok_or(InvalidFieldPointer)
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("field must be a JSON Pointer")]
pub struct InvalidFieldPointer;

impl From<InvalidFieldPointer> for io::Error {
    fn from(error: InvalidFieldPointer) -> Self {
        Self::new(io::ErrorKind::InvalidInput, error)
    }
}

impl FieldPointer {
    pub const fn root() -> Self {
        Self(String::new())
    }
    /// The product's result, where every tool payload lives.
    pub fn result() -> Self {
        Self::root().property("result")
    }
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
    /// The pointer of object member `key` under this one.
    pub fn property(&self, key: &str) -> Self {
        Self(format!("{}/{}", self.0, escape_pointer_segment(key)))
    }
    /// The pointer of array element `index` under this one.
    pub fn index(&self, index: usize) -> Self {
        Self(format!("{}/{index}", self.0))
    }
    /// The array index a reference token names: digits without a sign or a
    /// leading zero, as JSON Pointer requires.
    pub fn array_index(segment: &str) -> Option<usize> {
        let index = segment.parse::<usize>().ok()?;
        (segment == index.to_string()).then_some(index)
    }
    /// The unescaped reference tokens, outermost first; none for the root.
    pub fn segments(&self) -> impl Iterator<Item = String> + '_ {
        self.0
            .split('/')
            .skip(1)
            .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
    }
    /// Whether `other` addresses a strict descendant of this pointer.
    pub fn contains(&self, other: &Self) -> bool {
        other
            .0
            .strip_prefix(self.0.as_str())
            .is_some_and(|rest| rest.starts_with('/'))
    }

    /// `relative`, addressed from a value placed at this pointer.
    pub(crate) fn join(&self, relative: &Self) -> Self {
        Self(format!("{}{}", self.0, relative.0))
    }

    /// This pointer relative to `base`, when it addresses `base` or a descendant.
    pub(crate) fn relative_to(&self, base: &Self) -> Option<Self> {
        let rest = self.0.strip_prefix(base.0.as_str())?;
        (rest.is_empty() || rest.starts_with('/')).then(|| Self(rest.to_owned()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextCaptureField {
    Stdout,
    Stderr,
    Content,
    Console,
}

impl TextCaptureField {
    pub fn pointer(self) -> FieldPointer {
        FieldPointer::result().property(match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Content => "content",
            Self::Console => "console",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_pointers_parse_at_the_boundary_and_know_their_descendants() {
        for invalid in ["not-a-pointer", "/result/~", "/result/~2", "result"] {
            assert_eq!(invalid.parse::<FieldPointer>(), Err(InvalidFieldPointer));
        }
        let result = FieldPointer::result();
        let escaped = result.property("a/b").property("~key").index(2);
        assert_eq!(escaped.as_str(), "/result/a~1b/~0key/2");
        assert_eq!(escaped.as_str().parse::<FieldPointer>().unwrap(), escaped);
        assert!(FieldPointer::root().contains(&result));
        assert!(result.contains(&escaped));
        assert!(!result.contains(&result));
        assert!(!result.contains(&FieldPointer::root().property("results")));
        assert!(!escaped.contains(&result));
    }
}
