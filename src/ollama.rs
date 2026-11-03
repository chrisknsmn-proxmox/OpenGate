//! Chat Completions ⇄ Ollama native chat API (`POST /api/chat`) translation.
//!
//! The native API is used rather than Ollama's OpenAI-compatible endpoint
//! because only it can set the context size per request (`options.num_ctx`)
//! and refuse to silently drop history when the prompt does not fit
//! (`truncate: false`, `shift: false`): the request fails with a clear
//! `context_length_exceeded` instead of the model answering a truncated
//! conversation.
//!
//! Tools: OpenAI function tools map one-to-one. Ollama has no `tool_choice`,
//! so `none` sends no tools, a forced function sends only that tool, and
//! `required` / a forced function / `parallel_tool_calls: false` add a short
//! system instruction; the answer is then checked, and a violation is an
//! error (`tool_choice_not_satisfied`), never passed off as compliant.
//! Ollama returns tool calls whole, with parsed arguments; each becomes an
//! OpenAI tool call with a stable ID (Ollama's own, or a generated one) and
//! arguments serialized as a JSON object string. OpenGate never runs tools.

use serde_json::{json, Map, Value};

use crate::{
    error::ApiError,
    validate::{ChatRequest, Message, ResponseFormat, ToolChoice},
};

/// Longest upstream error detail passed back to the client.
const MAX_ERROR_DETAIL: usize = 200;

/// The `/api/chat` request body for `req`.
pub(crate) fn request_body(
    req: &ChatRequest,
    model: &str,
    num_ctx: Option<u32>,
) -> Result<Value, ApiError> {
    let offered = req.offered_tools();
    if req.response_format.is_some() && !offered.is_empty() {
        return Err(ApiError::invalid_request(
            "unsupported_parameter",
            "the local model cannot combine response_format with tools; send one or the other",
        )
        .with_param("response_format"));
    }

    let mut messages: Vec<Value> = req.messages.iter().map(message).collect();
    if let Some(instruction) = tool_instruction(req, !offered.is_empty()) {
        match messages.first_mut() {
            Some(first) if first["role"] == "system" => {
                let content = first["content"].as_str().unwrap_or_default();
                first["content"] = format!("{content}\n\n{instruction}").into();
            }
            _ => messages.insert(0, json!({ "role": "system", "content": instruction })),
        }
    }

    let mut options = Map::new();
    if let Some(n) = num_ctx {
        options.insert("num_ctx".into(), n.into());
    }
    for (name, value) in &req.sampling {
        options.insert((*name).into(), value.clone());
    }
    if let Some(n) = req.max_tokens {
        options.insert("num_predict".into(), n.into());
    }
    if !req.stop.is_empty() {
        options.insert("stop".into(), req.stop.clone().into());
    }

    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": req.stream,
        "options": options,
        // Fail instead of silently dropping history (truncate) or the start
        // of the context mid-answer (shift); see the module docs.
        "truncate": false,
        "shift": false,
    });
    if !offered.is_empty() {
        body["tools"] = offered
            .iter()
            .map(|t| {
                let mut f = json!({ "name": t.name, "parameters": t.parameters });
                if let Some(d) = &t.description {
                    f["description"] = d.as_str().into();
                }
                json!({ "type": "function", "function": f })
            })
            .collect();
    }
    match &req.response_format {
        Some(ResponseFormat::JsonObject) => body["format"] = "json".into(),
        Some(ResponseFormat::JsonSchema { schema, .. }) => body["format"] = schema.clone(),
        None => {}
    }
    Ok(body)
}

fn message(m: &Message) -> Value {
    match m {
        Message::System(c) => json!({ "role": "system", "content": c }),
        Message::User(c) => json!({ "role": "user", "content": c }),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let mut v = json!({ "role": "assistant", "content": content });
            if !tool_calls.is_empty() {
                v["tool_calls"] = tool_calls
                    .iter()
                    .map(|c| {
                        json!({ "id": c.id, "function": { "name": c.name, "arguments": c.arguments } })
                    })
                    .collect();
            }
            v
        }
        Message::Tool {
            call_id,
            name,
            content,
        } => json!({
            "role": "tool", "content": content, "tool_name": name, "tool_call_id": call_id,
        }),
    }
}

