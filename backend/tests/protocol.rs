use cpr_plugin_oai_basispoints::config::RuntimeConfig;
use cpr_plugin_oai_basispoints::protocol::{prepare_body, prepare_request};
use cpr_plugin_oai_basispoints::sse::SseFrameSplitter;
use serde_json::{Value, json};

fn config() -> RuntimeConfig {
    RuntimeConfig::from_configuration(&json!({
        "responsesUrl": "https://bps.openai.com/basispoints/api/responses",
        "models": [
            {"alias": "gpt-6-astra-basispoints", "upstreamModel": "gpt-6-astra"},
            {"alias": "gpt-5.6-sol-basispoints", "upstreamModel": "gpt-5.6-sol"}
        ]
    }))
    .unwrap()
}

#[test]
fn config_parses_defaults_and_resolves_aliases() {
    let config = config();
    assert_eq!(
        config.resolve_upstream("gpt-6-astra-basispoints"),
        Some("gpt-6-astra")
    );
    assert_eq!(
        config.resolve_upstream("gpt-5.6-sol-basispoints"),
        Some("gpt-5.6-sol")
    );
    assert_eq!(config.resolve_upstream("gpt-5.3-codex"), None);
    assert_eq!(config.max_response_bytes, 64 * 1024 * 1024);
}

#[test]
fn config_rejects_duplicates_and_http_urls() {
    let error = RuntimeConfig::from_configuration(&json!({
        "responsesUrl": "http://bps.openai.com/x",
        "models": [{"alias": "a", "upstreamModel": "b"}]
    }))
    .unwrap_err();
    assert!(error.0.contains("https"));
    let error = RuntimeConfig::from_configuration(&json!({
        "responsesUrl": "https://bps.openai.com/x",
        "models": [
            {"alias": "a", "upstreamModel": "b"},
            {"alias": "a", "upstreamModel": "c"}
        ]
    }))
    .unwrap_err();
    assert!(error.0.contains("重复"));
}

#[test]
fn prepare_maps_model_and_rejects_continuation() {
    let config = config();
    let source = json!({
        "model": "gpt-6-astra-basispoints",
        "input": "hello",
        "previous_response_id": "resp_123"
    })
    .as_object()
    .unwrap()
    .clone();
    let error = prepare_body(&source, "gpt-6-astra", false).unwrap_err();
    assert_eq!(error.status, 400);
    assert_eq!(error.code, "unsupported_continuation");
    assert_eq!(
        config.resolve_upstream("gpt-6-astra-basispoints"),
        Some("gpt-6-astra")
    );
}

#[test]
fn prepare_builds_standard_chat_body_with_turn_metadata() {
    let source = json!({
        "model": "gpt-5.6-sol-basispoints",
        "instructions": "be brief",
        "input": "hi",
        "reasoning": {"effort": "MAX"},
        "prompt_cache_key": "session-1"
    })
    .as_object()
    .unwrap()
    .clone();
    let body: Value =
        serde_json::from_slice(&prepare_body(&source, "gpt-5.6-sol", true).unwrap()).unwrap();
    assert_eq!(body["model"], "gpt-5.6-sol");
    assert_eq!(body["model_selection"], "explicit");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["reasoning_effort"], "xhigh");
    assert_eq!(body["prompt_cache_key"], "session-1");
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["role"], "developer");
    assert_eq!(input[0]["content"][0]["text"], "be brief");
    assert_eq!(input[1]["role"], "user");
    assert_eq!(input[1]["content"][0]["type"], "input_text");
    let metadata = body["metadata"].as_object().unwrap();
    assert!(metadata["task_id"].as_str().unwrap().len() == 36);
    assert!(metadata["turn_id"].as_str().unwrap().len() == 36);
    assert_eq!(metadata["agent_iteration"], "1");
}

#[test]
fn prepare_accepts_relay_tools_and_preserves_guards() {
    let mut source = json!({
        "input": "hi",
        "tools": [{
            "type": "function",
            "name": "exec",
            "description": "Run a command",
            "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
        }],
        "tool_choice": "auto"
    })
    .as_object()
    .unwrap()
    .clone();
    let prepared = prepare_request(&source, "gpt-6-astra", false).unwrap();
    let body: Value = serde_json::from_slice(&prepared.body).unwrap();
    assert!(prepared.relay.is_some());
    assert!(body.get("tools").is_none());
    assert_eq!(body["input"][0]["role"], "developer");
    assert!(
        body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("run_officejs")
    );
    assert_eq!(body["input"][1]["role"], "user");

    for (extra, code) in [
        (
            json!({"service_tier": "priority"}),
            "unsupported_service_tier",
        ),
        (
            json!({"text": {"format": {"type": "json_object"}}}),
            "unsupported_text_format",
        ),
    ] {
        for (key, value) in extra.as_object().unwrap() {
            source.insert(key.clone(), value.clone());
        }
        let error = prepare_body(&source, "gpt-6-astra", false).unwrap_err();
        assert_eq!(error.code, code, "case {code}");
        assert_eq!(error.status, 400);
        for key in extra.as_object().unwrap().keys() {
            source.remove(key);
        }
    }
}

