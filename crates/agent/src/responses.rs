//! The seed starts each inference from its local journal. This avoids carrying
//! provider-specific response IDs across a model change or a gateway failover.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub(crate) fn messages(history: &[Value]) -> Result<Vec<Value>> {
    let mut input = Vec::with_capacity(history.len());
    for message in history {
        let role = message["role"].as_str().context("missing message role")?;
        let text = message["content"].as_str().unwrap_or_default();
        let content = match role {
            "tool" => format!("Recorded tool result {}: {text}", message["tool_call_id"]),
            "assistant" if message["tool_calls"].is_array() => {
                format!("{text}\nRecorded tool calls: {}", message["tool_calls"])
            }
            _ => text.to_owned(),
        };
        if !content.is_empty() {
            let role = if role == "tool" { "user" } else { role };
            input.push(json!({"role":role,"content":content}));
        }
    }
    Ok(input)
}

pub(crate) fn request(chat: &Value) -> Result<Value> {
    let tools: Vec<_> = chat["tools"]
        .as_array()
        .context("missing function tools")?
        .iter()
        .map(|tool| {
            let mut definition = tool["function"].clone();
            definition["type"] = json!("function");
            // Our local validators accept optional fields omitted by the caller.
            definition["strict"] = json!(false);
            definition
        })
        .collect();
    Ok(
        json!({"model":chat["model"],"input":[],"tools":tools,"tool_choice":"auto","stream":false,"store":false,"max_output_tokens":chat["max_tokens"]}),
    )
}

pub(crate) fn completion(response: &Value) -> Result<Value> {
    ensure!(
        response["status"] == "completed" && response["error"].is_null(),
        "incomplete model response"
    );
    let mut content = String::new();
    let mut calls = Vec::new();
    for item in response["output"]
        .as_array()
        .context("missing response output")?
    {
        ensure!(
            item.get("status").is_none_or(|v| v == "completed"),
            "incomplete response output item"
        );
        match item["type"].as_str() {
            Some("reasoning") => {}
            Some("function_call") => {
                let function = json!({"name":item["name"],"arguments":item["arguments"]});
                calls.push(json!({"id":item["call_id"],"type":"function","function":function}));
            }
            Some("message") => {
                let parts = item["content"]
                    .as_array()
                    .context("invalid response message")?;
                for part in parts {
                    let key = match part["type"].as_str() {
                        Some("output_text") => "text",
                        Some("refusal") => "refusal",
                        _ => anyhow::bail!("unsupported response content"),
                    };
                    content.push_str(part[key].as_str().context("invalid response text")?);
                }
            }
            _ => anyhow::bail!("unsupported response output type"),
        }
    }
    let reason = if calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    Ok(
        json!({"choices":[{"finish_reason":reason,"message":{"role":"assistant","content":content,"tool_calls":calls}}]}),
    )
}
