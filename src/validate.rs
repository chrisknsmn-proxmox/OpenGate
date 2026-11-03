//! Validation of inbound chat requests against the supported subset.
//!
//! The request is parsed into a typed [`ChatRequest`] that each provider
//! translates on its own (`ollama.rs`, `chatgpt::responses`). Anything outside
//! the allowlist is rejected with a 400 naming the parameter, rather than
//! being silently dropped. A few OpenAI fields that cannot change the answer
//! (`user`, `store: false`, `prompt_cache_key`, …) are accepted and dropped.
//! Error messages never echo values.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::error::ApiError;

/// OpenAI's limits, enforced so a request valid here is valid upstream too.
const MAX_TOOLS: usize = 128;
const MAX_TOOL_NAME: usize = 64;
const MAX_CALL_ID: usize = 256;

pub(crate) struct ChatRequest {
    pub stream: bool,
    /// `stream_options.include_usage` on a streaming request.
    pub include_usage: bool,
    pub messages: Vec<Message>,
    /// Empty when the request has no tools.
    pub tools: Vec<Tool>,
    pub tool_choice: ToolChoice,
    pub parallel_tool_calls: Option<bool>,
    /// Sampling hints, sorted by name; the names are the same for Ollama.
    pub sampling: Vec<(&'static str, Value)>,
    pub max_tokens: Option<u64>,
    pub stop: Vec<String>,
    pub response_format: Option<ResponseFormat>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Message {
    /// `system` and `developer` messages.
    System(String),
    User(String),
    /// `content` is empty when the assistant only called tools.
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    /// A tool result. `name` is the called function, taken from the
    /// assistant message that issued `call_id`.
    Tool {
        call_id: String,
        name: String,
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolCall {
    pub id: String,
    pub name: String,
    /// Always a JSON object.
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tool {
    pub name: String,
    pub description: Option<String>,
    /// A JSON Schema object (`{"type":"object",…}`).
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ToolChoice {
    Auto,
    None,
    /// The model must call at least one tool.
    Required,
    /// The model must call this tool.
    Function(String),
}

impl ToolChoice {
    /// Whether the answer must contain a tool call.
    pub fn forced(&self) -> bool {
        matches!(self, ToolChoice::Required | ToolChoice::Function(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ResponseFormat {
    JsonObject,
    JsonSchema {
        name: String,
        schema: Value,
        description: Option<String>,
        strict: Option<bool>,
    },
}

impl ChatRequest {
    /// Tools the model may call: none when `tool_choice` is `none`, only the
    /// named one when a function is forced.
    pub fn offered_tools(&self) -> Vec<&Tool> {
        match &self.tool_choice {
            ToolChoice::None => Vec::new(),
            ToolChoice::Function(name) => self.tools.iter().filter(|t| &t.name == name).collect(),
            ToolChoice::Auto | ToolChoice::Required => self.tools.iter().collect(),
        }
    }

    /// The messages in OpenAI shape, for `LOG_PROMPT_CONTENT=true` only.
    pub fn messages_for_log(&self) -> Value {
        Value::Array(
            self.messages
                .iter()
                .map(|m| match m {
                    Message::System(c) => json!({ "role": "system", "content": c }),
                    Message::User(c) => json!({ "role": "user", "content": c }),
                    Message::Assistant {
                        content,
                        tool_calls,
                    } => json!({
                        "role": "assistant", "content": content,
                        "tool_calls": tool_calls.iter().map(|c| json!({
                            "id": c.id, "name": c.name, "arguments": c.arguments,
                        })).collect::<Vec<_>>(),
                    }),
                    Message::Tool {
                        call_id, content, ..
                    } => json!({ "role": "tool", "tool_call_id": call_id, "content": content }),
                })
                .collect(),
        )
    }
}

pub(crate) fn chat_request(req: Value, virtual_model: &str) -> Result<ChatRequest, ApiError> {
    let Value::Object(req) = req else {
        return Err(invalid("request body must be a JSON object"));
    };

    let mut stream = false;
    let mut include_usage = false;
    let mut saw_model = false;
    let mut raw_messages = None;
    let mut raw_tools = None;
    let mut raw_choice = None;
    let mut parallel_tool_calls = None;
    let mut sampling = Vec::new();
    let mut max_tokens = None;
    let mut stop_seqs = Vec::new();
    let mut response_format = None;

    for (key, value) in req {
        // OpenAI clients often send explicit nulls for "not set".
        if value.is_null() && key != "model" && key != "messages" {
            continue;
        }
        match key.as_str() {
            "model" => {
                if value.as_str() != Some(virtual_model) {
                    return Err(ApiError::invalid_request(
                        "model_not_supported",
                        format!("model must be \"{virtual_model}\""),
                    )
                    .with_param("model"));
                }
                saw_model = true;
            }
            "messages" => raw_messages = Some(value),
            "stream" => {
                stream = value
                    .as_bool()
                    .ok_or_else(|| wrong_type("stream", "a boolean"))?
            }
            "stream_options" => include_usage = stream_opts(&value)?,
            "temperature" => sampling.push(("temperature", number_in(&value, &key, 0.0, 2.0)?)),
            "top_p" => sampling.push(("top_p", number_in(&value, &key, 0.0, 1.0)?)),
            "presence_penalty" => {
                sampling.push(("presence_penalty", number_in(&value, &key, -2.0, 2.0)?))
            }
            "frequency_penalty" => {
                sampling.push(("frequency_penalty", number_in(&value, &key, -2.0, 2.0)?))
            }
            "seed" => {
                if value.as_i64().is_none() {
                    return Err(wrong_type("seed", "an integer"));
                }
                sampling.push(("seed", value));
            }
            "max_tokens" | "max_completion_tokens" => {
                let n = value
                    .as_u64()
                    .filter(|&n| n > 0)
                    .ok_or_else(|| wrong_type(&key, "a positive integer"))?;
                if max_tokens.replace(n).is_some() {
                    return Err(
                        invalid("send only one of max_tokens and max_completion_tokens")
                            .with_param(key),
                    );
                }
            }
            "stop" => stop_seqs = stop(value)?,
            "n" => {
                if value.as_u64() != Some(1) {
                    return Err(unsupported("n", "only n=1 is supported"));
                }
            }
            // Identifiers for abuse tracking or caching: meaningless to a
            // local model and never forwarded.
            "user" | "safety_identifier" | "prompt_cache_key" => {
                if !value.is_string() {
                    return Err(wrong_type(&key, "a string"));
                }
            }
            "metadata" => {
                if !value
                    .as_object()
                    .is_some_and(|o| o.len() <= 16 && o.values().all(Value::is_string))
                {
                    return Err(wrong_type("metadata", "an object of up to 16 strings"));
                }
            }
            // OpenGate never stores anything; `true` would ask for something it cannot do.
            "store" => {
                if value.as_bool() != Some(false) {
                    return Err(unsupported(
                        "store",
                        "OpenGate never stores completions; only store=false is supported",
                    ));
                }
            }
            "tools" => raw_tools = Some(value),
            "tool_choice" => raw_choice = Some(value),
            "parallel_tool_calls" => {
                parallel_tool_calls = Some(
                    value
                        .as_bool()
                        .ok_or_else(|| wrong_type("parallel_tool_calls", "a boolean"))?,
                )
            }
            // The deprecated functions API: only its "off" forms are accepted.
            "functions" => {
                if !value.as_array().is_some_and(Vec::is_empty) {
                    return Err(unsupported(
                        "functions",
                        "the deprecated functions API is not supported; use tools",
                    ));
                }
            }
            "function_call" => {
                if value.as_str() != Some("none") {
                    return Err(unsupported(
                        "function_call",
                        "the deprecated functions API is not supported; use tool_choice",
                    ));
                }
            }
            "response_format" => response_format = response_fmt(&value)?,
            _ => {
                return Err(unsupported(
                    &key,
                    "this parameter is not supported by OpenGate",
                ))
            }
        }
    }

    if !saw_model {
        return Err(invalid("model is required").with_param("model"));
    }
    let Some(raw_messages) = raw_messages else {
        return Err(invalid("messages is required").with_param("messages"));
    };
    let tools = match raw_tools {
        Some(v) => tools(v)?,
        None => Vec::new(),
    };
    let tool_choice = match raw_choice {
        Some(v) => tool_choice(&v, &tools)?,
        None => ToolChoice::Auto,
    };

    Ok(ChatRequest {
        stream,
        include_usage: stream && include_usage,
        messages: messages(raw_messages)?,
        tools,
        tool_choice,
        parallel_tool_calls,
        sampling,
        max_tokens,
        stop: stop_seqs,
        response_format,
    })
}

fn messages(value: Value) -> Result<Vec<Message>, ApiError> {
    let Value::Array(items) = value else {
        return Err(wrong_type("messages", "an array"));
    };
    if items.is_empty() {
        return Err(invalid("messages must not be empty").with_param("messages"));
    }
    let mut out = Vec::with_capacity(items.len());
    // Tool calls of the latest assistant message still awaiting a result,
    // and every call ID seen so far (with its function name).
    let mut pending: Vec<String> = Vec::new();
    let mut calls: HashMap<String, String> = HashMap::new();

    for (i, item) in items.into_iter().enumerate() {
        let param = format!("messages[{i}]");
        let msg = message(&param, item)?;
        match &msg {
            Message::Tool { call_id, .. } => {
                let Some(pos) = pending.iter().position(|p| p == call_id) else {
                    let why = if calls.contains_key(call_id) {
                        "this tool call already has a result, or its result is out of order"
                    } else {
                        "tool_call_id does not match a tool call in the preceding assistant message"
                    };
                    return Err(invalid(why).with_param(format!("{param}.tool_call_id")));
                };
                pending.remove(pos);
            }
            _ if !pending.is_empty() => {
                return Err(invalid(
                    "every tool call must be answered by a tool message (with its \
                     tool_call_id) before the next non-tool message",
                )
                .with_param(param));
            }
            _ => {}
        }
        if let Message::Assistant { tool_calls, .. } = &msg {
            for (j, call) in tool_calls.iter().enumerate() {
                if calls.insert(call.id.clone(), call.name.clone()).is_some() {
                    return Err(invalid("tool call IDs must be unique")
                        .with_param(format!("{param}.tool_calls[{j}].id")));
                }
                pending.push(call.id.clone());
            }
        }
        out.push(msg);
    }
    if !pending.is_empty() {
        return Err(
            invalid("the last assistant message has tool calls without tool results")
                .with_param("messages"),
        );
    }
    // Resolve each tool result's function name from the call it answers.
    for m in &mut out {
        if let Message::Tool { call_id, name, .. } = m {
            *name = calls.get(call_id).cloned().unwrap_or_default();
        }
    }
    Ok(out)
}

fn message(param: &str, value: Value) -> Result<Message, ApiError> {
    let Value::Object(mut m) = value else {
        return Err(wrong_type(param, "an object"));
    };
    let role = match m.get("role").and_then(Value::as_str) {
        Some(r @ ("system" | "developer" | "user" | "assistant" | "tool")) => r.to_owned(),
        Some("function") => {
            return Err(unsupported(
                &format!("{param}.role"),
                "the deprecated function role is not supported; use tool messages",
            ))
        }
        _ => {
            return Err(
                invalid("role must be one of system, developer, user, assistant, tool")
                    .with_param(format!("{param}.role")),
            )
        }
    };

    for (key, v) in &m {
        let ok = match (key.as_str(), role.as_str()) {
            ("role" | "content", _) => true,
            // A participant name: accepted and dropped, as before.
            ("name", "system" | "developer" | "user" | "assistant") => true,
            // The function name; OpenGate takes it from the matching call.
            ("name", "tool") => true,
            ("tool_calls", "assistant") | ("tool_call_id", "tool") => true,
            // An earlier answer's hidden reasoning or refusal: not part of
            // the conversation the model needs, so not forwarded.
            (
                "reasoning_content" | "reasoning" | "reasoning_details" | "reasoning_text"
                | "thinking" | "refusal",
                "assistant",
            ) => true,
            ("function_call", "assistant") if v.is_null() => true,
            ("function_call", _) => {
                return Err(unsupported(
                    &format!("{param}.function_call"),
                    "the deprecated functions API is not supported; use tool_calls",
                ))
            }
            _ => false,
        };
        if !ok {
            return Err(unsupported(
                &format!("{param}.{key}"),
                "this message field is not supported",
            ));
        }
    }

    let content = m.remove("content").unwrap_or(Value::Null);
    match role.as_str() {
        "system" | "developer" => Ok(Message::System(text_content(param, content, false)?)),
        "user" => Ok(Message::User(text_content(param, content, false)?)),
        "assistant" => {
            let tool_calls = match m.remove("tool_calls") {
                None | Some(Value::Null) => Vec::new(),
                Some(v) => assistant_tool_calls(param, v)?,
            };
            let content = text_content(param, content, true)?;
            Ok(Message::Assistant {
                content,
                tool_calls,
            })
        }
        _ => {
            let call_id = m
                .get("tool_call_id")
                .and_then(Value::as_str)
                .filter(|id| valid_call_id(id))
                .ok_or_else(|| wrong_type(&format!("{param}.tool_call_id"), "a non-empty string"))?
                .to_owned();
            if m.get("name").is_some_and(|n| !n.is_string()) {
                return Err(wrong_type(&format!("{param}.name"), "a string"));
            }
            Ok(Message::Tool {
                call_id,
                name: String::new(),
                content: text_content(param, content, true)?,
            })
        }
    }
}

/// A string, or text content parts joined with newlines. `nullable` allows
/// `null`/absent content (assistant tool-call messages, empty tool results).
fn text_content(param: &str, content: Value, nullable: bool) -> Result<String, ApiError> {
    match content {
        Value::String(text) => Ok(text),
        Value::Null if nullable => Ok(String::new()),
        Value::Array(parts) => {
            let mut text = Vec::with_capacity(parts.len());
            for (j, part) in parts.iter().enumerate() {
                match (part["type"].as_str(), part["text"].as_str()) {
                    (Some("text"), Some(t)) => text.push(t),
                    _ => {
                        return Err(unsupported(
                            &format!("{param}.content[{j}]"),
                            "only text content parts are supported",
                        ))
                    }
                }
            }
            Ok(text.join("\n"))
        }
        _ => Err(wrong_type(
            &format!("{param}.content"),
            "a string or an array of text parts",
        )),
    }
}

fn assistant_tool_calls(param: &str, value: Value) -> Result<Vec<ToolCall>, ApiError> {
    let Value::Array(items) = value else {
        return Err(wrong_type(&format!("{param}.tool_calls"), "an array"));
    };
    items
        .into_iter()
        .enumerate()
        .map(|(j, call)| {
            let p = format!("{param}.tool_calls[{j}]");
            let id = call["id"]
                .as_str()
                .filter(|id| valid_call_id(id))
                .ok_or_else(|| wrong_type(&format!("{p}.id"), "a non-empty string"))?;
            if !matches!(call.get("type"), None | Some(Value::Null)) && call["type"] != "function" {
                return Err(unsupported(
                    &format!("{p}.type"),
                    "only function tool calls are supported",
                ));
            }
            let name = call["function"]["name"]
                .as_str()
                .filter(|n| valid_tool_name(n))
                .ok_or_else(|| {
                    wrong_type(
                        &format!("{p}.function.name"),
                        "a function name (letters, digits, _ or -, at most 64)",
                    )
                })?;
            let arguments = match &call["function"]["arguments"] {
                Value::String(s) if s.trim().is_empty() => json!({}),
                Value::String(s) => serde_json::from_str::<Value>(s)
                    .ok()
                    .filter(Value::is_object)
                    .ok_or_else(|| {
                        wrong_type(
                            &format!("{p}.function.arguments"),
                            "a JSON object encoded as a string",
                        )
                    })?,
                Value::Null => json!({}),
                _ => {
                    return Err(wrong_type(
                        &format!("{p}.function.arguments"),
                        "a JSON object encoded as a string",
                    ))
                }
            };
            Ok(ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments,
            })
        })
        .collect()
}

fn tools(value: Value) -> Result<Vec<Tool>, ApiError> {
    let Value::Array(items) = value else {
        return Err(wrong_type("tools", "an array"));
    };
    if items.len() > MAX_TOOLS {
        return Err(invalid(format!("at most {MAX_TOOLS} tools are supported")).with_param("tools"));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(items.len());
    for (i, tool) in items.into_iter().enumerate() {
        let p = format!("tools[{i}]");
        if tool["type"] != "function" {
            return Err(unsupported(
                &format!("{p}.type"),
                "only function tools are supported",
            ));
        }
        let Some(f) = tool["function"].as_object() else {
            return Err(wrong_type(&format!("{p}.function"), "an object"));
        };
        for key in f.keys() {
            if !matches!(
                key.as_str(),
                "name" | "description" | "parameters" | "strict"
            ) {
                return Err(unsupported(
                    &format!("{p}.function.{key}"),
                    "this tool field is not supported",
                ));
            }
        }
        let name = f
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| valid_tool_name(n))
            .ok_or_else(|| {
                wrong_type(
                    &format!("{p}.function.name"),
                    "a function name (letters, digits, _ or -, at most 64)",
                )
            })?;
        if !seen.insert(name.to_owned()) {
            return Err(
                invalid("tool names must be unique").with_param(format!("{p}.function.name"))
            );
        }
        let description = match f.get("description") {
            None | Some(Value::Null) => None,
            Some(Value::String(d)) => Some(d.clone()),
            Some(_) => return Err(wrong_type(&format!("{p}.function.description"), "a string")),
        };
        let parameters = match f.get("parameters") {
            None | Some(Value::Null) => json!({ "type": "object", "properties": {} }),
            Some(Value::Object(schema))
                if matches!(schema.get("type"), None | Some(Value::Null))
                    || schema["type"] == "object" =>
            {
                Value::Object(schema.clone())
            }
            Some(_) => {
                return Err(wrong_type(
                    &format!("{p}.function.parameters"),
                    "a JSON Schema object with type \"object\"",
                ))
            }
        };
        match f.get("strict") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => {}
            Some(Value::Bool(true)) => {
                // Neither provider route can guarantee schema-exact arguments.
                return Err(unsupported(
                    &format!("{p}.function.strict"),
                    "strict tool schemas are not supported; omit strict or set it to false",
                ));
            }
            Some(_) => return Err(wrong_type(&format!("{p}.function.strict"), "a boolean")),
        }
        out.push(Tool {
            name: name.to_owned(),
            description,
            parameters,
        });
    }
    Ok(out)
}

fn tool_choice(value: &Value, tools: &[Tool]) -> Result<ToolChoice, ApiError> {
    let choice = match value {
        Value::String(s) => match s.as_str() {
            "auto" => ToolChoice::Auto,
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => {
                return Err(wrong_type(
                    "tool_choice",
                    "\"auto\", \"none\", \"required\" or a function choice",
                ))
            }
        },
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("function") => {
            let name = value["function"]["name"].as_str().ok_or_else(|| {
                wrong_type("tool_choice.function.name", "the name of a tool in tools")
            })?;
            if !tools.iter().any(|t| t.name == name) {
                return Err(invalid("tool_choice names a function that is not in tools")
                    .with_param("tool_choice.function.name"));
            }
            ToolChoice::Function(name.to_owned())
        }
        Value::Object(_) => {
            return Err(unsupported(
                "tool_choice",
                "only function tool choices are supported",
            ))
        }
        _ => {
            return Err(wrong_type(
                "tool_choice",
                "\"auto\", \"none\", \"required\" or a function choice",
            ))
        }
    };
    if choice == ToolChoice::Required && tools.is_empty() {
        return Err(
            invalid("tool_choice \"required\" needs at least one tool").with_param("tool_choice")
        );
    }
    Ok(choice)
}

fn response_fmt(value: &Value) -> Result<Option<ResponseFormat>, ApiError> {
    match value["type"].as_str() {
        Some("text") => Ok(None),
        Some("json_object") => Ok(Some(ResponseFormat::JsonObject)),
        Some("json_schema") => {
            let js = &value["json_schema"];
            let name = js["name"]
                .as_str()
                .filter(|n| valid_tool_name(n))
                .ok_or_else(|| {
                    wrong_type(
                        "response_format.json_schema.name",
                        "a name (letters, digits, _ or -, at most 64)",
                    )
                })?;
            let schema = js
                .get("schema")
                .filter(|s| s.is_object())
                .cloned()
                .ok_or_else(|| wrong_type("response_format.json_schema.schema", "an object"))?;
            let description = js["description"].as_str().map(str::to_owned);
            let strict = match js.get("strict") {
                None | Some(Value::Null) => None,
                Some(Value::Bool(b)) => Some(*b),
                Some(_) => {
                    return Err(wrong_type(
                        "response_format.json_schema.strict",
                        "a boolean",
                    ))
                }
            };
            Ok(Some(ResponseFormat::JsonSchema {
                name: name.to_owned(),
                schema,
                description,
                strict,
            }))
        }
        _ => Err(unsupported(
            "response_format",
            "response_format.type must be text, json_object or json_schema",
        )),
    }
}

/// OpenAI's rule for function names: `^[a-zA-Z0-9_-]{1,64}$`.
fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TOOL_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn valid_call_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_CALL_ID && !id.chars().any(char::is_control)
}

/// Returns `include_usage`.
fn stream_opts(value: &Value) -> Result<bool, ApiError> {
    let ok = value.as_object().is_some_and(|o| {
        o.iter().all(|(k, v)| {
            matches!(k.as_str(), "include_usage" | "include_obfuscation") && v.is_boolean()
        })
    });
    if ok {
        Ok(value["include_usage"].as_bool().unwrap_or(false))
    } else {
        Err(unsupported(
            "stream_options",
            "only stream_options.include_usage is supported",
        ))
    }
}

fn number_in(value: &Value, name: &str, min: f64, max: f64) -> Result<Value, ApiError> {
    match value.as_f64() {
        Some(n) if (min..=max).contains(&n) => Ok(value.clone()),
        _ => Err(wrong_type(
            name,
            &format!("a number between {min} and {max}"),
        )),
    }
}

fn stop(value: Value) -> Result<Vec<String>, ApiError> {
    let err = || wrong_type("stop", "a string or an array of up to 4 strings");
    match value {
        Value::String(s) => Ok(vec![s]),
        Value::Array(items) if items.len() <= 4 => items
            .into_iter()
            .map(|v| match v {
                Value::String(s) => Ok(s),
                _ => Err(err()),
            })
            .collect(),
        _ => Err(err()),
    }
}

fn invalid(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::invalid_request("invalid_request", message)
}

fn wrong_type(param: &str, expected: &str) -> ApiError {
    invalid(format!("{param} must be {expected}")).with_param(param)
}

fn unsupported(param: &str, message: &'static str) -> ApiError {
    ApiError::invalid_request("unsupported_parameter", message).with_param(param)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VM: &str = "opengate/local";

    fn ok(req: Value) -> ChatRequest {
        chat_request(req, VM).unwrap()
    }

    fn err(req: Value) -> (&'static str, Option<String>) {
        let e = chat_request(req, VM).err().expect("should be rejected");
        assert_eq!(e.status, 400);
        (e.code, e.param)
    }

    fn user(text: &str) -> Value {
        json!([{ "role": "user", "content": text }])
    }

    fn weather_tool() -> Value {
        json!({ "type": "function", "function": {
            "name": "get_weather", "description": "Weather for a city",
            "parameters": { "type": "object", "properties": { "city": { "type": "string" } }, "required": ["city"] },
        }})
    }

    #[test]
    fn minimal_request() {
        let r = ok(json!({ "model": VM, "messages": user("hi") }));
        assert!(!r.stream && !r.include_usage);
        assert_eq!(r.messages, vec![Message::User("hi".into())]);
        assert!(r.tools.is_empty());
        assert_eq!(r.tool_choice, ToolChoice::Auto);
    }

    #[test]
    fn keeps_supported_options_and_maps_max_completion_tokens() {
        let r = ok(json!({
            "model": VM, "messages": user("hi"), "stream": true,
            "stream_options": { "include_usage": true },
            "temperature": 0.2, "top_p": 0.9, "max_completion_tokens": 50,
            "stop": ["\n"], "seed": 7, "user": "u1", "tools": [], "tool_choice": "none",
            "presence_penalty": null, "store": false, "prompt_cache_key": "k",
            "parallel_tool_calls": true, "metadata": { "a": "b" },
        }));
        assert_eq!(r.max_tokens, Some(50));
        assert!(r.stream && r.include_usage);
        assert_eq!(r.stop, vec!["\n".to_owned()]);
        let names: Vec<_> = r.sampling.iter().map(|(k, _)| *k).collect();
        assert_eq!(names, ["seed", "temperature", "top_p"]);
        assert_eq!(r.tool_choice, ToolChoice::None);
        assert_eq!(r.parallel_tool_calls, Some(true));
    }

    #[test]
    fn include_usage_only_applies_to_streams() {
        let r = ok(
            json!({ "model": VM, "messages": user("hi"), "stream_options": { "include_usage": true } }),
        );
        assert!(!r.include_usage);
    }

    #[test]
    fn normalizes_messages() {
        let r = ok(json!({ "model": VM, "messages": [
            { "role": "developer", "content": "be brief" },
            { "role": "user", "name": "c", "content": [{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }] },
        ]}));
        assert_eq!(
            r.messages,
            vec![
                Message::System("be brief".into()),
                Message::User("a\nb".into())
            ]
        );
    }

    #[test]
    fn accepts_openclaw_tool_turns() {
        // The exact shapes OpenClaw's openai-completions transport sends.
        let r = ok(json!({
            "model": VM, "stream": true, "tool_choice": "auto", "max_completion_tokens": 4096,
            "tools": [weather_tool()],
            "messages": [
                { "role": "system", "content": "You are an agent." },
                { "role": "user", "content": "Weather in Paris and Rome?" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "call_1", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" } },
                    { "id": "call_2", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\":\"Rome\"}" } },
                ]},
                { "role": "tool", "tool_call_id": "call_1", "content": "18C" },
                { "role": "tool", "tool_call_id": "call_2", "content": [{ "type": "text", "text": "21C" }] },
                { "role": "assistant", "content": "Paris 18C, Rome 21C.", "reasoning_content": "dropped" },
                { "role": "user", "content": "Thanks" },
            ],
        }));
        assert_eq!(r.tools.len(), 1);
        assert_eq!(r.tools[0].name, "get_weather");
        assert_eq!(r.tool_choice, ToolChoice::Auto);
        let Message::Assistant {
            content,
            tool_calls,
        } = &r.messages[2]
        else {
            panic!()
        };
        assert!(content.is_empty());
        assert_eq!(tool_calls[1].arguments, json!({ "city": "Rome" }));
        assert_eq!(
            r.messages[4],
            Message::Tool {
                call_id: "call_2".into(),
                name: "get_weather".into(),
                content: "21C".into()
            }
        );
    }

    #[test]
    fn tool_choice_forms() {
        let base = |choice: Value| json!({ "model": VM, "messages": user("x"), "tools": [weather_tool()], "tool_choice": choice });
        assert_eq!(
            ok(base(json!("required"))).tool_choice,
            ToolChoice::Required
        );
        let forced = ok(base(
            json!({ "type": "function", "function": { "name": "get_weather" } }),
        ));
        assert_eq!(
            forced.tool_choice,
            ToolChoice::Function("get_weather".into())
        );
        assert_eq!(forced.offered_tools().len(), 1);
        assert!(ok(base(json!("none"))).offered_tools().is_empty());
        assert_eq!(
            err(base(
                json!({ "type": "function", "function": { "name": "other" } })
            ))
            .1
            .as_deref(),
            Some("tool_choice.function.name")
        );
        assert_eq!(err(base(json!("sometimes"))).0, "invalid_request");
        assert_eq!(
            err(json!({ "model": VM, "messages": user("x"), "tool_choice": "required" }))
                .1
                .as_deref(),
            Some("tool_choice")
        );
        // "none" and "auto" without tools stay accepted no-ops.
        ok(json!({ "model": VM, "messages": user("x"), "tool_choice": "auto" }));
    }

    #[test]
    fn rejects_invalid_tools() {
        let with = |tools: Value| json!({ "model": VM, "messages": user("x"), "tools": tools });
        let f = |function: Value| with(json!([{ "type": "function", "function": function }]));
        for (body, param) in [
            (f(json!({ "name": "has space" })), "tools[0].function.name"),
            (
                f(json!({ "name": "a".repeat(65) })),
                "tools[0].function.name",
            ),
            (
                f(json!({ "name": "f", "parameters": { "type": "string" } })),
                "tools[0].function.parameters",
            ),
            (
                f(json!({ "name": "f", "parameters": [] })),
                "tools[0].function.parameters",
            ),
            (
                f(json!({ "name": "f", "strict": true })),
                "tools[0].function.strict",
            ),
            (
                f(json!({ "name": "f", "extra": 1 })),
                "tools[0].function.extra",
            ),
            (
                with(json!([{ "type": "custom", "custom": { "name": "f" } }])),
                "tools[0].type",
            ),
            (
                with(json!([weather_tool(), weather_tool()])),
                "tools[1].function.name",
            ),
            (with(json!({})), "tools"),
        ] {
            assert_eq!(err(body).1.as_deref(), Some(param));
        }
        // A missing schema becomes an empty object schema.
        let r = ok(f(json!({ "name": "ping", "strict": false })));
        assert_eq!(
            r.tools[0].parameters,
            json!({ "type": "object", "properties": {} })
        );
    }

    #[test]
    fn enforces_tool_message_order() {
        let call = |id: &str| {
            json!({ "role": "assistant", "content": null, "tool_calls": [
            { "id": id, "type": "function", "function": { "name": "f", "arguments": "{}" } } ] })
        };
        let res = |id: &str| json!({ "role": "tool", "tool_call_id": id, "content": "r" });
        let req = |msgs: Value| json!({ "model": VM, "messages": msgs });

        // Result without a call; unknown ID; duplicate result; missing result.
        assert_eq!(
            err(req(json!([res("a")]))).1.as_deref(),
            Some("messages[0].tool_call_id")
        );
        assert_eq!(
            err(req(
                json!([{ "role": "user", "content": "x" }, call("a"), res("b")])
            ))
            .1
            .as_deref(),
            Some("messages[2].tool_call_id")
        );
        assert_eq!(
            err(req(json!([call("a"), res("a"), res("a")])))
                .1
                .as_deref(),
            Some("messages[2].tool_call_id")
        );
        assert_eq!(
            err(req(json!([call("a"), { "role": "user", "content": "x" }])))
                .1
                .as_deref(),
            Some("messages[1]")
        );
        assert_eq!(err(req(json!([call("a")]))).1.as_deref(), Some("messages"));
        assert_eq!(
            err(req(json!([call("a"), res("a"), call("a"), res("a")])))
                .1
                .as_deref(),
            Some("messages[2].tool_calls[0].id")
        );
        // Multi-turn: two rounds of calls and results.
        ok(req(
            json!([{ "role": "user", "content": "x" }, call("a"), res("a"), call("b"), res("b"),
            { "role": "assistant", "content": "done" }]),
        ));
    }

    #[test]
    fn rejects_bad_tool_call_arguments() {
        let msgs = |args: Value| {
            json!({ "model": VM, "messages": [
            { "role": "assistant", "content": null, "tool_calls": [
                { "id": "a", "type": "function", "function": { "name": "f", "arguments": args } } ] },
            { "role": "tool", "tool_call_id": "a", "content": "r" } ] })
        };
        for bad in [
            json!("{not json"),
            json!("[1,2]"),
            json!(7),
            json!({ "city": "x" }),
        ] {
            assert_eq!(
                err(msgs(bad)).1.as_deref(),
                Some("messages[0].tool_calls[0].function.arguments")
            );
        }
        ok(msgs(json!("")));
    }

    #[test]
    fn response_formats() {
        let with = |f: Value| json!({ "model": VM, "messages": user("x"), "response_format": f });
        assert_eq!(ok(with(json!({ "type": "text" }))).response_format, None);
        assert_eq!(
            ok(with(json!({ "type": "json_object" }))).response_format,
            Some(ResponseFormat::JsonObject)
        );
        let r = ok(with(
            json!({ "type": "json_schema", "json_schema": { "name": "out", "schema": { "type": "object" }, "strict": true } }),
        ));
        assert!(matches!(
            r.response_format,
            Some(ResponseFormat::JsonSchema {
                strict: Some(true),
                ..
            })
        ));
        assert_eq!(
            err(with(
                json!({ "type": "json_schema", "json_schema": { "name": "out" } })
            ))
            .0,
            "invalid_request"
        );
        assert_eq!(
            err(with(json!({ "type": "xml" }))).0,
            "unsupported_parameter"
        );
    }

    #[test]
    fn rejects_wrong_or_missing_model() {
        assert_eq!(
            err(json!({ "model": "llama3.1:8b", "messages": user("x") })).0,
            "model_not_supported"
        );
        assert_eq!(
            err(json!({ "model": "http://evil/", "messages": user("x") })).0,
            "model_not_supported"
        );
        assert_eq!(err(json!({ "messages": user("x") })).0, "invalid_request");
    }

    #[test]
    fn rejects_legacy_functions_api() {
        let f = json!([{ "name": "f" }]);
        assert_eq!(
            err(json!({ "model": VM, "messages": user("x"), "functions": f })).0,
            "unsupported_parameter"
        );
        assert_eq!(
            err(json!({ "model": VM, "messages": user("x"), "function_call": "auto" })).0,
            "unsupported_parameter"
        );
        let msgs = json!([{ "role": "function", "name": "f", "content": "r" }]);
        assert_eq!(
            err(json!({ "model": VM, "messages": msgs })).0,
            "unsupported_parameter"
        );
        ok(json!({ "model": VM, "messages": user("x"), "functions": [], "function_call": "none" }));
    }

    #[test]
    fn rejects_unknown_and_malformed_fields() {
        let (code, param) = err(json!({ "model": VM, "messages": user("x"), "logprobs": true }));
        assert_eq!(
            (code, param.as_deref()),
            ("unsupported_parameter", Some("logprobs"))
        );
        assert_eq!(
            err(json!({ "model": VM, "messages": user("x"), "store": true })).0,
            "unsupported_parameter"
        );
        assert_eq!(
            err(json!({ "model": VM, "messages": user("x"), "reasoning_effort": "high" })).0,
            "unsupported_parameter"
        );
        assert_eq!(
            err(json!({ "model": VM, "messages": [] })).0,
            "invalid_request"
        );
        assert_eq!(
            err(json!({ "model": VM, "messages": user("x"), "temperature": 9 })).0,
            "invalid_request"
        );
        assert_eq!(
            err(json!({ "model": VM, "messages": user("x"), "stream": "yes" })).0,
            "invalid_request"
        );
        assert_eq!(
            err(json!({ "model": VM, "messages": user("x"), "n": 2 })).0,
            "unsupported_parameter"
        );
        let image = json!([{ "role": "user", "content": [{ "type": "image_url", "image_url": { "url": "x" } }] }]);
        assert_eq!(
            err(json!({ "model": VM, "messages": image })).0,
            "unsupported_parameter"
        );
        let user_null = json!([{ "role": "user", "content": null }]);
        assert_eq!(
            err(json!({ "model": VM, "messages": user_null })).0,
            "invalid_request"
        );
        let audio = json!([{ "role": "assistant", "content": "x", "audio": { "id": "a" } }]);
        assert_eq!(
            err(json!({ "model": VM, "messages": audio })).1.as_deref(),
            Some("messages[0].audio")
        );
        assert_eq!(err(json!([1, 2])).0, "invalid_request");
    }
}
