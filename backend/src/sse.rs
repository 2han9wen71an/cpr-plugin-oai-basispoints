use serde_json::Value;

/// A parsed SSE event.
///
/// An event is dispatched when one or more `data:` fields are present. The
/// values of repeated `data:` fields are joined with a newline, as required by
/// the SSE wire format. Unknown fields and comment lines are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The value of the `event:` field, when one was supplied.
    pub event: Option<String>,
    /// The dispatched data value.
    pub data: String,
}

impl SseEvent {
    /// Return the event name without taking ownership of it.
    #[must_use]
    pub fn event_name(&self) -> Option<&str> {
        self.event.as_deref()
    }

    /// Parse this event's data as JSON.
    ///
    /// The `[DONE]` sentinel is deliberately not special-cased here; callers
    /// that need to distinguish control frames can use [`Self::is_done`]
    /// before calling this method.
    pub fn json(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(self.data.trim())
    }

    /// Alias for [`Self::json`], useful at call sites that make the data/value
    /// distinction explicit.
    pub fn json_value(&self) -> Result<Value, serde_json::Error> {
        self.json()
    }

    /// Return whether this event carries the conventional SSE done sentinel.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

/// A parsed SSE event whose data has already been decoded as JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct SseJsonEvent {
    /// The value of the `event:` field, when one was supplied.
    pub event: Option<String>,
    /// The decoded JSON data.
    pub data: Value,
}

impl SseJsonEvent {
    /// Return the explicit SSE event name, if present.
    #[must_use]
    pub fn event_name(&self) -> Option<&str> {
        self.event.as_deref()
    }

    /// Return the semantic event type.
    ///
    /// OpenAI Responses events normally carry the type in both the SSE event
    /// field and the JSON body. The wire event field takes precedence, while
    /// the JSON `type` is a useful fallback for data-only frames.
    #[must_use]
    pub fn event_type(&self) -> Option<&str> {
        self.event
            .as_deref()
            .or_else(|| self.data.get("type").and_then(Value::as_str))
    }

    /// Borrow the decoded JSON value.
    #[must_use]
    pub const fn value(&self) -> &Value {
        &self.data
    }

    /// Consume the helper and return its decoded JSON value.
    #[must_use]
    pub fn into_value(self) -> Value {
        self.data
    }

    /// Return whether the event is a terminal event.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.event_type().is_some_and(is_terminal_event)
    }
}

/// Compatibility aliases for callers that prefer the adjective first.
pub type JsonSseEvent = SseJsonEvent;
pub type SseSemanticEvent = SseJsonEvent;

/// Parse all complete SSE events in a byte/string-like value.
///
/// This forgiving helper uses replacement characters for invalid UTF-8. The
/// byte-preserving [`SseFrameSplitter`] remains the transport primitive, while
/// [`try_parse_sse_events`] is available when invalid UTF-8 should be rejected.
#[must_use]
pub fn parse_sse_events<T: AsRef<[u8]>>(input: T) -> Vec<SseEvent> {
    parse_sse_events_text(&String::from_utf8_lossy(input.as_ref()))
}

/// Parse the first event in a byte/string-like SSE frame.
#[must_use]
pub fn parse_sse_event<T: AsRef<[u8]>>(input: T) -> Option<SseEvent> {
    parse_sse_events(input).into_iter().next()
}

/// Parse a complete frame's events.
///
/// This is an explicit frame-named alias for [`parse_sse_events`]. A frame can
/// contain more than one dispatched event when used independently of the
/// splitter, so the return type is a vector.
#[must_use]
pub fn parse_sse_frame<T: AsRef<[u8]>>(frame: T) -> Vec<SseEvent> {
    parse_sse_events(frame)
}

/// Parse the first dispatched event from a complete frame.
#[must_use]
pub fn parse_sse_frame_event<T: AsRef<[u8]>>(frame: T) -> Option<SseEvent> {
    parse_sse_event(frame)
}