#[test]
fn prepare_treats_empty_tools_and_null_choice_as_no_relay() {
    let source = json!({"input": "hi", "tools": [], "tool_choice": null})
        .as_object()
        .unwrap()
        .clone();
    let prepared = prepare_request(&source, "gpt-6-astra", false).unwrap();
    let body: Value = serde_json::from_slice(&prepared.body).unwrap();
    assert!(prepared.relay.is_none());
    assert_eq!(body["input"].as_array().unwrap().len(), 1);
    assert_eq!(body["input"][0]["role"], "user");
}

#[test]
fn prepare_relay_rejects_invalid_tool_directory_and_continuation() {
    let source = json!({
        "input": "hi",
        "tools": [{"type": "function"}]
    })
    .as_object()
    .unwrap()
    .clone();
    let error = prepare_body(&source, "gpt-6-astra", false).unwrap_err();
    assert_eq!(error.code, "invalid_tool_directory");
    assert_eq!(error.status, 400);

    let source = json!({"input": "hi", "previous_response_id": "resp_123"})
        .as_object()
        .unwrap()
        .clone();
    let error = prepare_body(&source, "gpt-6-astra", false).unwrap_err();
    assert_eq!(error.code, "unsupported_continuation");
    assert_eq!(error.status, 400);
}

#[test]
fn prepare_translates_reasoning_and_drops_references() {
    let source = json!({
        "input": [
            {"type": "reasoning", "summary": [{"text": "s"}], "encrypted_content": "enc"},
            {"type": "reasoning", "summary": []},
            {"type": "item_reference", "id": "itm_1"},
            {"role": "user", "content": "next"}
        ]
    })
    .as_object()
    .unwrap()
    .clone();
    let body: Value =
        serde_json::from_slice(&prepare_body(&source, "gpt-6-astra", false).unwrap()).unwrap();
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["type"], "reasoning");
    assert_eq!(input[0]["summary"], json!([]));
    assert_eq!(input[0]["encrypted_content"], "enc");
    assert_eq!(input[1]["role"], "user");
}

#[test]
fn turn_iteration_counts_tool_outputs_after_last_user() {
    let source = json!({
        "input": [
            {"role": "user", "content": "q"},
            {"type": "function_call", "call_id": "c1", "name": "f"},
            {"type": "function_call_output", "call_id": "c1", "output": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "{}"},
            {"role": "user", "content": "again"},
            {"type": "function_call_output", "call_id": "c2", "output": "{}"}
        ]
    })
    .as_object()
    .unwrap()
    .clone();
    let body: Value =
        serde_json::from_slice(&prepare_body(&source, "gpt-6-astra", false).unwrap()).unwrap();
    assert_eq!(body["metadata"]["agent_iteration"], "2");
    // 相同用户 turn 前缀产生稳定 turn_id。
    let again: Value =
        serde_json::from_slice(&prepare_body(&source, "gpt-6-astra", false).unwrap()).unwrap();
    assert_eq!(body["metadata"]["turn_id"], again["metadata"]["turn_id"]);
}

#[test]
fn sse_splitter_emits_complete_frames_and_marks_done() {
    let mut splitter = SseFrameSplitter::new();
    let mut frames = splitter.feed(
        b"event: response.created\ndata: {\"a\":1}\n\nevent: response.output_text.delta\ndata: {\"b\":",
    );
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames.remove(0).bytes,
        b"event: response.created\ndata: {\"a\":1}\n\n"
    );
    frames.extend(splitter.feed(b"2}\n\n"));
    assert_eq!(frames.len(), 1);
    let second = frames.remove(0);
    assert_eq!(
        second.bytes,
        b"event: response.output_text.delta\ndata: {\"b\":2}\n\n"
    );
    assert!(!second.done);
    frames.extend(splitter.feed(b"data: [DONE]\n"));
    assert!(frames.is_empty());
    frames.extend(splitter.feed(b"\n"));
    let done = frames.remove(0);
    assert!(done.done);
    assert!(splitter.finish().is_none());
}

#[test]
fn sse_splitter_handles_crlf_and_partial_boundaries() {
    let mut splitter = SseFrameSplitter::new();
    let frames = splitter.feed(b"data: x\r\n\r\ndata: y");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].bytes, b"data: x\r\n\r\n");
    let tail = splitter.finish().unwrap();
    assert_eq!(tail.bytes, b"data: y");
    assert!(!tail.done);
}
