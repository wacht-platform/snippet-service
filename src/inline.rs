use serde_json::{Map, Value};

use crate::llm::GeneratedToolCall;

#[derive(Debug, Clone, PartialEq)]
pub struct InlineToolSubmission {
    pub calls: Vec<GeneratedToolCall>,
    pub residual_text: Option<String>,
}

pub fn extract_inline_tool_submissions(text: &str) -> InlineToolSubmission {
    let fenced = extract_fenced_tool_calls(text);
    let mut calls = fenced.calls;
    calls.extend(salvage_tool_calls(text));
    dedupe_calls(&mut calls);

    InlineToolSubmission {
        calls,
        residual_text: strip_markup_span(text),
    }
}

pub fn looks_like_inline_tool_submission(text: &str) -> bool {
    text.contains("<tool_call>")
        || text.contains("</tool_call>")
        || (text.contains("<function=") && text.contains("<parameter="))
        || text.contains("[TOOL_CALLS]")
        || text.contains("<|python_tag|>")
        || text.contains("<｜tool▁call")
        || text.contains("<|START_ACTION|>")
        || (text.contains("<arg_key>") && text.contains("<arg_value>"))
        || text.contains("```tool:")
}

fn dedupe_calls(calls: &mut Vec<GeneratedToolCall>) {
    let mut seen = std::collections::BTreeSet::new();
    calls.retain(|call| {
        let key = serde_json::to_string(call).unwrap_or_default();
        seen.insert(key)
    });
}

fn extract_fenced_tool_calls(text: &str) -> InlineToolSubmission {
    let mut calls = Vec::new();
    let mut residual = String::new();
    let mut cursor = 0;

    while let Some(rel) = text[cursor..].find("```tool:") {
        let start = cursor + rel;
        append_residual(&mut residual, &text[cursor..start]);
        let name_start = start + "```tool:".len();
        let Some(line_end_rel) = text[name_start..].find('\n') else {
            append_residual(&mut residual, &text[start..]);
            cursor = text.len();
            break;
        };
        let line_end = name_start + line_end_rel;
        let name = text[name_start..line_end].trim();
        let body_start = line_end + 1;
        // The body runs until the closing ``` — or, when the model mixes formats
        // (a `tool:` fence with a Qwen `<parameter=>` body), until a `</function>` /
        // `</tool_call>` closer or end of text.
        let body_end = ["```", "</function>", "</tool_call>"]
            .iter()
            .filter_map(|marker| text[body_start..].find(marker).map(|i| body_start + i))
            .min()
            .unwrap_or(text.len());
        let body = text[body_start..body_end].trim();
        let arguments = parse_fenced_body(body);
        if !name.is_empty() {
            calls.push(GeneratedToolCall {
                tool_name: name.to_string(),
                arguments,
                id: None,
            });
        }
        // Skip past the body and any trailing run of closer markup so it doesn't
        // leak into the residual prose.
        cursor = body_end;
        loop {
            let rest = &text[cursor..];
            let ws = rest.len() - rest.trim_start().len();
            let after_ws = &rest[ws..];
            let Some(marker) = ["```", "</function>", "</tool_call>", "</parameter>"]
                .iter()
                .find(|m| after_ws.starts_with(**m))
            else {
                break;
            };
            cursor += ws + marker.len();
        }
    }
    append_residual(&mut residual, &text[cursor..]);
    let residual = residual.trim();
    InlineToolSubmission {
        calls,
        residual_text: (!residual.is_empty()).then(|| residual.to_string()),
    }
}

/// Parse a fenced tool-call body: JSON if it parses, otherwise Qwen `<parameter=>`
/// XML, otherwise a raw `{input: body}` fallback.
fn parse_fenced_body(body: &str) -> Value {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        return value;
    }
    if body.contains("<parameter=") {
        return Value::Object(parse_xml_parameters(body));
    }
    let mut map = Map::new();
    map.insert("input".to_string(), Value::String(body.to_string()));
    Value::Object(map)
}