/// Parse SSE events while rejecting invalid UTF-8.
///
/// No third-party error type is needed: the standard library's
/// [`std::str::Utf8Error`] is sufficient for the wire validation performed by
/// this helper.
pub fn try_parse_sse_events(input: &[u8]) -> Result<Vec<SseEvent>, std::str::Utf8Error> {
    Ok(parse_sse_events_text(std::str::from_utf8(input)?))
}

/// Parse the first SSE event while rejecting invalid UTF-8.
pub fn try_parse_sse_event(input: &[u8]) -> Result<Option<SseEvent>, std::str::Utf8Error> {
    Ok(try_parse_sse_events(input)?.into_iter().next())
}

/// Parse an event's JSON data.
pub fn parse_json_event(event: &SseEvent) -> Result<SseJsonEvent, serde_json::Error> {
    Ok(SseJsonEvent {
        event: event.event.clone(),
        data: event.json()?,
    })
}

/// Parse the first JSON-bearing event in an SSE frame.
///
/// Empty frames and `[DONE]` control frames return `Ok(None)`.
pub fn parse_sse_json_event<T: AsRef<[u8]>>(
    frame: T,
) -> Result<Option<SseJsonEvent>, serde_json::Error> {
    let Some(event) = parse_sse_event(frame) else {
        return Ok(None);
    };
    if event.is_done() {
        return Ok(None);
    }
    parse_json_event(&event).map(Some)
}

/// Alias for [`parse_sse_json_event`].
pub fn parse_json_sse_event<T: AsRef<[u8]>>(
    frame: T,
) -> Result<Option<SseJsonEvent>, serde_json::Error> {
    parse_sse_json_event(frame)
}

/// Parse every JSON-bearing event in an SSE frame/body.
///
/// A `[DONE]` event is a control frame and is omitted. Invalid JSON is
/// returned rather than silently changing the semantic stream.
pub fn parse_sse_json_events<T: AsRef<[u8]>>(
    input: T,
) -> Result<Vec<SseJsonEvent>, serde_json::Error> {
    parse_sse_events(input)
        .into_iter()
        .filter(|event| !event.is_done())
        .map(|event| parse_json_event(&event))
        .collect()
}

fn parse_sse_events_text(input: &str) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let mut event_name = None;
    let mut data = String::new();
    let mut has_data = false;

    for (line_index, raw_line) in input.split('\n').enumerate() {
        let raw_line = if line_index == 0 {
            raw_line.strip_prefix('\u{feff}').unwrap_or(raw_line)
        } else {
            raw_line
        };
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            if has_data {
                events.push(SseEvent {
                    event: event_name.take(),
                    data: std::mem::take(&mut data),
                });
                has_data = false;
            } else {
                event_name = None;
                data.clear();
            }
            continue;
        }
        if line.starts_with(':') {
            continue;
        }

        let (field, value) = split_sse_field(line);
        match field {
            "event" => event_name = Some(value.to_owned()),
            "data" => {
                if has_data {
                    data.push('\n');
                }
                data.push_str(value);
                has_data = true;
            }
            _ => {}
        }
    }

    if has_data {
        events.push(SseEvent {
            event: event_name,
            data,
        });
    }
    events
}

fn split_sse_field(line: &str) -> (&str, &str) {
    let Some((field, value)) = line.split_once(':') else {
        return (line, "");
    };
    (field, value.strip_prefix(' ').unwrap_or(value))
}

/// Return whether an event type marks the end of a Responses-style stream.
#[must_use]
pub fn is_terminal_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "response.completed"
            | "response.done"
            | "response.incomplete"
            | "response.failed"
            | "response.cancelled"
            | "response.canceled"
            | "error"
    )
}

/// Return whether a parsed event is terminal, including the `[DONE]` sentinel.
#[must_use]
pub fn is_terminal_sse_event(event: &SseEvent) -> bool {
    event.is_done() || event.event_name().is_some_and(is_terminal_event)
}

