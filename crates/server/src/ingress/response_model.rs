//! Client-facing model identity normalization.
//!
//! Routing may replace a client's virtual model with a concrete upstream
//! model.  That concrete id must never leak back through a successful
//! response: clients should consistently see the model they asked for.

use serde_json::{json, Value};
use tiygate_core::ProtocolSuite;

/// The client-facing model identity and wire protocol used to express it.
#[derive(Clone, Debug)]
pub(super) struct ResponseModelOverride {
    suite: ProtocolSuite,
    model: String,
}

impl ResponseModelOverride {
    pub(super) fn new(suite: ProtocolSuite, model: impl Into<String>) -> Self {
        Self {
            suite,
            model: model.into(),
        }
    }

    /// Apply the model identity to a complete non-streaming response body.
    pub(super) fn apply_json(&self, body: &mut Value) {
        set_response_model(body, self.suite, &self.model);
    }

    /// Create a stateful rewriter for SSE bytes. A network chunk need not end
    /// on an SSE line boundary, so callers must retain this for a stream's
    /// entire lifetime and call [`finish`](SseModelRewriter::finish) at EOF.
    pub(super) fn sse_rewriter(&self) -> SseModelRewriter {
        SseModelRewriter {
            override_: self.clone(),
            pending: Vec::new(),
        }
    }
}

/// Rewrite model fields in SSE `data:` lines while retaining partial lines
/// until their terminating newline arrives.
pub(super) struct SseModelRewriter {
    override_: ResponseModelOverride,
    pending: Vec<u8>,
}

impl SseModelRewriter {
    pub(super) fn rewrite(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let mut output = Vec::with_capacity(self.pending.len());

        while let Some(newline) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=newline).collect();
            output.extend(rewrite_sse_line(&line, &self.override_));
        }

        output
    }

    pub(super) fn finish(&mut self) -> Vec<u8> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let line = std::mem::take(&mut self.pending);
        rewrite_sse_line(&line, &self.override_)
    }
}

fn set_response_model(body: &mut Value, suite: ProtocolSuite, model: &str) {
    let Some(object) = body.as_object_mut() else {
        return;
    };

    match suite {
        ProtocolSuite::OpenAiCompatible => {
            object.insert("model".to_string(), json!(model));
        }
        ProtocolSuite::OpenAiResponses => {
            // Non-streaming Responses bodies carry the model directly. SSE
            // lifecycle events are handled by `set_stream_response_model`.
            object.insert("model".to_string(), json!(model));
        }
        ProtocolSuite::AnthropicMessages => {
            object.insert("model".to_string(), json!(model));
        }
        ProtocolSuite::GoogleGemini => {
            object.insert("modelVersion".to_string(), json!(model));
        }
    }
}

fn set_stream_response_model(body: &mut Value, suite: ProtocolSuite, model: &str) {
    // Error frames are not model responses. Preserve their wire shape so
    // provider-specific clients can continue to inspect error payloads.
    if body.get("error").is_some_and(|error| !error.is_null()) {
        return;
    }

    if suite == ProtocolSuite::OpenAiResponses {
        if let Some(response) = body.get_mut("response") {
            set_response_model(response, suite, model);
        }
        return;
    }

    if suite == ProtocolSuite::AnthropicMessages {
        if body.get("type").and_then(Value::as_str) == Some("message_start") {
            if let Some(message) = body.get_mut("message") {
                set_response_model(message, suite, model);
            }
        }
        return;
    }

    set_response_model(body, suite, model);
}

fn rewrite_sse_line(line: &[u8], override_: &ResponseModelOverride) -> Vec<u8> {
    let Some((prefix_end, newline_start)) = data_line_bounds(line) else {
        return line.to_vec();
    };
    let payload = &line[prefix_end..newline_start];
    let Ok(mut body) = serde_json::from_slice::<Value>(trim_ascii_whitespace(payload)) else {
        return line.to_vec();
    };

    set_stream_response_model(&mut body, override_.suite, &override_.model);
    let Ok(encoded) = serde_json::to_vec(&body) else {
        return line.to_vec();
    };

    let mut output = Vec::with_capacity(prefix_end + encoded.len() + line.len() - newline_start);
    output.extend_from_slice(&line[..prefix_end]);
    output.extend_from_slice(&encoded);
    output.extend_from_slice(&line[newline_start..]);
    output
}