fn tool_instruction(req: &ChatRequest, has_tools: bool) -> Option<String> {
    if !has_tools {
        return None;
    }
    let mut parts = Vec::new();
    match &req.tool_choice {
        ToolChoice::Required => {
            parts.push("You must respond by calling one or more of the provided tools.".to_owned())
        }
        ToolChoice::Function(name) => {
            parts.push(format!("You must respond by calling the `{name}` tool."))
        }
        ToolChoice::Auto | ToolChoice::None => {}
    }
    if req.parallel_tool_calls == Some(false) {
        parts.push("Call at most one tool in this response.".to_owned());
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Maps an Ollama error message to a specific client error, when it names a
/// condition the client can act on. Ollama's messages describe the failure,
/// never the prompt.
pub(crate) fn classify_error(message: &str, num_ctx: Option<u32>) -> Option<ApiError> {
    let lower = message.to_ascii_lowercase();
    if lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("context size")
        || lower.contains("maximum context")
    {
        return Some(ApiError::context_length_exceeded(num_ctx, &short(message)));
    }
    if lower.contains("does not support tools") {
        return Some(ApiError::local_tools_unsupported());
    }
    None
}

pub(crate) fn short(message: &str) -> String {
    match message.char_indices().nth(MAX_ERROR_DETAIL) {
        Some((cut, _)) => format!("{}…", &message[..cut]),
        None => message.to_owned(),
    }
}

/// One translated piece of the model's answer.
#[derive(Debug, PartialEq)]
pub(crate) enum Event {
    Reasoning(String),
    Text(String),
    ToolCall {
        index: usize,
        id: String,
        name: String,
        /// A JSON object, serialized.
        arguments: String,
    },
    Done {
        finish_reason: &'static str,
        usage: Option<Value>,
    },
}

/// Translates Ollama's answer (streamed chunks, or the one non-streaming
/// object) and holds the response identity shared by every chunk.
pub(crate) struct Reply {
    pub id: String,
    pub created: u64,
    pub model: String,
    choice: ToolChoice,
    tools_offered: bool,
    single_call: bool,
    num_ctx: Option<u32>,
    calls: usize,
    pub done: bool,
}

impl Reply {
    pub fn new(req: &ChatRequest, model: String, num_ctx: Option<u32>) -> Self {
        Reply {
            id: format!("chatcmpl-{}", random_hex()),
            created: crate::chatgpt::store::unix_now(),
            model,
            choice: req.tool_choice.clone(),
            tools_offered: !req.offered_tools().is_empty(),
            single_call: req.parallel_tool_calls == Some(false),
            num_ctx,
            calls: 0,
            done: false,
        }
    }

    /// Translates one Ollama chunk. Nothing after `done` is expected.
    pub fn step(&mut self, chunk: &Value, out: &mut Vec<Event>) -> Result<(), ApiError> {
        if let Some(err) = chunk.get("error") {
            let msg = err.as_str().or(err["message"].as_str()).unwrap_or_default();
            return Err(classify_error(msg, self.num_ctx).unwrap_or_else(|| {
                ApiError::upstream_error("local model server reported an error mid-answer")
            }));
        }
        if !chunk.is_object() || !chunk["message"].is_object() && chunk["done"] != true {
            return Err(ApiError::upstream_invalid_response());
        }
        let msg = &chunk["message"];
        if let Some(t) = msg["thinking"].as_str().filter(|t| !t.is_empty()) {
            out.push(Event::Reasoning(t.to_owned()));
        }
        if let Some(t) = msg["content"].as_str().filter(|t| !t.is_empty()) {
            out.push(Event::Text(t.to_owned()));
        }
        match &msg["tool_calls"] {
            Value::Null => {}
            Value::Array(calls) => {
                for call in calls {
                    out.push(self.tool_call(call)?);
                }
            }
            _ => return Err(ApiError::upstream_invalid_tool_call()),
        }
        if chunk["done"] == true {
            self.done = true;
            out.push(self.finish(chunk)?);
        }
        Ok(())
    }

    fn tool_call(&mut self, call: &Value) -> Result<Event, ApiError> {
        let f = &call["function"];
        let name = f["name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .ok_or_else(ApiError::upstream_invalid_tool_call)?;
        let arguments = match &f["arguments"] {
            Value::Object(_) => f["arguments"].to_string(),
            Value::Null => "{}".to_owned(),
            // Some models emit the arguments as a JSON string.
            Value::String(s) => serde_json::from_str::<Value>(s)
                .ok()
                .filter(Value::is_object)
                .ok_or_else(ApiError::upstream_invalid_tool_call)?
                .to_string(),
            _ => return Err(ApiError::upstream_invalid_tool_call()),
        };
        if !self.tools_offered {
            return Err(ApiError::tool_choice_not_satisfied(
                "the local model called a tool although no tools were offered",
            ));
        }
        if let ToolChoice::Function(required) = &self.choice {
            if name != required {
                return Err(ApiError::tool_choice_not_satisfied(
                    "the local model called a different tool than tool_choice requires",
                ));
            }
        }
        if self.single_call && self.calls >= 1 {
            return Err(ApiError::tool_choice_not_satisfied(
                "the local model called more than one tool although parallel_tool_calls is false",
            ));
        }
        let id = call["id"]
            .as_str()
            .filter(|id| {
                !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_graphic())
            })
            .map_or_else(|| format!("call_{}", random_hex()), str::to_owned);
        let index = self.calls;
        self.calls += 1;
        Ok(Event::ToolCall {
            index,
            id,
            name: name.to_owned(),
            arguments,
        })
    }

    fn finish(&self, chunk: &Value) -> Result<Event, ApiError> {
        let finish_reason = if chunk["done_reason"] == "length" {
            "length"
        } else if self.calls > 0 {
            "tool_calls"
        } else if self.choice.forced() {
            return Err(ApiError::tool_choice_not_satisfied(
                "the local model answered without calling a tool although tool_choice requires one",
            ));
        } else {
            "stop"
        };
        let usage = match (
            chunk["prompt_eval_count"].as_u64(),
            chunk["eval_count"].as_u64(),
        ) {
            (Some(p), Some(c)) => Some(json!({
                "prompt_tokens": p, "completion_tokens": c, "total_tokens": p + c,
            })),
            _ => None,
        };
        Ok(Event::Done {
            finish_reason,
            usage,
        })
    }

    /// A `chat.completion` from Ollama's non-streaming answer.
    pub fn completion(&mut self, body: &Value) -> Result<Value, ApiError> {
        let mut events = Vec::new();
        self.step(body, &mut events)?;
        let (mut text, mut reasoning, mut calls) = (String::new(), String::new(), Vec::new());
        let mut end = None;
        for ev in events {
            match ev {
                Event::Reasoning(t) => reasoning.push_str(&t),
                Event::Text(t) => text.push_str(&t),
                Event::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } => calls.push(json!({
                    "id": id, "type": "function", "function": { "name": name, "arguments": arguments },
                })),
                Event::Done {
                    finish_reason,
                    usage,
                } => end = Some((finish_reason, usage)),
            }
        }
        let Some((finish_reason, usage)) = end else {
            return Err(ApiError::upstream_invalid_response());
        };
        let mut message = json!({ "role": "assistant", "content": text });
        if !calls.is_empty() {
            if text.is_empty() {
                message["content"] = Value::Null;
            }
            message["tool_calls"] = calls.into();
        }
        if !reasoning.is_empty() {
            message["reasoning_content"] = reasoning.into();
        }
        let mut completion = json!({
            "id": self.id, "object": "chat.completion", "created": self.created, "model": self.model,
            "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
        });
        if let Some(usage) = usage {
            completion["usage"] = usage;
        }
        Ok(completion)
    }

    /// A `chat.completion.chunk` with one choice.
    pub fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        json!({
            "id": self.id, "object": "chat.completion.chunk", "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
        })
    }
}

