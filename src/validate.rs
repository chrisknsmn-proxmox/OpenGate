//! Validation of inbound chat requests against the supported subset.
//!
//! Anything outside the allowlist is rejected with a 400 naming the parameter,
//! rather than being silently dropped. Error messages never echo values.

use serde_json::{Map, Value};

use crate::error::ApiError;

pub(crate) struct ChatRequest {
    pub stream: bool,
    /// Normalized messages and sampling options, without `model` or `stream`.
    fields: Map<String, Value>,
}

impl ChatRequest {
    pub fn messages(&self) -> &Value {
        &self.fields["messages"]
    }

    pub fn into_upstream(self, ollama_model: &str) -> Value {
        let mut body = self.fields;
        body.insert("model".into(), ollama_model.into());
        body.insert("stream".into(), self.stream.into());
        Value::Object(body)
    }
}

pub(crate) fn chat_request(req: Value, virtual_model: &str) -> Result<ChatRequest, ApiError> {
    let Value::Object(req) = req else {
        return Err(invalid("request body must be a JSON object"));
    };

    let mut fields = Map::new();
    let mut stream = false;
    let mut stream_options = None;
    let mut saw_model = false;

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
            "messages" => {
                fields.insert(key, messages(value)?);
            }
            "stream" => {
                stream = value
                    .as_bool()
                    .ok_or_else(|| wrong_type("stream", "a boolean"))?
            }
            "stream_options" => stream_options = Some(stream_opts(value)?),
            "temperature" => {
                fields.insert(key, number_in(&value, "temperature", 0.0, 2.0)?);
            }
            "top_p" => {
                fields.insert(key, number_in(&value, "top_p", 0.0, 1.0)?);
            }
            "presence_penalty" | "frequency_penalty" => {
                let v = number_in(&value, &key, -2.0, 2.0)?;
                fields.insert(key, v);
            }
            "max_tokens" | "max_completion_tokens" => {
                if value.as_u64().is_none_or(|n| n == 0) {
                    return Err(wrong_type(&key, "a positive integer"));
                }
                if fields.insert("max_tokens".into(), value).is_some() {
                    return Err(
                        invalid("send only one of max_tokens and max_completion_tokens")
                            .with_param(key),
                    );
                }
            }
            "seed" => {
                if value.as_i64().is_none() {
                    return Err(wrong_type("seed", "an integer"));
                }
                fields.insert(key, value);
            }
            "stop" => {
                fields.insert(key, stop(value)?);
            }
            "n" => {
                if value.as_u64() != Some(1) {
                    return Err(unsupported("n", "only n=1 is supported"));
                }
            }
            // End-user identifier for abuse tracking; meaningless to a local model.
            "user" => {
                if !value.is_string() {
                    return Err(wrong_type("user", "a string"));
                }
            }
            "tools" | "functions" => {
                if !value.as_array().is_some_and(Vec::is_empty) {
                    return Err(tools_unsupported(&key));
                }
            }
            "tool_choice" | "function_call" => {
                if value.as_str() != Some("none") {
                    return Err(tools_unsupported(&key));
                }
            }
            "parallel_tool_calls" => {
                if !value.is_boolean() {
                    return Err(wrong_type("parallel_tool_calls", "a boolean"));
                }
            }
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
    if !fields.contains_key("messages") {
        return Err(invalid("messages is required").with_param("messages"));
    }
    if let (true, Some(opts)) = (stream, stream_options) {
        fields.insert("stream_options".into(), opts);
    }

    Ok(ChatRequest { stream, fields })
}