/// Return the byte range after `data:` and before CR/LF for an SSE data line.
fn data_line_bounds(line: &[u8]) -> Option<(usize, usize)> {
    let mut prefix_end = 0;
    while prefix_end < line.len() && matches!(line[prefix_end], b' ' | b'\t') {
        prefix_end += 1;
    }
    if !line[prefix_end..].starts_with(b"data:") {
        return None;
    }
    prefix_end += b"data:".len();
    if line.get(prefix_end) == Some(&b' ') {
        prefix_end += 1;
    }
    let newline_start = line
        .iter()
        .position(|b| *b == b'\r' || *b == b'\n')
        .unwrap_or(line.len());
    Some((prefix_end, newline_start))
}

fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_all_nonstreaming_model_shapes() {
        let cases = [
            (
                ProtocolSuite::OpenAiCompatible,
                json!({"model": "upstream"}),
                "model",
            ),
            (
                ProtocolSuite::OpenAiResponses,
                json!({"model": "upstream"}),
                "model",
            ),
            (
                ProtocolSuite::AnthropicMessages,
                json!({"model": "upstream"}),
                "model",
            ),
            (
                ProtocolSuite::GoogleGemini,
                json!({"modelVersion": "upstream"}),
                "modelVersion",
            ),
        ];

        for (suite, mut body, field) in cases {
            ResponseModelOverride::new(suite, "virtual/model").apply_json(&mut body);
            assert_eq!(body[field], "virtual/model");
        }
    }

    #[test]
    fn rewrites_fragmented_responses_sse_lifecycle_events() {
        let override_ = ResponseModelOverride::new(ProtocolSuite::OpenAiResponses, "virtual/model");
        let mut rewriter = override_.sse_rewriter();
        let first =
            rewriter.rewrite(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",");
        assert!(first.is_empty());
        let second = rewriter.rewrite(b"\"model\":\"upstream\"}}\n\ndata: [DONE]\n\n");
        let output = String::from_utf8(second).unwrap_or_default();
        assert!(output.contains("\"model\":\"virtual/model\""));
        assert!(output.contains("data: [DONE]"));
    }

    #[test]
    fn rewrites_protocol_specific_sse_locations() {
        let cases = [
            (
                ProtocolSuite::OpenAiCompatible,
                b"data: {\"id\":\"c1\",\"choices\":[]}\n\n".as_slice(),
                "\"model\":\"virtual/model\"",
            ),
            (
                ProtocolSuite::AnthropicMessages,
                b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"upstream\"}}\n\n".as_slice(),
                "\"model\":\"virtual/model\"",
            ),
            (
                ProtocolSuite::GoogleGemini,
                b"data: {\"candidates\":[],\"modelVersion\":\"upstream\"}\n\n".as_slice(),
                "\"modelVersion\":\"virtual/model\"",
            ),
        ];

        for (suite, input, expected) in cases {
            let mut rewriter = ResponseModelOverride::new(suite, "virtual/model").sse_rewriter();
            let output = String::from_utf8(rewriter.rewrite(input)).unwrap_or_default();
            assert!(output.contains(expected), "{output}");
        }
    }

    #[test]
    fn leaves_sse_error_frames_unchanged() {
        let input = b"data: {\"error\":{\"message\":\"upstream failed\"}}\n\n";
        let mut rewriter =
            ResponseModelOverride::new(ProtocolSuite::OpenAiCompatible, "virtual/model")
                .sse_rewriter();
        assert_eq!(rewriter.rewrite(input), input);
    }

    #[test]
    fn rewrites_success_frame_with_null_error() {
        let input =
            b"data: {\"id\":\"c1\",\"model\":\"upstream\",\"error\":null,\"choices\":[]}\n\n";
        let mut rewriter =
            ResponseModelOverride::new(ProtocolSuite::OpenAiCompatible, "virtual/model")
                .sse_rewriter();
        let output = String::from_utf8(rewriter.rewrite(input)).unwrap_or_default();
        assert!(output.contains("\"model\":\"virtual/model\""), "{output}");
        assert!(output.contains("\"error\":null"), "{output}");
    }
}
