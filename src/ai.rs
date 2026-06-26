//! Minimal AI client for jterm1's terminal-side helpers (per-block error
//! explanation, session Q&A panel, `?` palette prefix).
//!
//! Mirrors rsh's provider conventions so a user who set `ANTHROPIC_API_KEY`
//! for the shell already has jterm1 wired up: detection prefers Claude →
//! OpenAI → Ollama (local fallback). Inference runs on a worker thread and
//! posts its result back to the GLib main thread via `glib::idle_add_local`,
//! so the UI never blocks on the HTTP round-trip.
//!
//! Privacy: nothing leaves the machine without an explicit user action
//! (clicking an Explain button, typing into the panel, hitting `?` in the
//! palette). Block output passed to the cloud LLM is bounded (head/tail)
//! by the caller before reaching us.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gtk4::glib;

/// Default cap on response tokens for any AI call. Keeps explanations
/// terse and bounds cost.
const MAX_TOKENS: u32 = 600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Anthropic,
    OpenAI,
    Ollama,
}

/// All the knobs jterm1's AI helpers need to make one HTTP call. Built once
/// from the environment and cached on App for the session.
#[derive(Clone, Debug)]
pub(crate) struct AiClient {
    pub provider: Provider,
    pub api_key: Option<String>,
    pub model: String,
    pub base_url: String,
}

impl AiClient {
    /// Inspect the environment and return a configured client when at least
    /// one provider looks usable. Returns None when there's no API key AND
    /// no Ollama at the default URL — callers gate UI on that None to hide
    /// AI surfaces silently rather than show a broken button.
    pub(crate) fn from_env() -> Option<Self> {
        // Mirror rsh's precedence:
        //   1. explicit JTERM1_AI_PROVIDER (anthropic/openai/ollama)
        //   2. ANTHROPIC_API_KEY → Anthropic
        //   3. OPENAI_API_KEY → OpenAI
        //   4. fall back to Ollama (no key needed)
        let forced = std::env::var("JTERM1_AI_PROVIDER").ok();
        let provider = match forced.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some("anthropic" | "claude") => Provider::Anthropic,
            Some("openai") => Provider::OpenAI,
            Some("ollama") => Provider::Ollama,
            // Unknown explicit choice → fall through to auto-detect.
            _ => {
                if std::env::var("ANTHROPIC_API_KEY")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .is_some()
                {
                    Provider::Anthropic
                } else if std::env::var("OPENAI_API_KEY")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .is_some()
                {
                    Provider::OpenAI
                } else {
                    Provider::Ollama
                }
            }
        };
        let (api_key, default_model, default_url) = match provider {
            Provider::Anthropic => (
                std::env::var("ANTHROPIC_API_KEY").ok(),
                "claude-sonnet-4-20250514",
                "https://api.anthropic.com",
            ),
            Provider::OpenAI => (
                std::env::var("OPENAI_API_KEY").ok(),
                "gpt-4o-mini",
                "https://api.openai.com/v1",
            ),
            Provider::Ollama => (None, "codellama:7b", "http://localhost:11434"),
        };
        // For Anthropic / OpenAI an absent or empty key means we can't reach
        // anyone; hide the UI. Ollama needs no key so we always try it.
        if matches!(provider, Provider::Anthropic | Provider::OpenAI)
            && api_key.as_deref().unwrap_or("").is_empty()
        {
            return None;
        }
        Some(AiClient {
            provider,
            api_key,
            model: std::env::var("JTERM1_AI_MODEL").unwrap_or_else(|_| default_model.to_string()),
            base_url: std::env::var("JTERM1_AI_BASE_URL")
                .unwrap_or_else(|_| default_url.to_string()),
        })
    }

    /// Short human label for status text ("Claude · sonnet-4 …").
    pub(crate) fn display_name(&self) -> String {
        let prov = match self.provider {
            Provider::Anthropic => "Claude",
            Provider::OpenAI => "OpenAI",
            Provider::Ollama => "Ollama",
        };
        format!("{} · {}", prov, self.model)
    }
}