fn random_hex() -> String {
    let mut buf = [0u8; 12];
    getrandom::fill(&mut buf).expect("the OS random number generator is available");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::chat_request;

    fn req(v: Value) -> ChatRequest {
        let mut v = v;
        v["model"] = "m".into();
        chat_request(v, "m").unwrap()
    }

    fn tools() -> Value {
        json!([
            { "type": "function", "function": { "name": "get_weather", "description": "d",
              "parameters": { "type": "object", "properties": { "city": { "type": "string" } } } } },
            { "type": "function", "function": { "name": "get_time" } },
        ])
    }

    #[test]
    fn plain_request_body() {
        let r = req(
            json!({ "messages": [{ "role": "developer", "content": "s" }, { "role": "user", "content": "u" }],
            "temperature": 0.1, "max_tokens": 9, "stop": "x", "seed": 3 }),
        );
        let body = request_body(&r, "llama3.1:8b", Some(32768)).unwrap();
        assert_eq!(
            body,
            json!({
                "model": "llama3.1:8b",
                "messages": [{ "role": "system", "content": "s" }, { "role": "user", "content": "u" }],
                "stream": false,
                "options": { "num_ctx": 32768, "temperature": 0.1, "seed": 3, "num_predict": 9, "stop": ["x"] },
                "truncate": false, "shift": false,
            })
        );
        let body = request_body(&r, "llama3.1:8b", None).unwrap();
        assert!(body["options"].get("num_ctx").is_none());
    }

    #[test]
    fn tool_conversation_body() {
        let r = req(json!({ "stream": true, "tools": tools(), "messages": [
            { "role": "user", "content": "weather?" },
            { "role": "assistant", "content": null, "tool_calls": [
                { "id": "call_a", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" } } ] },
            { "role": "tool", "tool_call_id": "call_a", "content": "18C" },
        ]}));
        let body = request_body(&r, "m", None).unwrap();
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
        assert_eq!(body["tools"][0]["function"]["description"], "d");
        assert!(body["tools"][1]["function"].get("description").is_none());
        assert_eq!(
            body["messages"][1],
            json!({ "role": "assistant", "content": "", "tool_calls": [
                { "id": "call_a", "function": { "name": "get_weather", "arguments": { "city": "Paris" } } } ] })
        );
        assert_eq!(
            body["messages"][2],
            json!({ "role": "tool", "content": "18C", "tool_name": "get_weather", "tool_call_id": "call_a" })
        );
        assert_eq!(
            body["messages"].as_array().unwrap().len(),
            3,
            "no instruction for auto"
        );
    }

    #[test]
    fn tool_choice_translation() {
        let with = |choice: Value| {
            req(json!({ "tools": tools(), "tool_choice": choice,
            "messages": [{ "role": "system", "content": "sys" }, { "role": "user", "content": "u" }] }))
        };
        let none = request_body(&with(json!("none")), "m", None).unwrap();
        assert!(none.get("tools").is_none());
        let forced = request_body(
            &with(json!({ "type": "function", "function": { "name": "get_time" } })),
            "m",
            None,
        )
        .unwrap();
        assert_eq!(forced["tools"].as_array().unwrap().len(), 1);
        assert_eq!(forced["tools"][0]["function"]["name"], "get_time");
        assert!(forced["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("sys\n\nYou must respond by calling the `get_time` tool."));
        let mut r = with(json!("required"));
        r.parallel_tool_calls = Some(false);
        r.messages.remove(0);
        let body = request_body(&r, "m", None).unwrap();
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("at most one tool"));
    }

    #[test]
    fn response_format_translation() {
        let r = req(
            json!({ "messages": [{ "role": "user", "content": "u" }], "response_format": { "type": "json_object" } }),
        );
        assert_eq!(request_body(&r, "m", None).unwrap()["format"], "json");
        let r = req(json!({ "messages": [{ "role": "user", "content": "u" }],
            "response_format": { "type": "json_schema", "json_schema": { "name": "o", "schema": { "type": "object" } } } }));
        assert_eq!(
            request_body(&r, "m", None).unwrap()["format"],
            json!({ "type": "object" })
        );
        let r = req(
            json!({ "messages": [{ "role": "user", "content": "u" }], "tools": tools(), "response_format": { "type": "json_object" } }),
        );
        assert_eq!(
            request_body(&r, "m", None).unwrap_err().param.as_deref(),
            Some("response_format")
        );
    }

    fn steps(r: &ChatRequest, chunks: &[Value]) -> Result<Vec<Event>, ApiError> {
        let mut reply = Reply::new(r, "opengate/local".into(), Some(8192));
        let mut out = Vec::new();
        for c in chunks {
            reply.step(c, &mut out)?;
        }
        Ok(out)
    }

    fn tool_req(choice: Value) -> ChatRequest {
        req(
            json!({ "tools": tools(), "tool_choice": choice, "messages": [{ "role": "user", "content": "u" }] }),
        )
    }

    #[test]
    fn translates_multiple_tool_calls_with_stable_ids() {
        let r = tool_req(json!("auto"));
        let out = steps(&r, &[
            json!({ "message": { "role": "assistant", "content": "", "tool_calls": [
                { "id": "call_ollama1", "function": { "index": 0, "name": "get_weather", "arguments": { "city": "Paris" } } },
                { "function": { "index": 1, "name": "get_time", "arguments": "{\"tz\":\"CET\"}" } } ] }, "done": false }),
            json!({ "message": { "role": "assistant", "content": "" }, "done": true, "done_reason": "stop",
                "prompt_eval_count": 10, "eval_count": 5 }),
        ]).unwrap();
        assert_eq!(out.len(), 3);
        assert!(
            matches!(&out[0], Event::ToolCall { index: 0, id, name, arguments }
            if id == "call_ollama1" && name == "get_weather" && arguments == "{\"city\":\"Paris\"}")
        );
        let Event::ToolCall {
            index: 1,
            id,
            arguments,
            ..
        } = &out[1]
        else {
            panic!()
        };
        assert!(id.starts_with("call_") && id.len() == 29);
        assert_eq!(arguments, "{\"tz\":\"CET\"}");
        assert_eq!(
            out[2],
            Event::Done {
                finish_reason: "tool_calls",
                usage: Some(
                    json!({ "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 })
                )
            }
        );
    }

    #[test]
    fn rejects_malformed_tool_calls() {
        let r = tool_req(json!("auto"));
        for call in [
            json!({ "function": { "arguments": {} } }),
            json!({ "function": { "name": "get_time", "arguments": "not json" } }),
            json!({ "function": { "name": "get_time", "arguments": [1] } }),
        ] {
            let chunk =
                json!({ "message": { "content": "", "tool_calls": [call] }, "done": false });
            assert_eq!(
                steps(&r, &[chunk]).unwrap_err().code,
                "upstream_invalid_tool_call"
            );
        }
        let chunk = json!({ "message": { "content": "", "tool_calls": "x" }, "done": false });
        assert_eq!(
            steps(&r, &[chunk]).unwrap_err().code,
            "upstream_invalid_tool_call"
        );
    }

    #[test]
    fn enforces_tool_choice_on_the_answer() {
        let text_only =
            [json!({ "message": { "content": "hi" }, "done": true, "done_reason": "stop" })];
        assert_eq!(
            steps(&tool_req(json!("required")), &text_only)
                .unwrap_err()
                .code,
            "tool_choice_not_satisfied"
        );
        assert!(steps(&tool_req(json!("auto")), &text_only).is_ok());
        let wrong = [
            json!({ "message": { "content": "", "tool_calls": [{ "function": { "name": "get_weather", "arguments": {} } }] }, "done": false }),
        ];
        let forced = tool_req(json!({ "type": "function", "function": { "name": "get_time" } }));
        assert_eq!(
            steps(&forced, &wrong).unwrap_err().code,
            "tool_choice_not_satisfied"
        );
        assert_eq!(
            steps(&tool_req(json!("none")), &wrong).unwrap_err().code,
            "tool_choice_not_satisfied"
        );
        let mut single = tool_req(json!("auto"));
        single.parallel_tool_calls = Some(false);
        let two = [json!({ "message": { "content": "", "tool_calls": [
            { "function": { "name": "get_time", "arguments": {} } }, { "function": { "name": "get_time", "arguments": {} } }] }, "done": false })];
        assert_eq!(
            steps(&single, &two).unwrap_err().code,
            "tool_choice_not_satisfied"
        );
        // Running out of room is reported as length, not as a tool_choice failure.
        let cut = [json!({ "message": { "content": "a" }, "done": true, "done_reason": "length" })];
        assert!(matches!(
            steps(&tool_req(json!("required")), &cut).unwrap().last(),
            Some(Event::Done {
                finish_reason: "length",
                ..
            })
        ));
    }

    #[test]
    fn maps_context_and_tool_capability_errors() {
        let r = tool_req(json!("auto"));
        let e = steps(
            &r,
            &[json!({ "error": "the input length exceeds the context length", "status": 400 })],
        )
        .unwrap_err();
        assert_eq!(
            (e.status.as_u16(), e.code),
            (400, "context_length_exceeded")
        );
        let e = steps(&r, &[json!({ "error": "boom" })]).unwrap_err();
        assert_eq!(e.code, "upstream_error");
        assert_eq!(
            classify_error(
                "registry.ollama.ai/library/gemma:2b does not support tools",
                None
            )
            .unwrap()
            .code,
            "tools_not_supported"
        );
        assert!(classify_error("model not found", None).is_none());
    }

    #[test]
    fn nonstreaming_completion() {
        let r = tool_req(json!("auto"));
        let mut reply = Reply::new(&r, "opengate/local".into(), None);
        let c = reply
            .completion(
                &json!({ "model": "llama3.1:8b", "message": { "role": "assistant", "content": "",
            "tool_calls": [{ "function": { "name": "get_time", "arguments": {} } }] },
            "done": true, "done_reason": "stop", "prompt_eval_count": 3, "eval_count": 2 }),
            )
            .unwrap();
        assert_eq!(c["object"], "chat.completion");
        assert_eq!(c["model"], "opengate/local");
        assert_eq!(c["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(c["choices"][0]["message"]["content"], Value::Null);
        assert_eq!(
            c["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
        assert_eq!(c["usage"]["total_tokens"], 5);
        let mut reply = Reply::new(&r, "opengate/local".into(), None);
        assert_eq!(
            reply
                .completion(&json!({ "message": { "content": "x" } }))
                .unwrap_err()
                .code,
            "upstream_invalid_response"
        );
    }
}
