use serde_json::Value;
use tiygate_core::EndpointCodec;
use tiygate_protocols::{gemini::GeminiCodec, responses::ResponsesCodec};
#[test]
fn earlier_usage_does_not_seal_response_before_final_usage(
) -> Result<(), Box<dyn std::error::Error>> {
    let mut d = GeminiCodec::new().stream_decoder();
    let mut e = ResponsesCodec::new().stream_encoder();
    let mut out = Vec::new();
    for line in [
        r#"data: {"responseId":"r","candidates":[{"content":{"parts":[{"text":"hi"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1,"totalTokenCount":11}}"#,
        r#"data: {"candidates":[{"content":{"parts":[{"text":" more"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
    ] {
        for part in d.feed(line)? {
            out.extend(e.encode_part(&part)?);
        }
    }
    for part in d.finish()? {
        out.extend(e.encode_part(&part)?);
    }
    let wire = String::from_utf8(out)?;
    println!("{wire}");
    let completed: Vec<Value> = wire
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["type"] == "response.completed")
        .collect();
    assert_eq!(completed.len(), 1);
    assert_eq!(
        completed[0]["response"]["usage"]["output_tokens"], 5,
        "terminal output must use final upstream accounting"
    );
    Ok(())
}
