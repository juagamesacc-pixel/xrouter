use std::collections::HashMap;
use serde_json::{Value, json};

/// Translate OpenAI request body to Anthropic format
pub fn openai_to_anthropic(body: &Value, target_model: &str) -> Value {
    let mut out = json!({});
    out["model"] = Value::String(target_model.to_string());
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
            let mut new_msg = json!({"role": role});
            if let Some(content) = m.get("content") {
                if content.is_string() {
                    new_msg["content"] = content.clone();
                } else if let Some(arr) = content.as_array() {
                    let mut parts = Vec::new();
                    for p in arr {
                        if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                            parts.push(json!({"type":"text","text": t}));
                        } else if let Some(s) = p.as_str() {
                            parts.push(json!({"type":"text","text": s}));
                        } else if p.get("type").and_then(|x| x.as_str()) == Some("image_url") {
                            if let Some(url) = p.get("image_url").and_then(|u| u.get("url")).and_then(|u| u.as_str()) {
                                if let Some(anth) = openai_image_url_to_anthropic(url) {
                                    parts.push(anth);
                                } else {
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
            if let Some(tc) = m.get("tool_calls") { new_msg["tool_calls"] = tc.clone(); }
            messages.push(new_msg);
        }
    }
    if !system_parts.is_empty() {
        out["system"] = Value::String(system_parts.join("\n"));
    }
    out["messages"] = Value::Array(messages);
    if let Some(mt) = body.get("max_tokens") { out["max_tokens"] = mt.clone(); }
    else if let Some(mt) = body.get("max_completion_tokens") { out["max_tokens"] = mt.clone(); }
    else { out["max_tokens"] = json!(1024); }

    if let Some(t) = body.get("temperature") { out["temperature"] = t.clone(); }
    if let Some(t) = body.get("top_p") { out["top_p"] = t.clone(); }
    if let Some(s) = body.get("stop") { out["stop_sequences"] = s.clone(); }
    if let Some(s) = body.get("stream") { out["stream"] = s.clone(); }
    
    // Translate OpenAI tools -> Anthropic tools
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let mut anth_tools = Vec::new();
        for t in tools {
            if let Some(f) = t.get("function") {
                let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let desc = f.get("description").and_then(|d| d.as_str()).unwrap_or("");
                let schema = f.get("parameters").cloned().unwrap_or(json!({"type": "object"}));
                anth_tools.push(json!({
                    "name": name,
                    "description": desc,
                    "input_schema": schema
                }));
            }
        }
        out["tools"] = Value::Array(anth_tools);
    }

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
            
            if let Some(content) = m.get("content") {
                if let Some(s) = content.as_str() {
                    messages.push(json!({"role": role, "content": s}));
                } else if let Some(arr) = content.as_array() {
                    let mut text_parts = Vec::new();
                    let mut tool_calls = Vec::new();

                    for block in arr {
                        let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match btype {
                            "text" => {
                                if let Some(t) = block.get("text").and_then(|x| x.as_str()) {
                                    text_parts.push(json!({"type": "text", "text": t}));
                                }
                            }
                            "image" => {
                                if let Some(url) = anthropic_image_to_openai_url(block) {
                                    text_parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                                }
                            }
                            "tool_use" => {
                                let id = block.get("id").and_then(|x| x.as_str()).unwrap_or("");
                                let name = block.get("name").and_then(|x| x.as_str()).unwrap_or("");
                                let input = block.get("input").cloned().unwrap_or(json!({}));
                                tool_calls.push(json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": input.to_string()
                                    }
                                }));
                            }
                            "tool_result" => {
                                let id = block.get("tool_use_id").and_then(|x| x.as_str()).unwrap_or("");
                                let res_content = block.get("content").and_then(|c| c.as_str()).unwrap_or("");
                                messages.push(json!({
                                    "role": "tool",
                                    "tool_call_id": id,
                                    "content": res_content
                                }));
                            }
                            _ => {}
                        }
                    }

                    if !text_parts.is_empty() || !tool_calls.is_empty() {
                        let mut msg = json!({"role": role});
                        if text_parts.len() == 1 && text_parts[0].get("type").and_then(|t| t.as_str()) == Some("text") {
                            msg["content"] = text_parts[0].get("text").cloned().unwrap();
                        } else if !text_parts.is_empty() {
                            msg["content"] = Value::Array(text_parts);
                        } else {
                            msg["content"] = Value::Null;
                        }

                        if !tool_calls.is_empty() {
                            msg["tool_calls"] = Value::Array(tool_calls);
                        }
                        messages.push(msg);
                    }
                }
            }
        }
    }
    out["messages"] = Value::Array(messages);
    if let Some(mt) = body.get("max_tokens") { out["max_tokens"] = mt.clone(); }
    if let Some(t) = body.get("temperature") { out["temperature"] = t.clone(); }
    if let Some(s) = body.get("stop_sequences") { out["stop"] = s.clone(); }
    if let Some(s) = body.get("stream") { out["stream"] = s.clone(); }

    // Translate Anthropic tools -> OpenAI functions
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let mut oa_tools = Vec::new();
        for t in tools {
            let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let desc = t.get("description").and_then(|d| d.as_str()).unwrap_or("");
            let schema = t.get("input_schema").cloned().unwrap_or(json!({"type": "object"}));
            oa_tools.push(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": desc,
                    "parameters": schema
                }
            }));
        }
        out["tools"] = Value::Array(oa_tools);
    }

    out
}