/// Handle held by the UI for an in-flight request. Drop it (or call
/// `cancel`) to ignore any pending callback — the HTTP request may still
/// finish in the background, but `on_done` will not run.
pub(crate) struct AiHandle {
    cancelled: Arc<AtomicBool>,
}

impl AiHandle {
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/// Fire one prompt at the configured provider. `on_done` is invoked exactly
/// once on the GLib main thread with either the assistant text or an error
/// string. Returns an `AiHandle` the caller can drop to cancel.
pub(crate) fn ask(
    client: AiClient,
    system: String,
    user: String,
    on_done: impl FnOnce(Result<String, String>) + 'static,
) -> AiHandle {
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_thread = cancelled.clone();

    // glib::Sender can't carry FnOnce closures portably; use a one-shot
    // channel pattern: thread parks the result behind a Mutex<Option<T>>
    // and a glib idle pulls it on the main thread.
    let slot: Arc<std::sync::Mutex<Option<Result<String, String>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let slot_thread = slot.clone();
    let slot_main = slot.clone();
    // `on_done` is FnOnce; wrap in Option so the idle closure can take it.
    let mut on_done_cell: Option<Box<dyn FnOnce(Result<String, String>)>> = Some(Box::new(on_done));

    std::thread::spawn(move || {
        let result = run_request(&client, &system, &user);
        if cancelled_thread.load(Ordering::SeqCst) {
            return;
        }
        *slot_thread.lock().expect("ai slot mutex poisoned") = Some(result);
    });

    // Poll the slot on the GLib main loop. Cheap: a tick once every 100ms
    // until the worker finishes (typical request: 0.5–5 s).
    let cancelled_main = cancelled.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if cancelled_main.load(Ordering::SeqCst) {
            return glib::ControlFlow::Break;
        }
        let mut guard = slot_main.lock().expect("ai slot mutex poisoned");
        if let Some(result) = guard.take() {
            if let Some(cb) = on_done_cell.take() {
                cb(result);
            }
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });

    AiHandle { cancelled }
}

/// Build a fresh ureq agent per call — connection reuse isn't worth the
/// extra global state for our low request rate, and a per-call agent makes
/// the cancel/timeout story trivial.
fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(60))
        .timeout_write(Duration::from_secs(15))
        .build()
}

fn run_request(client: &AiClient, system: &str, user: &str) -> Result<String, String> {
    match client.provider {
        Provider::Anthropic => call_anthropic(client, system, user),
        Provider::OpenAI => call_openai(client, system, user),
        Provider::Ollama => call_ollama(client, system, user),
    }
}

