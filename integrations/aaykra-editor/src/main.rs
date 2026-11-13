//! Reference editor bridge for Aaykra Code.
//!
//! This is deliberately small: it shows the exact HTTP/SSE contract an editor
//! needs, without depending on the main crate. In a real editor you would map
//! these events to transcript blocks, diff views, and permission dialogs.

use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> Result<()> {
    let mut server = std::env::var("AAYKRA_SERVER").unwrap_or_else(|_| "http://127.0.0.1:4096".into());
    let mut token = std::env::var("AAYKRA_SERVER_TOKEN").ok();
    let mut session_reference = None;
    let mut auto_approve = false;
    let mut prompt_parts = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" => server = args.next().context("--server needs a URL")?,
            "--token" => token = Some(args.next().context("--token needs a value")?),
            "--session" => {
                session_reference = Some(args.next().context("--session needs an id")?);
            }
            "--yes" => auto_approve = true,
            "-h" | "--help" => {
                println!(
                    "usage: aaykra-editor-client [--server URL] [--token TOKEN] [--session ID] [--yes] [PROMPT]"
                );
                return Ok(());
            }
            other => prompt_parts.push(other.to_string()),
        }
    }
    let mut prompt = prompt_parts.join(" ");
    if prompt.trim().is_empty() {
        std::io::stdin().read_to_string(&mut prompt)?;
    }
    if prompt.trim().is_empty() {
        bail!("no prompt supplied");
    }
    let server = server.trim_end_matches('/').to_string();
    let client = reqwest::Client::new();

    let session = if let Some(reference) = session_reference {
        let response = request(
            client.get(format!("{server}/v1/sessions/{reference}")),
            token.as_deref(),
        )
        .send()
        .await
        .context("could not load session")?;
        json_response(response).await?
    } else {
        let response = request(
            client.post(format!("{server}/v1/sessions")),
            token.as_deref(),
        )
        .json(&json!({"name": "editor"}))
        .send()
        .await
        .context("could not create session")?;
        json_response(response).await?
    };
    let session_id = session["id"]
        .as_str()
        .context("session response has no id")?
        .to_string();
    eprintln!("session: {session_id}");

    let response = request(
        client.post(format!("{server}/v1/sessions/{session_id}/turns")),
        token.as_deref(),
    )
    .json(&json!({
        "prompt": prompt,
        "stream": true,
        "auto_approve": auto_approve,
    }))
    .send()
    .await
    .context("could not start turn")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("turn failed with {status}: {body}");
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut finished = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("stream read failed")?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(index) = buffer.find("\n\n") {
            let frame: String = buffer.drain(..index + 2).collect();
            if handle_frame(frame, &client, &server, token.as_deref(), auto_approve).await? {
                finished = true;
            }
        }
    }
    if !finished {
        bail!("stream ended without a result event");
    }
    Ok(())
}

fn request(
    request: reqwest::RequestBuilder,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    if let Some(token) = token {
        request.bearer_auth(token)
    } else {
        request
    }
}

async fn json_response(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let text = response.text().await.context("could not read response")?;
    if !status.is_success() {
        bail!("server returned {status}: {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("invalid JSON response: {text}"))
}

/// Returns true when the final result event was received.
#[allow(clippy::too_many_lines)]
async fn handle_frame(
    frame: String,
    client: &reqwest::Client,
    server: &str,
    token: Option<&str>,
    auto_approve: bool,
) -> Result<bool> {
    let mut event = "message".to_string();
    let mut data = String::new();
    for line in frame.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }
    if data.is_empty() {
        return Ok(false);
    }
    let payload: Value = serde_json::from_str(&data)
        .with_context(|| format!("invalid SSE JSON in `{event}` event: {data}"))?;
    match event.as_str() {
        "text_delta" => {
            print!("{}", payload["text"].as_str().unwrap_or_default());
            std::io::stdout().flush().ok();
        }
        "tool_started" => {
            eprintln!("\n→ {} {}", payload["name"].as_str().unwrap_or("?"), payload["input"]);
        }
        "tool_finished" => {
            eprintln!(
                "\n{} {}",
                if payload["is_error"].as_bool().unwrap_or(false) {
                    "✗"
                } else {
                    "✓"
                },
                payload["name"].as_str().unwrap_or("?")
            );
        }
        "notice" | "warning" => {
            eprintln!("\n• {}", payload["text"].as_str().unwrap_or_default());
        }
        "error" | "run_failed" => {
            eprintln!(
                "\nerror: {}",
                payload["text"]
                    .as_str()
                    .or_else(|| payload["error"].as_str())
                    .unwrap_or_default()
            );
        }
        "permission_requested" => {
            let permission = &payload["request"];
            let id = permission["id"].as_str().unwrap_or_default();
            eprintln!(
                "\npermission: {} on `{}`\n{}\n{}",
                permission["tool"].as_str().unwrap_or("?"),
                permission["target"].as_str().unwrap_or("?"),
                permission["reason"].as_str().unwrap_or_default(),
                permission["diff"].as_str().unwrap_or_default()
            );
            let decision = if auto_approve {
                json!({"decision": "allow"})
            } else {
                // Editors should render an approval dialog here. This reference
                // client denies by default so it is safe to run unattended.
                json!({"decision": "deny", "reason": "editor bridge has no permission UI"})
            };
            let response = request(
                client.post(format!("{server}/v1/permissions/{id}")),
                token,
            )
            .json(&decision)
            .send()
            .await
            .context("could not answer permission")?;
            if !response.status().is_success() {
                eprintln!("warning: permission answer returned {}", response.status());
            }
        }
        "question_requested" => {
            let id = payload["id"].as_str().unwrap_or_default();
            eprintln!(
                "\nquestion skipped: {}",
                payload["question"]["question"].as_str().unwrap_or("?")
            );
            let response = request(
                client.post(format!("{server}/v1/questions/{id}")),
                token,
            )
            .json(&json!({"answer": null}))
            .send()
            .await
            .context("could not skip question")?;
            if !response.status().is_success() {
                eprintln!("warning: question answer returned {}", response.status());
            }
        }
        "result" => {
            if payload["is_error"].as_bool().unwrap_or(false) {
                eprintln!(
                    "\nagent error: {}",
                    payload["error"].as_str().unwrap_or("unknown error")
                );
            }
            eprintln!(
                "\n↳ {}/{} · {} · ${:.4} · {}",
                payload["usage"]["input_tokens"].as_u64().unwrap_or(0),
                payload["usage"]["output_tokens"].as_u64().unwrap_or(0),
                payload["stop_reason"].as_str().unwrap_or("?"),
                payload["cost_usd"].as_f64().unwrap_or(0.0),
                payload["session_id"].as_str().unwrap_or("?"),
            );
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}