fn messages(value: Value) -> Result<Value, ApiError> {
    let Value::Array(items) = value else {
        return Err(wrong_type("messages", "an array"));
    };
    if items.is_empty() {
        return Err(invalid("messages must not be empty").with_param("messages"));
    }
    items
        .into_iter()
        .enumerate()
        .map(|(i, m)| message(i, m))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

fn message(i: usize, value: Value) -> Result<Value, ApiError> {
    let param = format!("messages[{i}]");
    let Value::Object(mut m) = value else {
        return Err(wrong_type(&param, "an object"));
    };
    for key in m.keys() {
        match key.as_str() {
            "role" | "content" | "name" => {}
            "tool_calls" | "tool_call_id" | "function_call" => {
                return Err(tools_unsupported(&format!("{param}.{key}")))
            }
            _ => {
                return Err(unsupported(
                    &format!("{param}.{key}"),
                    "this message field is not supported",
                ))
            }
        }
    }

    let role = match m.get("role").and_then(Value::as_str) {
        Some(r @ ("system" | "user" | "assistant")) => r.to_owned(),
        // Newer OpenAI clients send system prompts as "developer".
        Some("developer") => "system".to_owned(),
        Some("tool" | "function") => return Err(tools_unsupported(&format!("{param}.role"))),
        _ => {
            return Err(
                invalid("role must be one of system, developer, user, assistant")
                    .with_param(format!("{param}.role")),
            )
        }
    };

    let content = match m.remove("content") {
        Some(Value::String(text)) => text,
        // Content parts: only text is supported by the local model.
        Some(Value::Array(parts)) => {
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
            text.join("\n")
        }
        _ => {
            return Err(wrong_type(
                &format!("{param}.content"),
                "a string or an array of text parts",
            ))
        }
    };

    Ok(serde_json::json!({ "role": role, "content": content }))
}

fn stream_opts(value: Value) -> Result<Value, ApiError> {
    let ok = value.as_object().is_some_and(|o| {
        o.iter()
            .all(|(k, v)| k == "include_usage" && v.is_boolean())
    });
    if ok {
        Ok(value)
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

fn stop(value: Value) -> Result<Value, ApiError> {
    let ok = match &value {
        Value::String(_) => true,
        Value::Array(items) => items.len() <= 4 && items.iter().all(Value::is_string),
        _ => false,
    };
    if ok {
        Ok(value)
    } else {
        Err(wrong_type(
            "stop",
            "a string or an array of up to 4 strings",
        ))
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

fn tools_unsupported(param: &str) -> ApiError {
    ApiError::invalid_request(
        "tools_not_supported",
        "tool calling is not supported by OpenGate yet",
    )
    .with_param(param)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const VM: &str = "opengate/local";

    fn ok(req: Value) -> Value {
        chat_request(req, VM)
            .map(|r| r.into_upstream("llama3.1:8b"))
            .unwrap()
    }

    fn err(req: Value) -> (&'static str, Option<String>) {
        let e = chat_request(req, VM).err().expect("should be rejected");
        assert_eq!(e.status, 400);
        (e.code, e.param)
    }

    fn user(text: &str) -> Value {
        json!([{ "role": "user", "content": text }])
    }

    #[test]
    fn translates_minimal_request() {
        let body = ok(json!({ "model": VM, "messages": user("hi") }));
        assert_eq!(
            body,
            json!({ "model": "llama3.1:8b", "messages": user("hi"), "stream": false })
        );
    }

    #[test]
    fn keeps_supported_options_and_maps_max_completion_tokens() {
        let body = ok(json!({
            "model": VM, "messages": user("hi"), "stream": true,
            "stream_options": { "include_usage": true },
            "temperature": 0.2, "top_p": 0.9, "max_completion_tokens": 50,
            "stop": ["\n"], "seed": 7, "user": "u1", "tools": [], "tool_choice": "none",
            "presence_penalty": null,
        }));
        assert_eq!(body["max_tokens"], 50);
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"], json!({ "include_usage": true }));
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("user").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("presence_penalty").is_none());
    }

    #[test]
    fn normalizes_messages() {
        let body = ok(json!({ "model": VM, "messages": [
            { "role": "developer", "content": "be brief" },
            { "role": "user", "name": "c", "content": [{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }] },
        ]}));
        assert_eq!(
            body["messages"],
            json!([{ "role": "system", "content": "be brief" }, { "role": "user", "content": "a\nb" }])
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
    fn rejects_tools() {
        let tools = json!([{ "type": "function", "function": { "name": "f" } }]);
        assert_eq!(
            err(json!({ "model": VM, "messages": user("x"), "tools": tools })).0,
            "tools_not_supported"
        );
        assert_eq!(
            err(json!({ "model": VM, "messages": user("x"), "tool_choice": "auto" })).0,
            "tools_not_supported"
        );
        let msgs = json!([{ "role": "tool", "content": "r", "tool_call_id": "1" }]);
        assert_eq!(
            err(json!({ "model": VM, "messages": msgs })).0,
            "tools_not_supported"
        );
    }

    #[test]
    fn rejects_unknown_and_malformed_fields() {
        let (code, param) = err(json!({ "model": VM, "messages": user("x"), "logprobs": true }));
        assert_eq!(
            (code, param.as_deref()),
            ("unsupported_parameter", Some("logprobs"))
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
        assert_eq!(err(json!([1, 2])).0, "invalid_request");
    }
}
