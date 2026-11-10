use serde_json::{Value, json};

pub fn live_ready() -> bool {
    std::env::var("LIGHTBULB_MODEL").is_ok()
        && (std::env::var("OPENAI_API_KEY").is_ok() || std::env::var("ANTHROPIC_API_KEY").is_ok())
}
/// A bounded role runner. The caller validates the selected action and its arguments;
/// model output is a proposal and never supplies identity, approval, or authorization.
pub async fn step(
    mode: &str,
    role: &str,
    observation: Value,
    action: &str,
) -> Result<Value, String> {
    if mode == "scripted" {
        return Ok(json!({"action":action,"arguments":observation,"runner":"scripted"}));
    }
    if mode != "live" || !live_ready() {
        return Err("Live mode needs LIGHTBULB_MODEL and OPENAI_API_KEY or ANTHROPIC_API_KEY in the server environment".into());
    }
    let model = std::env::var("LIGHTBULB_MODEL").map_err(|e| e.to_string())?;
    let system = format!(
        "You are the {role} agent in a simulated business transaction. Use the supplied observation to propose the next tool action. This role's tool is {action}. Reply ONLY with a JSON object {{\"action\":\"{action}\",\"arguments\":{{...}},\"reason\":\"short explanation\"}}. Preserve supplied transaction facts. Do not claim an action ran, an owner approved, or an order completed. Credentials and authority belong to the host, not you."
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(45))
        .build()
        .map_err(|e| e.to_string())?;
    let output = if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        let response=client.post("https://api.openai.com/v1/chat/completions").bearer_auth(key).json(&json!({"model":model,"messages":[{"role":"system","content":system},{"role":"user","content":observation.to_string()}],"response_format":{"type":"json_object"},"max_completion_tokens":500})).send().await.map_err(|e|e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("model provider returned {}", response.status()));
        }
        let v: Value = response.json().await.map_err(|e| e.to_string())?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .ok_or("model response missing content")?
            .to_owned()
    } else {
        let key = std::env::var("ANTHROPIC_API_KEY").map_err(|e| e.to_string())?;
        let response=client.post("https://api.anthropic.com/v1/messages").header("x-api-key",key).header("anthropic-version","2023-06-01").json(&json!({"model":model,"system":system,"max_tokens":500,"messages":[{"role":"user","content":observation.to_string()}]})).send().await.map_err(|e|e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("model provider returned {}", response.status()));
        }
        let v: Value = response.json().await.map_err(|e| e.to_string())?;
        v["content"]
            .as_array()
            .ok_or("missing model content")?
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("")
    };
    let parsed: Value =
        serde_json::from_str(&output).map_err(|_| "model output was not valid JSON")?;
    if parsed["action"].as_str() != Some(action) || !parsed["arguments"].is_object() {
        return Err("model proposed an invalid role action".into());
    }
    // Model cannot alter structured facts at the action boundary.
    if parsed["arguments"] != observation {
        return Err("model changed transaction arguments; action refused".into());
    }
    Ok(parsed)
}