fn openai_image_url_to_anthropic(url: &str) -> Option<Value> {
    if let Some(rest) = url.strip_prefix("data:") {
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
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct OaToAnthStreamState {
    pub message_started: bool,
    pub text_block_started: bool,
    pub text_block_index: usize,
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

#[derive(Default, Clone)]
pub struct AnthToOaStreamState {
    pub tool_blocks: HashMap<usize, (usize, String, String)>,
    pub next_tool_index: usize,
    pub finished: bool,
}

/// Translate one OpenAI SSE `data:` payload into zero or more Anthropic events.
pub fn translate_sse_openai_to_anthropic_chunk_st(state: &mut OaToAnthStreamState, openai_data: &str) -> Vec<String> {
    if openai_data.trim() == "[DONE]" {
        let mut out = Vec::new();
        if !state.finished {
            // Close text block if open
            if state.text_block_started {
                out.push(format!("event: content_block_stop\ndata: {}\n\n", json!({"type":"content_block_stop","index": state.text_block_index})));
                state.text_block_started = false;
            }
            // Close tool blocks
            for (_, tb) in state.tool_blocks.iter() {
                if tb.started {
                    out.push(format!("event: content_block_stop\ndata: {}\n\n", json!({"type":"content_block_stop","index": tb.block_index})));
                }
            }
            out.push("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string());
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

    // 1. Emit message_start on the very first chunk
    if !state.message_started {
        let role = delta.and_then(|d| d.get("role")).and_then(|r| r.as_str()).unwrap_or("assistant");
        let msg = json!({
            "type": "message_start",
            "message": {
                "id": val.get("id").and_then(|x| x.as_str()).unwrap_or("msg_translated"),
                "type": "message",
                "role": role,
                "model": val.get("model").and_then(|m| m.as_str()).unwrap_or(""),
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        });
        out.push(format!("event: message_start\ndata: {}\n\n", msg));
        state.message_started = true;
    }

    // 2. Extract text (supports both standard `content` and reasoning `reasoning_content`)
    let text = delta.and_then(|d| d.get("content")).and_then(|c| c.as_str())
        .or_else(|| delta.and_then(|d| d.get("reasoning_content")).and_then(|r| r.as_str()));

    if let Some(content) = text {
        if !content.is_empty() {
            // If text block hasn't started, emit content_block_start first!
            if !state.text_block_started {
                state.text_block_index = state.next_block_index;
                state.next_block_index += 1;
                let start = json!({
                    "type": "content_block_start",
                    "index": state.text_block_index,
                    "content_block": {"type": "text", "text": ""}
                });
                out.push(format!("event: content_block_start\ndata: {}\n\n", start));
                state.text_block_started = true;
            }

            let anth = json!({
                "type": "content_block_delta",
                "index": state.text_block_index,
                "delta": {"type": "text_delta", "text": content}
            });
            out.push(format!("event: content_block_delta\ndata: {}\n\n", anth));
        }
    }

    // 3. Tool calls
    if let Some(tool_calls) = delta.and_then(|d| d.get("tool_calls")).and_then(|t| t.as_array()) {
        // If a text block was open, close it before opening tool blocks
        if state.text_block_started {
            out.push(format!("event: content_block_stop\ndata: {}\n\n", json!({"type":"content_block_stop","index": state.text_block_index})));
            state.text_block_started = false;
        }

        for tc in tool_calls {
            let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            let tb = state.tool_blocks.entry(idx).or_insert_with(|| {
                let bi = state.next_block_index;
                state.next_block_index += 1;
                ToolBlock { block_index: bi, id: String::new(), name: String::new(), started: false }
            });
            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) { if !id.is_empty() { tb.id = id.to_string(); } }
            if let Some(name) = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) { if !name.is_empty() { tb.name = name.to_string(); } }
            if !tb.started && (!tb.id.is_empty() || !tb.name.is_empty()) {
                let block = json!({
                    "type": "content_block_start",
                    "index": tb.block_index,
                    "content_block": { "type": "tool_use", "id": tb.id, "name": tb.name }
                });
                out.push(format!("event: content_block_start\ndata: {}\n\n", block));
                tb.started = true;
            }
            if let Some(args) = tc.get("function").and_then(|f| f.get("arguments")).and_then(|a| a.as_str()) {
                if !args.is_empty() {
                    let d = json!({"type":"content_block_delta","index": tb.block_index,"delta":{"type":"input_json_delta","partial_json": args}});
                    out.push(format!("event: content_block_delta\ndata: {}\n\n", d));
                }
            }
        }
    }

    // 4. Finish reason -> close all blocks, emit message_delta + message_stop exactly ONCE
    if let Some(finish) = choice.get("finish_reason").and_then(|r| r.as_str()) {
        if !finish.is_empty() && finish != "null" && !state.finished {
            // Close text block if open
            if state.text_block_started {
                out.push(format!("event: content_block_stop\ndata: {}\n\n", json!({"type":"content_block_stop","index": state.text_block_index})));
                state.text_block_started = false;
            }
            // Close tool blocks if open
            for (_, tb) in state.tool_blocks.iter() {
                if tb.started {
                    out.push(format!("event: content_block_stop\ndata: {}\n\n", json!({"type":"content_block_stop","index": tb.block_index})));
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
            out.push(format!("event: message_delta\ndata: {}\n\n", msg_delta));
            out.push("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string());
            state.finished = true;
        }
    }

    out
}

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
            Some(format!("data: {}\n\n", openai))
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
                Some(format!("data: {}\n\n", openai))
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
                Some(format!("data: {}\n\n", openai))
            } else if let Some(partial) = delta.and_then(|d| d.get("partial_json")).and_then(|p| p.as_str()) {
                let oa_index = state.tool_blocks.get(&block_index).map(|(i, _, _)| *i)?;
                let openai = json!({
                    "id": "chatcmpl-translated", "object": "chat.completion.chunk",
                    "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": oa_index, "function": { "arguments": partial } }] }, "finish_reason": null }]
                });
                Some(format!("data: {}\n\n", openai))
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
            Some(format!("data: {}\n\n", openai))
        }
        "message_stop" => {
            if !state.finished {
                state.finished = true;
                Some("data: [DONE]\n\n".to_string())
            } else { None }
        }
        _ => None,
    }
}

pub fn gen_response_id() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let n: u128 = rng.random();
    format!("resp_{:032x}", n)
}

pub fn responses_object(
    id: &str,
    model: &str,
    text: &str,
    input_tokens: u64,
    output_tokens: u64,
    status: &str,
) -> Value {
    let total_tokens = input_tokens + output_tokens;
    json!({
        "id": id,
        "object": "response",
        "status": status,
        "model": model,
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }]
        }],
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": total_tokens
        }
    })
}

pub fn responses_to_chat(body: &Value) -> Value {
    let mut out = json!({});
    if let Some(m) = body.get("model").and_then(|x| x.as_str()) {
        out["model"] = Value::String(m.to_string());
    }

    let mut messages: Vec<Value> = Vec::new();

    if let Some(instructions) = body.get("instructions").and_then(|x| x.as_str()) {
        if !instructions.is_empty() {
            messages.push(json!({ "role": "system", "content": instructions }));
        }
    }

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
    let id = gen_response_id();
    responses_object(&id, model, text, input_tokens, output_tokens, "completed")
}
