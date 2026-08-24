use std::collections::HashMap;
use serde_json::{Value, json};

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
                        } else if p.get("type").and_then(|x| x.as_str()) == Some("image_url") {
                            // OpenAI image_url -> Anthropic image source.
                            if let Some(url) = p.get("image_url").and_then(|u| u.get("url")).and_then(|u| u.as_str()) {
                                if let Some(anth) = openai_image_url_to_anthropic(url) {
                                    parts.push(anth);
                                } else {
                                    // Unsupported/unknown url scheme: keep as-is
                                    // so the part is not silently dropped.
                                    parts.push(p.clone());
                                }
                            } else {
                                parts.push(p.clone());
                            }
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
                        } else if block.get("type").and_then(|x| x.as_str()) == Some("image") {
                            // Anthropic image -> OpenAI image_url.
                            if let Some(url) = anthropic_image_to_openai_url(block) {
                                parts.push(json!({"type":"image_url","image_url":{"url": url}}));
                            } else {
                                // Could not translate (e.g. missing source): keep
                                // the original block so nothing is dropped.
                                parts.push(block.clone());
                            }
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

/// Convert an OpenAI `image_url` value (a URL string) into an Anthropic image
/// content block. Supports both inline `data:image/<mime>;base64,<data>` URLs
/// and remote `http(s)://` URLs (Anthropic source type `url`).
fn openai_image_url_to_anthropic(url: &str) -> Option<Value> {
    if let Some(rest) = url.strip_prefix("data:") {
        // format: image/png;base64,xxxx  (or image/png;name=..;base64,xxxx)
        let (meta, data) = rest.split_once(',')?;
        let media_type = meta.split(';').next()?.to_string();
        let b64 = if let Some(b) = data.strip_prefix("base64,") { b } else { data };
        Some(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": b64 }
        }))
    } else if url.starts_with("http://") || url.starts_with("https://") {
        Some(json!({
            "type": "image",
            "source": { "type": "url", "url": url }
        }))
    } else {
        None
    }
}

/// Extract an OpenAI-compatible `image_url` string from an Anthropic image
/// content block. Inline base64 sources are re-encoded as a `data:` URL; remote
/// `url` sources are passed through directly.
fn anthropic_image_to_openai_url(block: &Value) -> Option<String> {
    let source = block.get("source")?;
    let source_type = source.get("type").and_then(|t| t.as_str())?;
    match source_type {
        "base64" => {
            let media_type = source.get("media_type").and_then(|m| m.as_str()).unwrap_or("image/png");
            let data = source.get("data").and_then(|d| d.as_str())?;
            Some(format!("data:{};base64,{}", media_type, data))
        }
        "url" => {
            let url = source.get("url").and_then(|u| u.as_str())?;
            Some(url.to_string())
        }
        _ => None,
    }
}