/// Return the status carried by a decoded JSON event.
///
/// Responses events commonly put status at `response.status`; a top-level
/// status is supported for simpler upstream event shapes.
#[must_use]
pub fn event_status(value: &Value) -> Option<&str> {
    value
        .pointer("/response/status")
        .or_else(|| value.get("status"))
        .and_then(Value::as_str)
        .filter(|status| !status.is_empty())
}

/// Return the semantic status of a parsed JSON event.
#[must_use]
pub fn sse_event_status(event: &SseJsonEvent) -> Option<&str> {
    event_status(&event.data).or_else(|| event.event_type().and_then(default_terminal_status))
}

fn default_terminal_status(event_type: &str) -> Option<&'static str> {
    match event_type {
        "response.completed" | "response.done" => Some("completed"),
        "response.incomplete" => Some("incomplete"),
        "response.failed" | "error" => Some("failed"),
        "response.cancelled" | "response.canceled" => Some("cancelled"),
        _ => None,
    }
}

/// Extract output text from a decoded semantic event.
///
/// Delta events return their incremental `delta`; completed events return a
/// final `text` when present. For response snapshots, text is collected from
/// output message/content items.
#[must_use]
pub fn event_output(value: &Value) -> Option<String> {
    value
        .get("delta")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            value
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(ToOwned::to_owned)
        })
        .or_else(|| {
            value
                .pointer("/response/output")
                .and_then(collect_output_text)
        })
}

/// Extract output text from a parsed JSON event.
#[must_use]
pub fn sse_event_output(event: &SseJsonEvent) -> Option<String> {
    event_output(&event.data)
}

fn collect_output_text(value: &Value) -> Option<String> {
    let mut output = String::new();
    collect_output_text_into(value, &mut output);
    (!output.is_empty()).then_some(output)
}

fn collect_output_text_into(value: &Value, output: &mut String) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_output_text_into(value, output);
            }
        }
        Value::Object(object) => {
            if let Some(text) = object
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                output.push_str(text);
            }
            for key in ["content", "output"] {
                if let Some(value) = object.get(key) {
                    collect_output_text_into(value, output);
                }
            }
        }
        Value::String(_) | Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Accumulated terminal/status/output facts for one SSE stream.
///
/// The tracker is intentionally independent from middleware and transport
/// concerns. It can observe parsed events while the original bytes continue
/// to be forwarded unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseStreamTracker {
    /// Whether a terminal event or `[DONE]` sentinel has been observed.
    pub terminal: bool,
    /// The latest observed response status, if any.
    pub status: Option<String>,
    /// Accumulated visible output text.
    pub output: String,
    /// Number of dispatched events observed, including control events.
    pub event_count: usize,
}

/// Semantic-state alias for callers that do not need the word “tracker”.
pub type SseStreamState = SseStreamTracker;
pub type SseSemanticState = SseStreamTracker;
pub type SseEventTracker = SseStreamTracker;

impl SseStreamTracker {
    /// Create an empty stream tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe a parsed SSE event and return the resulting terminal flag.
    pub fn observe(&mut self, event: &SseEvent) -> bool {
        self.event_count = self.event_count.saturating_add(1);
        if event.is_done() {
            self.terminal = true;
            return self.terminal;
        }
        let Ok(json_event) = parse_json_event(event) else {
            return self.terminal;
        };
        self.observe_json(&json_event)
    }

    /// Observe an already-decoded JSON event and return the resulting terminal
    /// flag.
    pub fn observe_json(&mut self, event: &SseJsonEvent) -> bool {
        self.observe_value(event.event_type(), &event.data)
    }

    /// Observe an event type and JSON value without allocating an intermediate
    /// [`SseJsonEvent`].
    pub fn observe_value(&mut self, event_type: Option<&str>, value: &Value) -> bool {
        if let Some(status) = event_status(value) {
            self.status = Some(status.to_owned());
        }

        let event_type = event_type.or_else(|| value.get("type").and_then(Value::as_str));
        if let Some(event_type) = event_type {
            if let Some(status) = default_terminal_status(event_type)
                && self.status.is_none()
            {
                self.status = Some(status.to_owned());
            }
            self.terminal |= is_terminal_event(event_type);
            self.observe_output(event_type, value);
        }
        self.terminal
    }

