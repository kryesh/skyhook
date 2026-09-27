//! Streaming renderings of saved fields, which pages read without hydrating them.
use super::*;

fn render(
    saved: &Saved,
    field: &FieldPointer,
    value: &Value,
    out: &mut impl Write,
    cancellation: &crate::job::CancellationToken,
) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        return Err(ToolError::cancelled());
    }
    if let Some(mut source) = saved.stored(field) {
        if !value.is_string() {
            std::io::copy(&mut source, out)?;
            return Ok(());
        }
        out.write_all(b"\"")?;
        let mut input = BufReader::new(source);
        // Escape a stored string incrementally rather than hydrating it.
        loop {
            if cancellation.is_cancelled() {
                return Err(ToolError::cancelled());
            }
            let bytes = input.fill_buf()?;
            if bytes.is_empty() {
                break;
            }
            let mut run = 0;
            for (index, &byte) in bytes.iter().enumerate() {
                if !matches!(byte, b'"' | b'\\' | 0..=31) {
                    continue;
                }
                out.write_all(&bytes[run..index])?;
                match byte {
                    b'"' | b'\\' => out.write_all(&[b'\\', byte])?,
                    _ => write!(out, "\\u{byte:04x}")?,
                }
                run = index + 1;
            }
            out.write_all(&bytes[run..])?;
            let length = bytes.len();
            input.consume(length);
        }
        out.write_all(b"\"")?;
        return Ok(());
    }
    match value {
        Value::Object(map) => {
            out.write_all(b"{\n")?;
            for (index, (key, value)) in map.iter().enumerate() {
                if index > 0 {
                    out.write_all(b",\n")?;
                }
                serde_json::to_writer(&mut *out, key)?;
                out.write_all(b": ")?;
                render(saved, &field.property(key), value, out, cancellation)?;
            }
            out.write_all(b"\n}")?;
        }
        Value::Array(items) => {
            out.write_all(b"[\n")?;
            for (index, value) in items.iter().enumerate() {
                if index > 0 {
                    out.write_all(b",\n")?;
                }
                render(saved, &field.index(index), value, out, cancellation)?;
            }
            out.write_all(b"\n]")?;
        }
        _ => serde_json::to_writer(out, value)?,
    }
    Ok(())
}

/// The pageable bytes of `field`: a registered capture, or a rendering of the value
/// the saved document holds there. `None` when neither exists yet.
pub(super) fn field_source(
    saved: &Saved,
    field: &FieldPointer,
    cancellation: &crate::job::CancellationToken,
) -> Result<Option<Source>, ToolError> {
    if let Some(source) = saved.capture(field) {
        return Ok(Some(source));
    }
    let Some(product) = &saved.product else {
        return Ok(None);
    };
    let mut document = product.document();
    if document.pointer(field.as_str()).is_none() {
        // Stored containers hide their descendants in the compact document.
        // Only load the selected ancestor, not unrelated large output fields.
        if let Some(ancestor) = saved.fields.iter().find(|stored| stored.contains(field)) {
            hydrate_field(saved, &mut document, ancestor)?;
        }
    }
    let value = document
        .pointer(field.as_str())
        .ok_or_else(|| ToolError::invalid_arguments("field does not exist in this result"))?;
    materialize_field(saved, field, value, cancellation).map(Some)
}

// Projection already owns the resolved value; keep the same renderer for stable
// pagination positions.
pub(super) fn materialize_field(
    saved: &Saved,
    field: &FieldPointer,
    value: &Value,
    cancellation: &crate::job::CancellationToken,
) -> Result<Source, ToolError> {
    if let Some(source) = saved.stored(field) {
        return Ok(source);
    }
    let write = |out: &mut dyn Write| -> Result<(), ToolError> {
        match value.as_str() {
            Some(text) => Ok(out.write_all(text.as_bytes())?),
            None => render(saved, field, value, &mut &mut *out, cancellation),
        }
    };
    // Values enclosing stored fields need disk-backed rendering. Reuse a saved
    // rendering only when its contents are independent of the reader.
    let encloses_stored = saved.fields.iter().any(|stored| field.contains(stored));
    let db = &saved.output.db;
    let job = saved.output.job.get();
    if encloses_stored && saved.cacheable(field) {
        if let Some(capture) = db.rendering(job, field).map_err(database)? {
            return Ok(Source::Capture(reader::CaptureReader::new(
                db.clone(),
                capture,
            )));
        }
        // Cache reservations are optional: contention or an unavailable cache
        // can still use private disk storage, without buffering the whole output.
        if let Ok(pending) = PendingCapture::rendering(&saved.output, field) {
            let mut writer = pending.open();
            write(&mut writer)?;
            let capture = writer.finish()?.capture_id();
            return Ok(Source::Capture(reader::CaptureReader::new(
                db.clone(),
                capture,
            )));
        }
    }
    if encloses_stored {
        // Capability-sensitive renderings must never be persisted under a shared
        // field key. The anonymous file also disappears on errors or cancellation.
        let mut writer = std::io::BufWriter::new(tempfile::tempfile()?);
        write(&mut writer)?;
        let mut file = writer.into_inner().map_err(|error| error.into_error())?;
        std::io::Seek::rewind(&mut file)?;
        return Ok(Source::Temporary(file));
    }
    let mut bytes = Vec::new();
    write(&mut bytes)?;
    Ok(Source::Memory(std::io::Cursor::new(bytes)))
}