pub fn is_streaming(body: &Value) -> bool {
    body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Stateful SSE streaming translators
//
// Cross-protocol streaming must not lose data (especially tool calls). The
// OpenAI and Anthropic SSE dialects split a single logical event across many
// chunks, so translation carries per-stream state between chunks.
// ---------------------------------------------------------------------------

/// State for OpenAI -> Anthropic streaming translation.
#[derive(Default, Clone)]
pub struct OaToAnthStreamState {
    pub message_started: bool,
    pub tool_blocks: HashMap<usize, ToolBlock>,
    pub next_block_index: usize,
    pub finished: bool,
}

#[derive(Clone)]
pub struct ToolBlock {
    pub block_index: usize,
    pub id: String,
    pub name: String,
    pub started: bool,
}

/// State for Anthropic -> OpenAI streaming translation.
#[derive(Default, Clone)]
pub struct AnthToOaStreamState {
    /// anthropic block_index -> (openai tool index, id, name)
    pub tool_blocks: HashMap<usize, (usize, String, String)>,
    pub next_tool_index: usize,
    pub finished: bool,
}

/// Translate one OpenAI SSE `data:` payload into zero or more Anthropic events.
pub fn translate_sse_openai_to_anthropic_chunk_st(state: &mut OaToAnthStreamState, openai_data: &str) -> Vec<String> {
    if openai_data.trim() == "[DONE]" {
        let mut out = Vec::new();
        if !state.finished {
            for (_, tb) in state.tool_blocks.iter() {
                if tb.started {
                    out.push(format!("event: content_block_stop\ndata: {}", json!({"type":"content_block_stop","index": tb.block_index})));
                }
            }
            out.push("event: message_stop\ndata: {\"type\":\"message_stop\"}".to_string());
            state.finished = true;
        }
        return out;
    }
    let val: Value = match serde_json::from_str(openai_data) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let mut out = Vec::new();
    let choice = val.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first());
    let choice = match choice { Some(c) => c, None => return out };
    let delta = choice.get("delta");

    // message_start on first role
    if !state.message_started {
        if let Some(role) = delta.and_then(|d| d.get("role")).and_then(|r| r.as_str()) {
            let msg = json!({
                "type": "message_start",
                "message": {
                    "id": val.get("id").and_then(|x| x.as_str()).unwrap_or(""),
                    "role": role,
                    "model": val.get("model").and_then(|m| m.as_str()).unwrap_or(""),
                    "content": [],
                }
            });
            out.push(format!("event: message_start\ndata: {}", msg));
            state.message_started = true;
        }
    }

    // text content
    if let Some(content) = delta.and_then(|d| d.get("content")).and_then(|c| c.as_str()) {
        if !content.is_empty() {
            let anth = json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text": content}});
            out.push(format!("event: content_block_delta\ndata: {}", anth));
        }
    }

    // tool calls (may be streamed across many chunks)
    if let Some(tool_calls) = delta.and_then(|d| d.get("tool_calls")).and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            let tb = state.tool_blocks.entry(idx).or_insert_with(|| {
                let bi = state.next_block_index;
                state.next_block_index += 1;
                ToolBlock { block_index: bi, id: String::new(), name: String::new(), started: false }
            });
            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) { if !id.is_empty() { tb.id = id.to_string(); } }
            if let Some(name) = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) { if !name.is_empty() { tb.name = name.to_string(); } }
            if !tb.started {
                let block = json!({
                    "type": "content_block_start",
                    "index": tb.block_index,
                    "content_block": { "type": "tool_use", "id": tb.id, "name": tb.name }
                });
                out.push(format!("event: content_block_start\ndata: {}", block));
                tb.started = true;
            }
            if let Some(args) = tc.get("function").and_then(|f| f.get("arguments")).and_then(|a| a.as_str()) {
                if !args.is_empty() {
                    let d = json!({"type":"content_block_delta","index": tb.block_index,"delta":{"type":"input_json_delta","partial_json": args}});
                    out.push(format!("event: content_block_delta\ndata: {}", d));
                }
            }
        }
    }

    // finish_reason -> close blocks, emit message_delta + message_stop
    if let Some(finish) = choice.get("finish_reason").and_then(|r| r.as_str()) {
        if !finish.is_empty() && finish != "null" {
            for (_, tb) in state.tool_blocks.iter() {
                if tb.started {
                    out.push(format!("event: content_block_stop\ndata: {}", json!({"type":"content_block_stop","index": tb.block_index})));
                }
            }
            let stop_reason = match finish {
                "tool_calls" => "tool_use",
                "length" => "max_tokens",
                _ => "end_turn",
            };
            let msg_delta = json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason, "stop_sequence": null },
                "usage": val.get("usage").cloned().unwrap_or(json!({}))
            });
            out.push(format!("event: message_delta\ndata: {}", msg_delta));
            out.push("event: message_stop\ndata: {\"type\":\"message_stop\"}".to_string());
            state.finished = true;
        }
    }
    out
}