    /// Alias for [`Self::observe`].
    pub fn track(&mut self, event: &SseEvent) -> bool {
        self.observe(event)
    }

    /// Whether the stream has reached a terminal state.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Borrow the latest observed status.
    #[must_use]
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Borrow the accumulated output text.
    #[must_use]
    pub fn output_text(&self) -> &str {
        &self.output
    }

    /// Reset all observed facts for reuse with another stream.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn observe_output(&mut self, event_type: &str, value: &Value) {
        let is_delta = matches!(
            event_type,
            "response.output_text.delta" | "response.refusal.delta"
        );
        if is_delta {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                self.output.push_str(delta);
            }
            return;
        }

        let is_text_done = matches!(
            event_type,
            "response.output_text.done" | "response.refusal.done"
        );
        if is_text_done {
            if let Some(text) = value
                .get("text")
                .or_else(|| value.get("refusal"))
                .and_then(Value::as_str)
            {
                reconcile_output(&mut self.output, text);
            }
            return;
        }

        if matches!(event_type, "response.completed" | "response.incomplete")
            && self.output.is_empty()
            && let Some(text) = value
                .pointer("/response/output")
                .and_then(collect_output_text)
        {
            self.output = text;
        }
    }
}

fn reconcile_output(output: &mut String, complete: &str) {
    if output.is_empty() || complete.starts_with(output.as_str()) {
        *output = complete.to_owned();
    }
}

/// Apply one parsed event to a tracker.
pub fn track_sse_event(tracker: &mut SseStreamTracker, event: &SseEvent) -> bool {
    tracker.observe(event)
}

/// Apply one parsed JSON event to a tracker.
pub fn track_sse_json_event(tracker: &mut SseStreamTracker, event: &SseJsonEvent) -> bool {
    tracker.observe_json(event)
}

/// Incremental SSE frame splitter: split an upstream byte stream at blank
/// lines while preserving the exact wire representation.
///
/// Frame bytes retain the original wire shape (including `event:`/`data:`
/// lines and the separator blank line), matching the host provider's raw SSE
/// frame contract.
#[derive(Default)]
pub struct SseFrameSplitter {
    buffer: Vec<u8>,
}

/// A complete frame with its original bytes; `done` marks a `data: [DONE]`
/// terminal frame.
pub struct SseFrame {
    pub bytes: Vec<u8>,
    pub done: bool,
}

impl SseFrameSplitter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed an upstream byte chunk and return all completed frames.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some((split, separator)) = find_separator(&self.buffer) {
            let frame: Vec<u8> = self.buffer.drain(..split + separator).collect();
            let done = frame_is_done(&frame);
            frames.push(SseFrame { bytes: frame, done });
        }
        frames
    }

    /// At stream end, return residual bytes as an incomplete frame.
    pub fn finish(&mut self) -> Option<SseFrame> {
        if self.buffer.is_empty() {
            return None;
        }
        let bytes = std::mem::take(&mut self.buffer);
        Some(SseFrame {
            done: frame_is_done(&bytes),
            bytes,
        })
    }
}

fn find_separator(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut lf = None;
    for window in buffer.windows(2).enumerate() {
        if window.1 == b"\n\n" {
            lf = Some((window.0, 2));
            break;
        }
    }
    let mut crlf = None;
    for window in buffer.windows(4).enumerate() {
        if window.1 == b"\r\n\r\n" {
            crlf = Some((window.0, 4));
            break;
        }
    }
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(found), None) | (None, Some(found)) => Some(found),
        (None, None) => None,
    }
}

