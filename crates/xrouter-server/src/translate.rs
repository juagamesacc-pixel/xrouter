use serde_json::{Value, json};

/// Extract model string via fast peek (serde_json value)
pub fn peek_model(body: &[u8]) -> Option<String> {
    // minimal parse: look for "model" field
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
            return Some(m.to_string());
        }
    }
    None
}

pub fn is_streaming(body: &Value) -> bool {
    body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Translate OpenAI request body to Anthropic format
pub fn openai_to_anthropic(body: &Value, target_model: &str) -> Value {
    let mut out = json!({});
    out["model"] = Value::String(target_model.to_string());
    // messages
    let mut messages = Vec::new();
    let mut system_parts: Vec<String> = Vec::new();

    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            if role == "system" {
                if let Some(content) = m.get("content") {
                    if let Some(s) = content.as_str() { system_parts.push(s.to_string()); }
                    else if let Some(arr) = content.as_array() {
                        for part in arr {
                            if let Some(t) = part.get("text").and_then(|x| x.as_str()) { system_parts.push(t.to_string()); }
                            else if let Some(s) = part.as_str() { system_parts.push(s.to_string()); }
                        }
                    }
                }
                continue;
            }
            // For openai messages, pass through but normalize content
            let mut new_msg = json!({"role": role});
            if let Some(content) = m.get("content") {
                // OpenAI may have string or array of content parts
                if content.is_string() {
                    new_msg["content"] = content.clone();
                } else if let Some(arr) = content.as_array() {
                    // convert to anthropic content array: preserve text parts
                    let mut parts = Vec::new();
                    for p in arr {
                        if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                            parts.push(json!({"type":"text","text": t}));
                        } else if let Some(s) = p.as_str() {
                            parts.push(json!({"type":"text","text": s}));
                        } else {
                            parts.push(p.clone());
                        }
                    }
                    new_msg["content"] = Value::Array(parts);
                } else {
                    new_msg["content"] = content.clone();
                }
            }
            // tool calls etc pass through minimally
            if let Some(tc) = m.get("tool_calls") { new_msg["tool_calls"] = tc.clone(); }
            messages.push(new_msg);
        }
    }
    if !system_parts.is_empty() {
        out["system"] = Value::String(system_parts.join("\n"));
    }
    out["messages"] = Value::Array(messages);
    // max_tokens required for Anthropic
    if let Some(mt) = body.get("max_tokens") { out["max_tokens"] = mt.clone(); }
    else if let Some(mt) = body.get("max_completion_tokens") { out["max_tokens"] = mt.clone(); }
    else { out["max_tokens"] = json!(1024); }

    // optional fields
    if let Some(t) = body.get("temperature") { out["temperature"] = t.clone(); }
    if let Some(t) = body.get("top_p") { out["top_p"] = t.clone(); }
    if let Some(s) = body.get("stop") { out["stop_sequences"] = s.clone(); }
    if let Some(s) = body.get("stream") { out["stream"] = s.clone(); }
    // tools
    if let Some(tools) = body.get("tools") { out["tools"] = tools.clone(); }

    out
}

/// Translate Anthropic request body to OpenAI format
pub fn anthropic_to_openai(body: &Value, target_model: &str) -> Value {
    let mut out = json!({});
    out["model"] = Value::String(target_model.to_string());
    let mut messages: Vec<Value> = Vec::new();

    // system -> first message with role system
    if let Some(sys) = body.get("system") {
        let text = if let Some(s) = sys.as_str() { s.to_string() }
        else if let Some(arr) = sys.as_array() {
            arr.iter().filter_map(|v| v.get("text").and_then(|x| x.as_str())).collect::<Vec<_>>().join("\n")
        } else { sys.to_string() };
        if !text.is_empty() {
            messages.push(json!({"role":"system","content": text}));
        }
    }

    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let mut new_msg = json!({"role": role});
            if let Some(content) = m.get("content") {
                if let Some(s) = content.as_str() {
                    new_msg["content"] = Value::String(s.to_string());
                } else if let Some(arr) = content.as_array() {
                    // anthropic content blocks
                    let mut parts = Vec::new();
                    for block in arr {
                        if let Some(t) = block.get("text").and_then(|x| x.as_str()) {
                            parts.push(json!({"type":"text","text": t}));
                        } else {
                            parts.push(block.clone());
                        }
                    }
                    // OpenAI expects string or array; keep array if multiple parts else string
                    if parts.len() == 1 && parts[0].get("text").is_some() {
                        new_msg["content"] = Value::String(parts[0].get("text").unwrap().as_str().unwrap().to_string());
                    } else {
                        new_msg["content"] = Value::Array(parts);
                    }
                }
            }
            messages.push(new_msg);
        }
    }
    out["messages"] = Value::Array(messages);
    if let Some(mt) = body.get("max_tokens") { out["max_tokens"] = mt.clone(); }
    if let Some(t) = body.get("temperature") { out["temperature"] = t.clone(); }
    if let Some(s) = body.get("stop_sequences") { out["stop"] = s.clone(); }
    if let Some(s) = body.get("stream") { out["stream"] = s.clone(); }
    if let Some(tools) = body.get("tools") { out["tools"] = tools.clone(); }

    out
}