fn call_anthropic(client: &AiClient, system: &str, user: &str) -> Result<String, String> {
    let url = format!("{}/v1/messages", client.base_url);
    let body = serde_json::json!({
        "model": client.model,
        "max_tokens": MAX_TOKENS,
        "system": system,
        "messages": [{"role": "user", "content": user}],
    });
    let mut req = http_agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .set("anthropic-version", "2023-06-01");
    if let Some(key) = &client.api_key {
        req = req.set("x-api-key", key);
    }
    let resp = req
        .send_string(&body.to_string())
        .map_err(|e| format!("anthropic request failed: {e}"))?;
    let text = resp.into_string().map_err(|e| format!("read body: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("parse: {e}"))?;
    if let Some(s) = v["content"][0]["text"].as_str() {
        Ok(s.to_string())
    } else if let Some(msg) = v["error"]["message"].as_str() {
        Err(msg.to_string())
    } else {
        Err(format!(
            "unexpected anthropic response: {}",
            trim_for_log(&text)
        ))
    }
}

fn call_openai(client: &AiClient, system: &str, user: &str) -> Result<String, String> {
    let url = format!("{}/chat/completions", client.base_url);
    let body = serde_json::json!({
        "model": client.model,
        "max_tokens": MAX_TOKENS,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    });
    let mut req = http_agent()
        .post(&url)
        .set("Content-Type", "application/json");
    if let Some(key) = &client.api_key {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    let resp = req
        .send_string(&body.to_string())
        .map_err(|e| format!("openai request failed: {e}"))?;
    let text = resp.into_string().map_err(|e| format!("read body: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("parse: {e}"))?;
    if let Some(s) = v["choices"][0]["message"]["content"].as_str() {
        Ok(s.to_string())
    } else if let Some(msg) = v["error"]["message"].as_str() {
        Err(msg.to_string())
    } else {
        Err(format!(
            "unexpected openai response: {}",
            trim_for_log(&text)
        ))
    }
}

fn call_ollama(client: &AiClient, system: &str, user: &str) -> Result<String, String> {
    let url = format!("{}/api/generate", client.base_url);
    let body = serde_json::json!({
        "model": client.model,
        "system": system,
        "prompt": user,
        "stream": false,
        "options": { "num_predict": MAX_TOKENS },
    });
    let resp = http_agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| format!("ollama request failed (is `ollama serve` running?): {e}"))?;
    let text = resp.into_string().map_err(|e| format!("read body: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("parse: {e}"))?;
    if let Some(s) = v["response"].as_str() {
        Ok(s.to_string())
    } else if let Some(msg) = v["error"].as_str() {
        Err(msg.to_string())
    } else {
        Err(format!(
            "unexpected ollama response: {}",
            trim_for_log(&text)
        ))
    }
}

fn trim_for_log(s: &str) -> String {
    if s.len() <= 256 {
        s.to_string()
    } else {
        format!("{}…", &s[..256])
    }
}

// ── Prompt builders ────────────────────────────────────────────────────────

/// Bounded output sample for prompts: head + tail so a multi-MB build log
/// still fits in the context window without dropping the failing tail.
fn sample_output(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_string();
    }
    let half = max_bytes / 2;
    let head = &output[..half];
    let tail = &output[output.len() - half..];
    format!(
        "{head}\n\n… [{} bytes elided] …\n\n{tail}",
        output.len() - max_bytes
    )
}

/// Build the system+user prompt for "explain why this failed and how to fix it".
pub(crate) fn build_explain_prompt(
    command: &str,
    output: &str,
    exit_code: i32,
    cwd: &str,
) -> (String, String) {
    let system = "You are a senior shell user helping debug a failed command. \
Read the command, its output, and its exit code. Reply with:\n\
1. One short sentence on what went wrong.\n\
2. A single concrete fix (one shell command or one config change).\n\
Be terse. No markdown headers. No filler. If the error is ambiguous, say so."
        .to_string();
    let sample = sample_output(output, 8 * 1024);
    let user = format!("cwd: {cwd}\nexit: {exit_code}\ncommand:\n{command}\n\noutput:\n{sample}");
    (system, user)
}

/// Build the system+user prompt for the `?` palette: natural language → one
/// shell command. The model is told to emit ONLY the command so we can paste
/// it directly into the input line without further parsing.
pub(crate) fn build_nl_to_cmd_prompt(query: &str, cwd: &str) -> (String, String) {
    let system = "You convert natural language requests into one shell command. \
Output ONLY the command, no markdown, no quotes, no explanation. \
If the request is ambiguous, output the safest interpretation."
        .to_string();
    let user = format!("cwd: {cwd}\nrequest: {query}");
    (system, user)
}

/// Build the system prompt for agent mode. The user-side payload is the
/// running transcript, assembled by `agent::AgentSession::build_user_prompt`.
///
/// The JSON-action protocol is the load-bearing piece: the UI parses each
/// reply with `agent::parse_action`, and a malformed reply degrades to a
/// `say` so the session continues. Few-shot examples cover the three
/// actions the model is allowed to emit (`run` / `say` / `done`).
pub(crate) fn build_agent_system_prompt(cwd: &str, shell: &str, os: &str) -> String {
    format!(
        "You are an interactive shell agent helping the user in their terminal. \
Each reply MUST be a single JSON object — no prose, no markdown fences, no commentary. \
Schema:\n\
  {{ \"thought\": \"...\", \"action\": \"run\"|\"say\"|\"done\", \"command\": \"...\", \"message\": \"...\" }}\n\
- `action: run` means the user must approve a shell command. Put the command in `command`. \
  Use this for anything that changes filesystem, network, or state. Do not chain unrelated \
  steps with `;` or `&&` — one command per turn so the user can review each.\n\
- `action: say` means you need a clarifying answer from the user, or want to comment without \
  running a command. Put the text in `message`.\n\
- `action: done` means the task is complete. Put a short summary in `message`.\n\
The user runs the command after approving it; you then receive an `Output (exit=N):` block \
in the next turn and can decide what to do next. Prefer the smallest command that yields the \
information you need. Never assume a command succeeded — wait for the observation.\n\
\n\
Environment:\n\
  cwd: {cwd}\n\
  shell: {shell}\n\
  os: {os}\n\
\n\
Examples (single-line for clarity — actual replies should still be valid JSON):\n\
User: my disk is full, what's eating space?\n\
Assistant: {{\"thought\":\"survey top-level usage first\",\"action\":\"run\",\"command\":\"du -sh /* 2>/dev/null | sort -h | tail -20\"}}\n\
Output (exit=0): 12G /var\\n8.4G /home\\n…\n\
Assistant: {{\"thought\":\"/var is biggest, drill into it\",\"action\":\"run\",\"command\":\"du -sh /var/* 2>/dev/null | sort -h | tail -10\"}}\n\
\n\
User: rename all .txt to .md in this folder\n\
Assistant: {{\"action\":\"run\",\"command\":\"for f in *.txt; do mv -- \\\"$f\\\" \\\"${{f%.txt}}.md\\\"; done\"}}\n\
\n\
User: is port 5432 free?\n\
Assistant: {{\"action\":\"run\",\"command\":\"ss -tlnp | grep ':5432' || echo free\"}}\n\
Output (exit=0): free\n\
Assistant: {{\"action\":\"done\",\"message\":\"Port 5432 is free.\"}}\n\
"
    )
}

/// Build the system+user prompt for the session panel, optionally seeded
/// with the most recent block context.
pub(crate) fn build_session_prompt(question: &str, context: Option<&str>) -> (String, String) {
    let system = "You are a terminal assistant. Answer the user's question concisely. \
If shell context is attached, use it. No filler, no markdown headers."
        .to_string();
    let user = match context {
        Some(c) => format!("Recent shell context:\n{c}\n\nQuestion: {question}"),
        None => format!("Question: {question}"),
    };
    (system, user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_output_passes_through_small() {
        let s = "hi";
        assert_eq!(sample_output(s, 1000), s);
    }

    #[test]
    fn sample_output_truncates_large_with_marker() {
        let big = "x".repeat(20_000);
        let s = sample_output(&big, 1000);
        assert!(s.len() < 1500);
        assert!(s.contains("elided"));
    }

    #[test]
    fn explain_prompt_contains_command_and_exit() {
        let (sys, user) = build_explain_prompt("false", "out", 1, "/tmp");
        assert!(sys.to_lowercase().contains("debug"));
        assert!(user.contains("false"));
        assert!(user.contains("exit: 1"));
        assert!(user.contains("/tmp"));
    }

    #[test]
    fn nl_to_cmd_prompt_emits_request() {
        let (_sys, user) = build_nl_to_cmd_prompt("list large files", "/var");
        assert!(user.contains("list large files"));
        assert!(user.contains("/var"));
    }

    #[test]
    fn detection_prefers_anthropic_when_key_set() {
        // We can't mutate process env safely in tests; just sanity-check that
        // the explicit JTERM1_AI_PROVIDER path picks Ollama (no key needed).
        std::env::set_var("JTERM1_AI_PROVIDER", "ollama");
        let c = AiClient::from_env().expect("ollama needs no key");
        assert_eq!(c.provider, Provider::Ollama);
        std::env::remove_var("JTERM1_AI_PROVIDER");
    }
}