/// A frame is done only when every data line is `[DONE]`.
fn frame_is_done(frame: &[u8]) -> bool {
    let text = String::from_utf8_lossy(frame);
    let mut saw_data = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(data) = line.strip_prefix("data:") {
            saw_data = true;
            if data.trim() != "[DONE]" {
                return false;
            }
        }
    }
    saw_data
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_lf_and_multiline_data() {
        let event = parse_sse_event(
            b"event: response.output_text.delta\ndata: {\"delta\":\"hello\"}\ndata: world\n\n",
        )
        .expect("one event");
        assert_eq!(event.event.as_deref(), Some("response.output_text.delta"));
        assert_eq!(event.data, "{\"delta\":\"hello\"}\nworld");
    }

    #[test]
    fn parses_crlf_and_ignores_comments_and_unknown_fields() {
        let event = parse_sse_event(
            b": keep-alive\r\nevent: response.created\r\nretry: 100\r\ndata: {\"ok\":true}\r\n\r\n",
        )
        .expect("one event");
        assert_eq!(event.event_name(), Some("response.created"));
        assert_eq!(event.json().expect("json"), json!({"ok": true}));
    }

    #[test]
    fn parser_dispatches_multiple_events_and_data_only_events() {
        let events = parse_sse_events(b"event: one\ndata: a\n\ndata: b\n\n");
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: Some("one".to_owned()),
                    data: "a".to_owned(),
                },
                SseEvent {
                    event: None,
                    data: "b".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn json_event_uses_sse_name_before_json_type() {
        let event = parse_sse_json_event(
            b"event: response.completed\ndata: {\"type\":\"other\",\"status\":\"completed\"}\n\n",
        )
        .expect("valid json")
        .expect("event");
        assert_eq!(event.event_type(), Some("response.completed"));
        assert!(event.is_terminal());
        assert_eq!(sse_event_status(&event), Some("completed"));
    }

    #[test]
    fn done_event_is_terminal_but_not_json() {
        assert!(is_terminal_sse_event(
            &parse_sse_event(b"data: [DONE]\n\n").expect("done")
        ));
        assert!(
            parse_sse_json_event(b"data: [DONE]\n\n")
                .expect("valid done frame")
                .is_none()
        );
    }

    #[test]
    fn tracker_collects_deltas_status_and_terminal_snapshot() {
        let mut tracker = SseStreamTracker::new();
        assert!(
            !tracker.observe(
                &parse_sse_event(
                    b"event: response.output_text.delta\ndata: {\"delta\":\"hello \"}\n\n"
                )
                .expect("delta")
            )
        );
        assert!(
            !tracker.observe(
                &parse_sse_event(
                    b"event: response.output_text.delta\ndata: {\"delta\":\"world\"}\n\n"
                )
                .expect("delta")
            )
        );
        assert!(tracker.observe(
            &parse_sse_event(
                b"event: response.completed\ndata: {\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello world\"}]}]}}\n\n"
            )
            .expect("terminal")
        ));
        assert_eq!(tracker.status(), Some("completed"));
        assert_eq!(tracker.output_text(), "hello world");
        assert_eq!(tracker.event_count, 3);
    }

    #[test]
    fn tracker_can_use_data_only_json_type_and_reset() {
        let mut tracker = SseStreamTracker::default();
        assert!(
            tracker.observe(
                &parse_sse_event(
                    b"data: {\"type\":\"response.failed\",\"error\":{\"message\":\"no\"}}\n\n"
                )
                .expect("failure")
            )
        );
        assert_eq!(tracker.status(), Some("failed"));
        tracker.reset();
        assert_eq!(tracker, SseStreamTracker::default());
    }

    #[test]
    fn existing_splitter_behavior_remains_unchanged() {
        let mut splitter = SseFrameSplitter::new();
        let mut frames = splitter.feed(b"data: first\n\nevent: next\ndata: {\"x\":");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames.remove(0).bytes, b"data: first\n\n");
        frames.extend(splitter.feed(b"1}\r\n\r\n"));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].bytes, b"event: next\ndata: {\"x\":1}\r\n\r\n");
        assert!(!frames[0].done);
    }
}