/// Translate one Anthropic SSE `data:` payload (with its preceding `event:`)
/// into an OpenAI chunk line (or `None`).
pub fn translate_sse_anthropic_to_openai_chunk_st(state: &mut AnthToOaStreamState, event: &str, data: &str) -> Option<String> {
    match event {
        "message_start" => {
            let v: Value = serde_json::from_str(data).ok()?;
            let role = v.get("message").and_then(|m| m.get("role")).and_then(|r| r.as_str()).unwrap_or("assistant");
            let id = v.get("message").and_then(|m| m.get("id")).and_then(|x| x.as_str()).unwrap_or("chatcmpl-translated");
            let model = v.get("message").and_then(|m| m.get("model")).and_then(|x| x.as_str()).unwrap_or("");
            let openai = json!({
                "id": id, "object": "chat.completion.chunk", "model": model,
                "choices": [{ "index": 0, "delta": { "role": role }, "finish_reason": null }]
            });
            Some(format!("data: {}\n", openai))
        }
        "content_block_start" => {
            let v: Value = serde_json::from_str(data).ok()?;
            let block = v.get("content_block");
            let block_index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            let block_type = block.and_then(|b| b.get("type")).and_then(|t| t.as_str()).unwrap_or("");
            if block_type == "tool_use" {
                let id = block.and_then(|b| b.get("id")).and_then(|x| x.as_str()).unwrap_or("").to_string();
                let name = block.and_then(|b| b.get("name")).and_then(|x| x.as_str()).unwrap_or("").to_string();
                let oa_index = state.next_tool_index;
                state.next_tool_index += 1;
                state.tool_blocks.insert(block_index, (oa_index, id.clone(), name.clone()));
                let openai = json!({
                    "id": "chatcmpl-translated", "object": "chat.completion.chunk",
                    "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": oa_index, "id": id, "type": "function", "function": { "name": name, "arguments": "" } }] }, "finish_reason": null }]
                });
                Some(format!("data: {}\n", openai))
            } else { None }
        }
        "content_block_delta" => {
            let v: Value = serde_json::from_str(data).ok()?;
            let block_index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            let delta = v.get("delta");
            if let Some(text) = delta.and_then(|d| d.get("text")).and_then(|t| t.as_str()) {
                let openai = json!({
                    "id": "chatcmpl-translated", "object": "chat.completion.chunk",
                    "choices": [{ "index": 0, "delta": { "content": text }, "finish_reason": null }]
                });
                Some(format!("data: {}\n", openai))
            } else if let Some(partial) = delta.and_then(|d| d.get("partial_json")).and_then(|p| p.as_str()) {
                let oa_index = state.tool_blocks.get(&block_index).map(|(i, _, _)| *i)?;
                let openai = json!({
                    "id": "chatcmpl-translated", "object": "chat.completion.chunk",
                    "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": oa_index, "function": { "arguments": partial } }] }, "finish_reason": null }]
                });
                Some(format!("data: {}\n", openai))
            } else { None }
        }
        "message_delta" => {
            let v: Value = serde_json::from_str(data).ok()?;
            let stop = v.get("delta").and_then(|d| d.get("stop_reason")).and_then(|s| s.as_str()).unwrap_or("stop");
            let finish_reason = match stop {
                "tool_use" => "tool_calls",
                "max_tokens" => "length",
                _ => "stop",
            };
            let usage = v.get("usage").cloned().unwrap_or(json!({}));
            let openai = json!({
                "id": "chatcmpl-translated", "object": "chat.completion.chunk",
                "choices": [{ "index": 0, "delta": {}, "finish_reason": finish_reason }],
                "usage": usage
            });
            Some(format!("data: {}\n", openai))
        }
        "message_stop" => {
            if !state.finished {
                state.finished = true;
                Some("data: [DONE]\n".to_string())
            } else { None }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// OpenAI Responses API <-> Chat Completions translation
//
// The Responses API (`POST /v1/responses`) is translated into the Chat
// Completions shape so it can ride the exact same routing/quota/failover path
// as `/v1/chat/completions`. Upstream we ALWAYS translate to
// `/v1/chat/completions` (never pass `/v1/responses` through natively) for v1
// correctness — even when a provider (e.g. opencode zen muse-spark) exposes
// `/v1/responses` natively. A future optimization could passthrough when the
// provider kind is openai-compat AND the tier entry model is known-responses.
// ---------------------------------------------------------------------------

/// Build a Responses API response object (used by both streaming and
/// non-streaming translation).
pub fn responses_object(
    id: &str,
    model: &str,
    text: &str,
    input_tokens: u64,
    output_tokens: u64,
    status: &str,
) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": 0,
        "model": model,
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
            "status": "completed"
        }],
        "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens },
        "status": status
    })
}

/// Translate an OpenAI Responses API request body into a Chat Completions body.
pub fn responses_to_chat(body: &Value) -> Value {
    let mut out = json!({});
    if let Some(m) = body.get("model").and_then(|x| x.as_str()) {
        out["model"] = Value::String(m.to_string());
    }

    let mut messages: Vec<Value> = Vec::new();

    // instructions -> system message
    if let Some(instructions) = body.get("instructions").and_then(|x| x.as_str()) {
        if !instructions.is_empty() {
            messages.push(json!({ "role": "system", "content": instructions }));
        }
    }

    // input -> messages
    match body.get("input") {
        Some(Value::String(s)) => {
            messages.push(json!({ "role": "user", "content": s.clone() }));
        }
        Some(Value::Array(arr)) => {
            for item in arr {
                let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                let mut parts: Vec<Value> = Vec::new();
                if let Some(content) = item.get("content") {
                    if let Some(s) = content.as_str() {
                        parts.push(json!({ "type": "text", "text": s }));
                    } else if let Some(arr) = content.as_array() {
                        for p in arr {
                            let ptype = p.get("type").and_then(|x| x.as_str());
                            if ptype == Some("input_text") {
                                if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                                    parts.push(json!({ "type": "text", "text": t }));
                                }
                            } else if ptype == Some("input_image") {
                                if let Some(url) = p.get("image_url").and_then(|x| x.as_str()) {
                                    parts.push(json!({ "type": "image_url", "image_url": { "url": url } }));
                                }
                            } else if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                                parts.push(json!({ "type": "text", "text": t }));
                            }
                        }
                    }
                }
                // collapse a single text part to a plain string (chat-completions friendly)
                let content_value = if parts.len() == 1
                    && parts[0].get("type").and_then(|x| x.as_str()) == Some("text")
                {
                    Value::String(parts[0].get("text").unwrap().as_str().unwrap().to_string())
                } else if parts.is_empty() {
                    Value::String(String::new())
                } else {
                    Value::Array(parts)
                };
                messages.push(json!({ "role": role, "content": content_value }));
            }
        }
        _ => {}
    }

    out["messages"] = Value::Array(messages);

    if let Some(mt) = body.get("max_output_tokens") { out["max_tokens"] = mt.clone(); }
    if let Some(s) = body.get("stream") { out["stream"] = s.clone(); }
    if let Some(t) = body.get("temperature") { out["temperature"] = t.clone(); }
    if let Some(p) = body.get("top_p") { out["top_p"] = p.clone(); }
    if let Some(tools) = body.get("tools") { out["tools"] = tools.clone(); }

    out
}

