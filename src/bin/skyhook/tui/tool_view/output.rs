//! One presentation boundary for manager products and historical wire output.
use super::preview::{PreviewView, wire_continuation};
use serde_json::Value;
use skyhook::job::PresentedOutput;

/// The manager's projection stays authoritative; products and historical wire
/// output share one adapter that validates its preview shape once, not on
/// every card rebuild.
#[derive(Clone)]
pub struct OutputView {
    value: Value,
    shape: OutputShape,
}
/// The validated presentation derived from an output value. Borrowed values
/// (historical cards) build this directly without copying the output.
#[derive(Clone, Default)]
pub(super) struct OutputShape {
    pub(super) preview: Option<PreviewView>,
    pub(super) captures: Vec<CaptureView>,
}
#[derive(Clone)]
pub(super) struct CaptureView {
    pub(super) field: String,
    pub(super) errors: Vec<Value>,
    pub(super) preview: Option<PreviewView>,
}

impl OutputShape {
    pub(super) fn wire(value: &Value) -> Self {
        let preview = PreviewView::wire(value.get("preview"));
        let captures = value
            .get("captures")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|capture| CaptureView {
                field: capture
                    .get("field")
                    .and_then(Value::as_str)
                    .unwrap_or("Capture")
                    .into(),
                errors: ["/output/error", "/output/result/error"]
                    .into_iter()
                    .filter_map(|pointer| {
                        capture
                            .pointer(pointer)
                            .filter(|value| !value.is_null())
                            .cloned()
                    })
                    .collect(),
                preview: PreviewView::wire(capture.pointer("/output/preview")),
            })
            .collect();
        Self { preview, captures }
    }
}

impl OutputView {
    pub fn error(error: String) -> Self {
        Self {
            value: serde_json::json!({"error": error}),
            shape: OutputShape::default(),
        }
    }

    pub fn historical(value: Value) -> Self {
        Self {
            shape: OutputShape::wire(&value),
            value,
        }
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    /// Next saved-source page, in owner order: selected preview, first
    /// truncation, then first captured-read preview with a continuation.
    pub fn continuation(&self) -> Option<(&str, usize, usize)> {
        self.shape
            .preview
            .as_ref()
            .and_then(PreviewView::continuation)
            .or_else(|| {
                self.value
                    .pointer("/truncated/0")
                    .and_then(wire_continuation)
            })
            .or_else(|| {
                self.shape
                    .captures
                    .iter()
                    .filter_map(|capture| capture.preview.as_ref())
                    .find_map(PreviewView::continuation)
            })
    }

    pub(super) fn shape(&self) -> &OutputShape {
        &self.shape
    }
}

impl From<PresentedOutput> for OutputView {
    fn from(product: PresentedOutput) -> Self {
        Self::historical(product.into_view())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn historical_continuations_keep_source_coordinates_and_owner_precedence() {
        let mut value = json!({
            "preview": {"field": "", "lines": ["{\"result\":[1,"], "next_start": 1, "next_offset": 13},
            "truncated": [{"field": "/result/stdout", "next_start": 4, "next_offset": 7}],
            "captures": [
                {"field": "/result/end", "output": {"preview": {
                    "field": "/result/end", "lines": ["end"], "total_lines": 1}}},
                {"field": "/result/custom~1field", "output": {"preview": {
                    "field": "/result/custom~1field", "lines": ["é雪"], "next_start": 8, "next_offset": 5}}}
            ]
        });
        for (remove, expected) in [
            (None, Some(("", 1, 13))),
            (Some("preview"), Some(("/result/stdout", 4, 7))),
            (Some("truncated"), Some(("/result/custom~1field", 8, 5))),
            (Some("captures"), None),
        ] {
            if let Some(key) = remove {
                value.as_object_mut().unwrap().remove(key);
            }
            let output = OutputView::historical(value.clone());
            assert_eq!(output.continuation(), expected);
            assert_eq!(output.value(), &value);
            let mut document = super::super::Document::default();
            document.output_with_error("script", &Value::Null, Some(&output), None);
            // Formatting the page cannot change saved source line/byte coordinates.
            assert_eq!(output.continuation(), expected);
            assert_eq!(output.value(), &value);
        }
    }
}