/// Translate SSE chunk OpenAI -> Anthropic style (simplified)
///
/// For streaming translation, we operate chunk-wise.
/// Input is an OpenAI chunk line like `data: {...}` or `data: [DONE]`
/// Output is Anthropic event lines.
/// This is a minimal implementation: synthesize message_start, content_block_delta, message_stop.
pub fn translate_sse_openai_to_anthropic_chunk(openai_data: &str) -> Vec<String> {
    // openai_data is JSON string from delta
    if openai_data.trim() == "[DONE]" { return vec!["event: message_stop\ndata: {\"type\":\"message_stop\"}".to_string()]; }
    let v: Result<Value, _> = serde_json::from_str(openai_data);
    if let Ok(val) = v {
        if let Some(choices) = val.get("choices").and_then(|c| c.as_array()) {
            if let Some(choice) = choices.first() {
                if let Some(delta) = choice.get("delta") {
                    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                        let anth = json!({
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": { "type": "text_delta", "text": content }
                        });
                        return vec![format!("event: content_block_delta\ndata: {}", anth)];
                    }
                    // tool call delta etc omitted for brevity
                }
                if let Some(finish) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                    if finish != "null" {
                        return vec!["event: message_stop\ndata: {\"type\":\"message_stop\"}".to_string()];
                    }
                }
            }
        }
    }
    vec![]
}

pub fn translate_sse_anthropic_to_openai_chunk(anth_event: &str, anth_data: &str) -> Option<String> {
    if anth_event == "content_block_delta" {
        if let Ok(v) = serde_json::from_str::<Value>(anth_data) {
            if let Some(delta) = v.get("delta").and_then(|d| d.get("text")).and_then(|t| t.as_str()) {
                let openai = json!({
                    "id": "chatcmpl-translated",
                    "object": "chat.completion.chunk",
                    "choices": [{ "index": 0, "delta": { "content": delta }, "finish_reason": null }]
                });
                return Some(format!("data: {}\n", openai));
            }
        }
    } else if anth_event == "message_stop" {
        return Some("data: [DONE]\n".to_string());
    } else if anth_event == "message_start" {
        // synthesize initial chunk
        let openai = json!({
            "id": "chatcmpl-translated",
            "object": "chat.completion.chunk",
            "choices": [{ "index": 0, "delta": {"role":"assistant","content":""}, "finish_reason": null }]
        });
        return Some(format!("data: {}\n", openai));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn peek_model_extract() {
        let body = br#"{"model":"big-pickle","messages":[]}"#;
        assert_eq!(peek_model(body).unwrap(), "big-pickle");
        assert!(peek_model(br#"{"messages":[]}"#).is_none());
    }
    #[test]
    fn openai_to_anthropic_basic() {
        let body = json!({"model":"big-pickle","messages":[{"role":"system","content":"you are helpful"},{"role":"user","content":"hi"}],"max_tokens":100});
        let out = openai_to_anthropic(&body, "target-model");
        assert_eq!(out["model"], "target-model");
        assert_eq!(out["system"], "you are helpful");
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["max_tokens"], 100);
    }
    #[test]
    fn anthropic_to_openai_basic() {
        let body = json!({"model":"big-pickle","system":"you are helpful","messages":[{"role":"user","content":"hi"}]});
        let out = anthropic_to_openai(&body, "target");
        assert_eq!(out["model"], "target");
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][1]["role"], "user");
    }
}