/// XML/text parameter values are always strings, but some tools expect structured
/// arguments (e.g. `ask_user.questions` is an array). Coerce JSON-looking values to
/// the real thing so the dispatcher's schema validation passes.
fn coerce_value(raw: &str) -> Value {
    let trimmed = raw.trim();
    let looks_json = (trimmed.starts_with('[') && trimmed.ends_with(']'))
        || (trimmed.starts_with('{') && trimmed.ends_with('}'));
    if looks_json
        && let Ok(value) = serde_json::from_str::<Value>(trimmed)
    {
        return value;
    }
    Value::String(trimmed.to_string())
}

fn append_residual(buffer: &mut String, chunk: &str) {
    let chunk = chunk.trim();
    if chunk.is_empty() {
        return;
    }
    if !buffer.is_empty() {
        buffer.push('\n');
    }
    buffer.push_str(chunk);
}

fn strip_markup_span(text: &str) -> Option<String> {
    if text.contains("```tool:") {
        return extract_fenced_tool_calls(text).residual_text;
    }

    const STARTS: [&str; 3] = ["<tool_call>", "<function=", "<arg_key>"];
    const ENDS: [&str; 4] = [
        "</tool_call>",
        "</function>",
        "</parameter>",
        "</arg_value>",
    ];
    let Some(start) = STARTS.iter().filter_map(|marker| text.find(marker)).min() else {
        return Some(text.trim().to_string()).filter(|s| !s.is_empty());
    };
    let end = ENDS
        .iter()
        .filter_map(|marker| text.rfind(marker).map(|i| i + marker.len()))
        .max()
        .unwrap_or(text.len())
        .max(start);

    let mut residual = text[..start].trim_end().to_string();
    let tail = text[end..].trim_start();
    if !tail.is_empty() {
        if !residual.is_empty() {
            residual.push('\n');
        }
        residual.push_str(tail);
    }
    let residual = residual.trim();
    (!residual.is_empty()).then(|| residual.to_string())
}

fn salvage_tool_calls(text: &str) -> Vec<GeneratedToolCall> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }

    let xml = salvage_qwen_xml(text);
    if !xml.is_empty() {
        return xml;
    }

    let glm = salvage_glm(text);
    if !glm.is_empty() {
        return glm;
    }

    let deepseek = salvage_deepseek(text);
    if !deepseek.is_empty() {
        return deepseek;
    }

    let mut calls = Vec::new();
    for value in extract_json_values(text) {
        collect_json_calls(&value, &mut calls);
    }
    calls
}

fn salvage_qwen_xml(text: &str) -> Vec<GeneratedToolCall> {
    let mut calls = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = text[cursor..].find("<function=") {
        let name_start = cursor + rel + "<function=".len();
        let name_rest = &text[name_start..];
        let name_len = name_rest
            .find(|c: char| c == '>' || c == '<' || c.is_whitespace())
            .unwrap_or(name_rest.len());
        let name = name_rest[..name_len].trim().to_string();
        let region_start = name_start + name_len;
        let region_end = text[region_start..]
            .find("<function=")
            .map(|i| region_start + i)
            .unwrap_or(text.len());
        if !name.is_empty() {
            let arguments = parse_xml_parameters(&text[region_start..region_end]);
            calls.push(GeneratedToolCall {
                tool_name: name,
                arguments: Value::Object(arguments),
                id: None,
            });
        }
        cursor = region_end;
    }
    calls
}

fn parse_xml_parameters(region: &str) -> Map<String, Value> {
    const DELIMS: [&str; 4] = ["</parameter>", "<parameter=", "</function>", "</tool_call>"];
    let mut args = Map::new();
    let mut cursor = 0;
    while let Some(rel) = region[cursor..].find("<parameter=") {
        let key_start = cursor + rel + "<parameter=".len();
        let key_rest = &region[key_start..];
        let key_len = key_rest
            .find(|c: char| c == '>' || c.is_whitespace())
            .unwrap_or(key_rest.len());
        let key = key_rest[..key_len].trim().to_string();
        let mut value_start = key_start + key_len;
        if let Some(gt) = region[value_start..].find('>') {
            value_start += gt + 1;
        }
        let rest = &region[value_start..];
        let value_end = DELIMS
            .iter()
            .filter_map(|delimiter| rest.find(delimiter))
            .min()
            .unwrap_or(rest.len());
        if !key.is_empty() {
            args.insert(key, coerce_value(&rest[..value_end]));
        }
        cursor = value_start + value_end;
    }
    args
}

