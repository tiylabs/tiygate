use serde_json::json;
use tiygate_core::{EndpointCodec, RawEnvelope};
use tiygate_protocols::{
    chat_completions::ChatCompletionsCodec, messages::MessagesCodec, responses::ResponsesCodec,
};
fn env() -> RawEnvelope {
    RawEnvelope {
        method: "POST".into(),
        path: "/test".into(),
        headers: Default::default(),
        body: None,
        original_body_size: 0,
        timestamp: chrono::Utc::now(),
    }
}
#[test]
fn messages_document_preserved_or_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let src = MessagesCodec::new();
    let dst = ResponsesCodec::new();
    let decoded=src.decode_request(json!({"model":"m","max_tokens":100,"messages":[{"role":"user","content":[{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0x"}},{"type":"text","text":"summarize attached"}]}]}),&env());
    if let Ok(ir) = decoded {
        if tiygate_core::protocol::lossy::check_lossy_conversion(&ir, dst.id(), dst.capabilities())
            .is_ok()
        {
            let out = dst.encode_request(&ir)?.0;
            println!("{out}");
            assert!(
                out.to_string().contains("JVBERi0x"),
                "document vanished before guard could classify it"
            );
        }
    }
    Ok(())
}
#[test]
fn responses_conversation_preserved_on_reencode() -> Result<(), Box<dyn std::error::Error>> {
    let codec = ResponsesCodec::new();
    let ir = codec.decode_request(
        json!({"model":"m","input":"continue","conversation":"conv_123"}),
        &env(),
    )?;
    let out = codec.encode_request(&ir)?.0;
    println!("{out}");
    assert_eq!(out["conversation"], "conv_123");
    Ok(())
}
#[test]
fn chat_parallel_false_to_messages_preserved_or_rejected() -> Result<(), Box<dyn std::error::Error>>
{
    let src = ChatCompletionsCodec::new();
    let dst = MessagesCodec::new();
    let ir=src.decode_request(json!({"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],"parallel_tool_calls":false}),&env())?;
    if tiygate_core::protocol::lossy::check_lossy_conversion(&ir, dst.id(), dst.capabilities())
        .is_ok()
    {
        let out = dst.encode_request(&ir)?.0;
        println!("{out}");
        assert_eq!(out["tool_choice"]["disable_parallel_tool_use"], true);
    }
    Ok(())
}