/// Translate a Chat Completions response body into a Responses API response.
pub fn chat_to_responses(v: &Value) -> Value {
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("unknown");
    let text = v.get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|ch| ch.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    let usage = v.get("usage").cloned().unwrap_or(json!({}));
    let input_tokens = usage.get("prompt_tokens").and_then(|x| x.as_u64())
        .or_else(|| usage.get("input_tokens").and_then(|x| x.as_u64()))
        .unwrap_or(0);
    let output_tokens = usage.get("completion_tokens").and_then(|x| x.as_u64())
        .or_else(|| usage.get("output_tokens").and_then(|x| x.as_u64()))
        .unwrap_or(0);
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("resp-unknown").to_string();
    responses_object(&id, model, text, input_tokens, output_tokens, "completed")
}

#[cfg(test)]
mod tests {
    use super::*;
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
    #[test]
    fn responses_to_chat_basic() {
        let body = json!({"model":"gpt","instructions":"be nice","input":"hello","max_output_tokens":50});
        let out = responses_to_chat(&body);
        assert_eq!(out["model"], "gpt");
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][0]["content"], "be nice");
        assert_eq!(out["messages"][1]["role"], "user");
        assert_eq!(out["messages"][1]["content"], "hello");
        assert_eq!(out["max_tokens"], 50);
    }

    #[test]
    fn responses_to_chat_input_array() {
        let body = json!({"model":"gpt","input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}]});
        let out = responses_to_chat(&body);
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"], "hi");
    }

    #[test]
    fn responses_to_chat_input_image() {
        let body = json!({"model":"gpt","input":[{"role":"user","content":[{"type":"input_image","image_url":"http://x/y.png"}]}]});
        let out = responses_to_chat(&body);
        assert_eq!(out["messages"][0]["content"][0]["type"], "image_url");
        assert_eq!(out["messages"][0]["content"][0]["image_url"]["url"], "http://x/y.png");
    }

    #[test]
    fn chat_to_responses_basic() {
        let chat = json!({"id":"chatcmpl-1","model":"gpt","choices":[{"message":{"role":"assistant","content":"hello world"}}],"usage":{"prompt_tokens":5,"completion_tokens":2}});
        let out = chat_to_responses(&chat);
        assert_eq!(out["object"], "response");
        assert_eq!(out["status"], "completed");
        assert_eq!(out["output"][0]["type"], "message");
        assert_eq!(out["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(out["output"][0]["content"][0]["text"], "hello world");
        assert_eq!(out["usage"]["input_tokens"], 5);
        assert_eq!(out["usage"]["output_tokens"], 2);
    }

    #[test]
    fn oa_tool_call_stream_translates() {
        let mut st = OaToAnthStreamState::default();
        let c1 = translate_sse_openai_to_anthropic_chunk_st(&mut st, r#"{"id":"x","model":"m","choices":[{"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#);
        assert!(c1.iter().any(|l| l.contains("message_start")));
        let c2 = translate_sse_openai_to_anthropic_chunk_st(&mut st, r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"f","arguments":""}}]},"finish_reason":null}]}"#);
        assert!(c2.iter().any(|l| l.contains("content_block_start") && l.contains("tool_use")));
        let c3 = translate_sse_openai_to_anthropic_chunk_st(&mut st, r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\":1}"}}]},"finish_reason":null}]}"#);
        assert!(c3.iter().any(|l| l.contains("input_json_delta")));
        let c4 = translate_sse_openai_to_anthropic_chunk_st(&mut st, r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#);
        assert!(c4.iter().any(|l| l.contains("message_stop")));
    }
}