fn salvage_glm(text: &str) -> Vec<GeneratedToolCall> {
    if !text.contains("<arg_key>") {
        return Vec::new();
    }
    let Some(after) = text
        .find("<tool_call>")
        .map(|i| &text[i + "<tool_call>".len()..])
    else {
        return Vec::new();
    };
    let name_len = after
        .find(|c: char| c == '<' || c == '\n')
        .unwrap_or(after.len());
    let name = after[..name_len].trim().to_string();
    if name.is_empty() {
        return Vec::new();
    }

    let mut args = Map::new();
    let mut cursor = 0;
    while let Some(rel) = text[cursor..].find("<arg_key>") {
        let key_start = cursor + rel + "<arg_key>".len();
        let Some(key_end) = text[key_start..].find("</arg_key>").map(|i| key_start + i) else {
            break;
        };
        let key = text[key_start..key_end].trim().to_string();
        let Some(val_start) = text[key_end..]
            .find("<arg_value>")
            .map(|i| key_end + i + "<arg_value>".len())
        else {
            break;
        };
        let val_end = text[val_start..]
            .find("</arg_value>")
            .map(|i| val_start + i)
            .unwrap_or(text.len());
        if !key.is_empty() {
            args.insert(
                key,
                Value::String(text[val_start..val_end].trim().to_string()),
            );
        }
        cursor = val_end;
    }
    if args.is_empty() {
        return Vec::new();
    }
    vec![GeneratedToolCall {
        tool_name: name,
        arguments: Value::Object(args),
        id: None,
    }]
}

fn salvage_deepseek(text: &str) -> Vec<GeneratedToolCall> {
    const SEP: &str = "<｜tool▁sep｜>";
    let mut calls = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = text[cursor..].find(SEP) {
        let name_start = cursor + rel + SEP.len();
        let name_rest = &text[name_start..];
        let name_len = name_rest
            .find(|c: char| c.is_whitespace() || c == '<')
            .unwrap_or(name_rest.len());
        let name = name_rest[..name_len].trim().to_string();
        let arguments = extract_json_values(&text[name_start + name_len..])
            .into_iter()
            .find(Value::is_object)
            .unwrap_or_else(|| Value::Object(Map::new()));
        if !name.is_empty() {
            calls.push(GeneratedToolCall {
                tool_name: name,
                arguments,
                id: None,
            });
        }
        cursor = name_start + name_len;
    }
    calls
}

fn collect_json_calls(value: &Value, out: &mut Vec<GeneratedToolCall>) {
    match value {
        Value::Array(items) => items.iter().for_each(|item| collect_json_calls(item, out)),
        Value::Object(obj) => {
            if let Some(inner) = obj.get("function").filter(|value| value.is_object()) {
                return collect_json_calls(inner, out);
            }
            let name = ["name", "tool_name", "tool"]
                .iter()
                .find_map(|key| obj.get(*key).and_then(Value::as_str))
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let Some(name) = name else { return };
            let arguments = ["arguments", "parameters", "args"]
                .iter()
                .find_map(|key| obj.get(*key).cloned())
                .map(|value| match value {
                    Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
                    other => other,
                })
                .unwrap_or_else(|| Value::Object(Map::new()));
            out.push(GeneratedToolCall {
                tool_name: name.to_string(),
                arguments,
                id: None,
            });
        }
        _ => {}
    }
}

fn extract_json_values(text: &str) -> Vec<Value> {
    let bytes = text.as_bytes();
    let mut values = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if (bytes[i] == b'{' || bytes[i] == b'[')
            && let Some((value, end)) = parse_balanced(text, i)
        {
            values.push(value);
            i = end;
            continue;
        }
        i += 1;
    }
    values
}

fn parse_balanced(text: &str, start: usize) -> Option<(Value, usize)> {
    let bytes = text.as_bytes();
    let (open, close) = if bytes[start] == b'{' {
        (b'{', b'}')
    } else {
        (b'[', b']')
    };
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else if c == b'"' {
            in_str = true;
        } else if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return serde_json::from_str::<Value>(&text[start..=i])
                    .ok()
                    .map(|value| (value, i + 1));
            }
        }
        i += 1;
    }
    None
}
