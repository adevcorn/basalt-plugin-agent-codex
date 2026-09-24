//! OpenAI Codex CLI Agent plugin for Basalt.
//!
//! Provides `CAP_AGENT_LAUNCHER` for running OpenAI Codex CLI (`codex`) agent sessions.
//!
//! Launch contract (verified against `codex-cli 0.156`):
//! - Fresh turn: `codex exec --json --model {model} {prompt}`
//! - Resume: `codex exec resume {session_id} --json --model {model} {prompt}`
//! - Reasoning effort and MCP wiring are passed via `-c` config overrides in
//!   `basalt_agent_prepare_launch` (there are no `--reasoning-effort` /
//!   `--mcp-config` / `--continue` flags).
//! - `codex` exposes no `models` subcommand, so model discovery is host-side
//!   via a static fallback list (see `fetch_agent_model_infos`).
//!
//! Event parsing handles the real `codex exec --json` JSONL shapes:
//! `thread.started`, `turn.started`, `item.started|updated|completed` (with the
//! payload nested under `item`), `turn.completed|failed`, and top-level `error`.

use basalt_plugin_sdk::prelude::*;

pub const PLUGIN_NAME: &str = "codex";
pub const PLUGIN_VERSION: &str = "0.2.0";

basalt_plugin_meta! {
    name:              "codex",
    version:           "0.2.0",
    hook_flags:        CAP_AGENT_LAUNCHER,
    provides:          "agent-launcher@codex/v1",
    requires:          "",
    optional_requires: "",
    file_globs:        "",
    activates_on:      "",
    activation_events: "",
}

/// Launch-contract types shared with the Basalt host as JSON.
///
/// These mirror `basalt-core/src/agent_metadata.rs`. They intentionally live
/// here (rather than in `basalt-plugin-sdk`, which no longer exports them) so
/// the plugin stays self-contained and buildable against the current SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StandardTool {
    Read,
    Write,
    Execute,
    Question,
}

/// A single file to materialize into the agent's workspace before launch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentWorkspaceFile {
    pub relative_path: String,
    pub content: String,
}

/// Request passed (as JSON) to `basalt_agent_prepare_launch` by the host.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentLaunchRequest {
    #[serde(default)]
    pub mcp_url: Option<String>,
    #[serde(default)]
    pub disabled_tools: Vec<StandardTool>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub workspace_path: Option<String>,
}

/// Result of launch preparation: extra CLI args, env vars, and workspace files.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentLaunchPreparation {
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub workspace_files: Vec<AgentWorkspaceFile>,
}

/// Prompt wrapper baked into the launch templates.
///
/// The `[Reasoning effort: {variant}]` tag serves two purposes: it records the
/// requested effort where the model can see it, and its `{variant}`
/// placeholder suppresses the host's generic `--variant <v>` injection (a flag
/// `codex` does not accept). The real effort switch is applied via
/// `-c model_reasoning_effort=…` in [`prepare_codex_launch`].
const PROMPT_TEMPLATE: &str = "[Workspace: .] MANDATORY: Use Basalt MCP tools for workspace file operations. [Reasoning effort: {variant}] User instruction: {prompt}";

#[no_mangle]
pub extern "C" fn basalt_agent_metadata() -> u64 {
    // NOTE: `codex exec` has no `--output-format`, `--reasoning-effort`,
    // `--mcp-config`, or `--continue` flags. JSONL streaming is `--json`,
    // resume is the `exec resume <id>` subcommand, and effort/MCP are `-c`
    // config overrides (see `prepare_codex_launch`).
    // `--dangerously-bypass-approvals-and-sandbox` keeps the non-interactive
    // run autonomous inside Basalt's disposable shadow workspace; it is the
    // only approval mode `exec resume` also accepts, keeping fresh and resumed
    // turns consistent.
    let meta = AgentMetadata {
        name: "OpenAI Codex CLI".into(),
        executable: "codex".into(),
        args: vec!["exec".into(), "--json".into()],
        resume_new_args: vec![
            "exec".into(),
            "--json".into(),
            "--dangerously-bypass-approvals-and-sandbox".into(),
            "--model".into(),
            "{model}".into(),
            PROMPT_TEMPLATE.into(),
        ],
        resume_cont_args: vec![
            "exec".into(),
            "resume".into(),
            "{session_id}".into(),
            "--json".into(),
            "--dangerously-bypass-approvals-and-sandbox".into(),
            "--model".into(),
            "{model}".into(),
            PROMPT_TEMPLATE.into(),
        ],
        execution_tier: AgentExecutionTier::MountedWorkspace,
        workspace_capabilities: vec!["mcp".into(), "shadow".into()],
        protocol: AgentProtocol::Cli,
    };
    let bytes = encode_agent_metadata(&meta);
    pack_output(bytes)
}

#[no_mangle]
pub extern "C" fn basalt_agent_settings_schema() -> u64 {
    // The host decodes this export with the binary `encode_agent_settings_schema`
    // wire format (see `basalt-plugin-sdk`), not JSON. Model discovery is done
    // host-side: `codex` has no `models` subcommand, so the host falls back to
    // a static list of known Codex models (see `fetch_agent_model_infos`).
    // No settings fields are needed here.
    pack_output(encode_agent_settings_schema(&[]))
}

/// Pure implementation of Codex launch preparation for testability and guest execution.
///
/// Reasoning effort (`low`…`max`) and the Basalt MCP server are wired via
/// `-c` config overrides, which compose over the user's `config.toml` (auth
/// and other settings are preserved). An empty/`default` variant emits no
/// override — `codex` fatally rejects an empty `model_reasoning_effort`.
pub fn prepare_codex_launch(req: &AgentLaunchRequest) -> AgentLaunchPreparation {
    let mut extra_args = Vec::new();

    if let Some(ref variant) = req.variant {
        let v = variant.trim();
        if !v.is_empty() && v != "default" {
            extra_args.push("-c".to_string());
            extra_args.push(format!("model_reasoning_effort=\"{v}\""));
        }
    }

    if let Some(ref mcp_url) = req.mcp_url {
        extra_args.push("-c".to_string());
        extra_args.push(format!("mcp_servers.basalt.url=\"{mcp_url}\""));
    }

    AgentLaunchPreparation {
        extra_args,
        env: std::collections::HashMap::new(),
        workspace_files: Vec::new(),
    }
}

#[no_mangle]
pub extern "C" fn basalt_agent_prepare_launch(
    req_ptr: *const u8,
    req_len: u32,
) -> u64 {
    let req: AgentLaunchRequest = if !req_ptr.is_null() && req_len > 0 {
        let slice = unsafe { std::slice::from_raw_parts(req_ptr, req_len as usize) };
        serde_json::from_slice(slice).unwrap_or_default()
    } else {
        AgentLaunchRequest::default()
    };

    let prep = prepare_codex_launch(&req);
    let bytes = serde_json::to_vec(&prep).unwrap_or_default();
    pack_output(bytes)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(&next) = chars.peek() {
                if next == '[' {
                    chars.next(); // consume '['
                    // consume parameters and final byte
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ('\x40'..='\x7e').contains(&ch) {
                            break;
                        }
                    }
                } else if next == ']' {
                    chars.next(); // consume ']'
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch == '\x07' {
                            break;
                        }
                        if ch == '\x1b' {
                            if let Some(&'\\') = chars.peek() {
                                chars.next();
                            }
                            break;
                        }
                    }
                } else if next == '(' || next == ')' {
                    chars.next();
                    chars.next();
                } else {
                    chars.next();
                }
            }
        } else if !c.is_control() || c == '\n' || c == '\t' {
            out.push(c);
        }
    }
    out
}

fn categorize_tool(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if lower.contains("read") || lower.contains("view") {
        "read"
    } else if lower.contains("write") || lower.contains("edit") || lower.contains("replace") {
        "write"
    } else if lower.contains("test") {
        "test"
    } else if lower.contains("build") || lower.contains("compile") {
        "build"
    } else if lower.contains("git") {
        "git"
    } else if lower.contains("search") || lower.contains("grep") || lower.contains("find") || lower.contains("glob") {
        "search"
    } else if lower.contains("run") || lower.contains("bash") || lower.contains("exec") || lower.contains("command") {
        "run"
    } else if lower.contains("ask") || lower.contains("question") {
        "question"
    } else {
        "run"
    }
}

/// Collect file paths from MCP tool arguments (`path`, `filePath`, …).
fn file_paths_from_args(args: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(p) = args
        .get("filePath")
        .or_else(|| args.get("path"))
        .or_else(|| args.get("file_path"))
        .or_else(|| args.get("file"))
        .or_else(|| args.get("target_file"))
        .or_else(|| args.get("TargetFile"))
        .and_then(|p| p.as_str())
    {
        paths.push(p.to_string());
    }
    if let Some(list) = args.get("paths").and_then(|p| p.as_array()) {
        for p in list.iter().filter_map(|p| p.as_str()) {
            paths.push(p.to_string());
        }
    }
    paths
}

/// Render an `mcp_tool_call` result to display lines: text content blocks
/// joined by newlines, falling back to `structured_content` JSON.
fn mcp_result_lines(item: &serde_json::Value) -> Vec<String> {
    if let Some(content) = item
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
    {
        let mut lines = Vec::new();
        for block in content {
            let is_text = block.get("type").and_then(|t| t.as_str()) == Some("text");
            if !is_text {
                continue;
            }
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                lines.extend(strip_ansi(text).lines().map(|l| l.to_string()));
            }
        }
        if !lines.is_empty() {
            return lines;
        }
    }
    if let Some(structured) = item.get("result").and_then(|r| r.get("structured_content")) {
        if !structured.is_null() {
            return vec![structured.to_string()];
        }
    }
    Vec::new()
}

/// State byte flags threaded through `basalt_agent_parse_line`.
const STATE_NONE: u8 = 0;
const STATE_MSG_OPEN: u8 = 1;
const STATE_THOUGHT_OPEN: u8 = 2;

/// Tool-call ids with an open entry, matching `item.started` to a later
/// `item.completed` with the same `item.id`.
static OPEN_TOOLS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn mark_open(id: &str) -> bool {
    match OPEN_TOOLS.lock() {
        Ok(mut open) => {
            if open.iter().any(|x| x == id) {
                false
            } else {
                open.push(id.to_string());
                true
            }
        }
        Err(_) => true,
    }
}

fn take_open(id: &str) -> bool {
    match OPEN_TOOLS.lock() {
        Ok(mut open) => {
            if let Some(pos) = open.iter().position(|x| x == id) {
                open.swap_remove(pos);
                true
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

fn clear_open() {
    if let Ok(mut open) = OPEN_TOOLS.lock() {
        open.clear();
    }
}

/// Stateful parse of one `codex exec --json` JSONL line.
/// Returns `(new_state, events)`.
pub fn parse_codex_line(line_str: &str, open_entry: u8) -> (u8, Vec<AgentEvent>) {
    let mut events = Vec::new();

    let val: serde_json::Value = match serde_json::from_str(line_str) {
        Ok(v) => v,
        Err(_) => {
            // Plain text fallback with ANSI stripped.
            let cleaned = strip_ansi(line_str);
            if !cleaned.trim().is_empty() {
                let lower = cleaned.to_lowercase();
                let category = if lower.contains("permission requested") {
                    "question"
                } else if lower.contains("error") {
                    "diagnostic"
                } else {
                    "log"
                };
                events.push(AgentEvent::NewEntry {
                    vendor_id: format!("raw-{}", cleaned.len()),
                    tool: cleaned.chars().take(80).collect(),
                    category: category.into(),
                    raw_cmd: cleaned,
                    file_paths: Vec::new(),
                });
            }
            return (STATE_NONE, events);
        }
    };

    let event_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match event_type {
        // New thread: the resume handle for later `exec resume` turns.
        "thread.started" => {
            clear_open();
            if let Some(tid) = val
                .get("thread_id")
                .or_else(|| val.get("threadId"))
                .or_else(|| val.get("session_id"))
                .and_then(|s| s.as_str())
            {
                events.push(AgentEvent::SessionIDAvailable(tid.to_string()));
            }
            return (STATE_NONE, events);
        }
        // Turn boundary (not a session boundary): reset in-flight tools.
        "turn.started" => {
            clear_open();
            return (STATE_NONE, events);
        }
        // Turn success: the end of this agent turn.
        "turn.completed" => {
            events.push(AgentEvent::SessionEnded { success: true });
            return (STATE_NONE, events);
        }
        "turn.failed" => {
            let msg = val
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("turn failed");
            let cleaned = strip_ansi(msg);
            events.push(AgentEvent::NewEntry {
                vendor_id: format!("err-{}", cleaned.len()),
                tool: cleaned.chars().take(80).collect(),
                category: "message".into(),
                raw_cmd: cleaned.clone(),
                file_paths: Vec::new(),
            });
            events.push(AgentEvent::SessionEnded { success: false });
            return (STATE_NONE, events);
        }
        "error" => {
            let msg = val.get("message").and_then(|m| m.as_str()).unwrap_or("stream error");
            let cleaned = strip_ansi(msg);
            // Transient reconnect notices are non-fatal progress updates.
            if cleaned.contains("Reconnecting") {
                events.push(AgentEvent::NewEntry {
                    vendor_id: format!("log-{}", cleaned.len()),
                    tool: cleaned.chars().take(80).collect(),
                    category: "log".into(),
                    raw_cmd: cleaned,
                    file_paths: Vec::new(),
                });
                return (open_entry, events);
            }
            events.push(AgentEvent::NewEntry {
                vendor_id: format!("err-{}", cleaned.len()),
                tool: cleaned.chars().take(80).collect(),
                category: "message".into(),
                raw_cmd: cleaned.clone(),
                file_paths: Vec::new(),
            });
            events.push(AgentEvent::SessionEnded { success: false });
            return (STATE_NONE, events);
        }
        "item.started" | "item.updated" | "item.completed" => {
            return parse_codex_item(event_type, &val, open_entry);
        }
        _ => {}
    }

    (open_entry, events)
}

fn parse_codex_item(
    event_type: &str,
    val: &serde_json::Value,
    open_entry: u8,
) -> (u8, Vec<AgentEvent>) {
    let mut events = Vec::new();
    let item = val.get("item").unwrap_or(val);
    let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let item_id = item.get("id").and_then(|i| i.as_str()).unwrap_or("item");
    let is_done = event_type == "item.completed";

    match item_type {
        "command_execution" => {
            let command = item.get("command").and_then(|c| c.as_str()).unwrap_or("command");
            let status = item.get("status").and_then(|s| s.as_str()).unwrap_or("");
            let done = is_done || status == "completed" || status == "failed";
            if !done {
                if mark_open(item_id) {
                    events.push(AgentEvent::NewEntry {
                        vendor_id: item_id.to_string(),
                        tool: command.to_string(),
                        category: "run".into(),
                        raw_cmd: command.to_string(),
                        file_paths: Vec::new(),
                    });
                }
                return (STATE_NONE, events);
            }
            let exit_code = item
                .get("exit_code")
                .and_then(|c| c.as_i64())
                .map(|c| c as i32)
                .unwrap_or(if status == "failed" { 1 } else { 0 });
            let output = item.get("aggregated_output").and_then(|o| o.as_str()).unwrap_or("");
            let lines: Vec<String> = strip_ansi(output).lines().map(|l| l.to_string()).collect();
            if !take_open(item_id) {
                events.push(AgentEvent::NewEntry {
                    vendor_id: item_id.to_string(),
                    tool: command.to_string(),
                    category: "run".into(),
                    raw_cmd: command.to_string(),
                    file_paths: Vec::new(),
                });
            }
            events.push(AgentEvent::CloseEntry {
                vendor_id: item_id.to_string(),
                exit_code,
                output_lines: lines,
            });
            return (STATE_NONE, events);
        }
        "mcp_tool_call" => {
            let server = item.get("server").and_then(|s| s.as_str()).unwrap_or("");
            let tool = item.get("tool").and_then(|t| t.as_str()).unwrap_or("tool");
            let display = if server.is_empty() {
                tool.to_string()
            } else {
                format!("{server}/{tool}")
            };
            let category = categorize_tool(tool);
            let args = item.get("arguments").cloned().unwrap_or(serde_json::Value::Null);
            let raw_cmd = if args.is_null() {
                String::new()
            } else {
                args.to_string()
            };
            let file_paths = if args.is_null() {
                Vec::new()
            } else {
                file_paths_from_args(&args)
            };
            let status = item.get("status").and_then(|s| s.as_str()).unwrap_or("");
            let has_error = item.get("error").map(|e| !e.is_null()).unwrap_or(false);
            let done = is_done || status == "completed" || status == "failed";
            if !done {
                if mark_open(item_id) {
                    events.push(AgentEvent::NewEntry {
                        vendor_id: item_id.to_string(),
                        tool: display,
                        category: category.into(),
                        raw_cmd,
                        file_paths,
                    });
                }
                return (STATE_NONE, events);
            }
            let mut lines = mcp_result_lines(item);
            if let Some(err) = item.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
                lines.push(strip_ansi(err));
            }
            let exit_code = if status == "failed" || has_error { 1 } else { 0 };
            if !take_open(item_id) {
                events.push(AgentEvent::NewEntry {
                    vendor_id: item_id.to_string(),
                    tool: display,
                    category: category.into(),
                    raw_cmd,
                    file_paths,
                });
            }
            events.push(AgentEvent::CloseEntry {
                vendor_id: item_id.to_string(),
                exit_code,
                output_lines: lines,
            });
            return (STATE_NONE, events);
        }
        "agent_message" => {
            let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let cleaned = strip_ansi(text);
            if cleaned.trim().is_empty() {
                return (open_entry, events);
            }
            if open_entry == STATE_MSG_OPEN {
                events.push(AgentEvent::AppendToEntry {
                    vendor_id: "agent-response".to_string(),
                    text: cleaned,
                });
            } else {
                events.push(AgentEvent::NewEntry {
                    vendor_id: "agent-response".to_string(),
                    tool: cleaned.chars().take(80).collect(),
                    category: "message".into(),
                    raw_cmd: cleaned,
                    file_paths: Vec::new(),
                });
            }
            return (STATE_MSG_OPEN, events);
        }
        "reasoning" => {
            let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let cleaned = strip_ansi(text);
            if cleaned.trim().is_empty() {
                return (open_entry, events);
            }
            if open_entry == STATE_THOUGHT_OPEN {
                events.push(AgentEvent::AppendToEntry {
                    vendor_id: "agent-thought".to_string(),
                    text: cleaned,
                });
            } else {
                events.push(AgentEvent::NewEntry {
                    vendor_id: "agent-thought".to_string(),
                    tool: cleaned.chars().take(80).collect(),
                    category: "thought".into(),
                    raw_cmd: cleaned,
                    file_paths: Vec::new(),
                });
            }
            return (STATE_THOUGHT_OPEN, events);
        }
        "file_change" => {
            let mut paths = Vec::new();
            let mut summary = Vec::new();
            if let Some(changes) = item.get("changes").and_then(|c| c.as_array()) {
                for ch in changes {
                    if let Some(p) = ch.get("path").and_then(|p| p.as_str()) {
                        let kind = ch.get("kind").and_then(|k| k.as_str()).unwrap_or("update");
                        summary.push(format!("{kind} {p}"));
                        paths.push(p.to_string());
                    }
                }
            }
            let raw_cmd = item
                .get("changes")
                .map(|c| c.to_string())
                .unwrap_or_default();
            let status = item.get("status").and_then(|s| s.as_str()).unwrap_or("completed");
            let done = is_done || status == "completed" || status == "failed";
            if !done {
                if mark_open(item_id) {
                    events.push(AgentEvent::NewEntry {
                        vendor_id: item_id.to_string(),
                        tool: "file_change".to_string(),
                        category: "write".into(),
                        raw_cmd,
                        file_paths: paths,
                    });
                }
                return (STATE_NONE, events);
            }
            let exit_code = if status == "failed" { 1 } else { 0 };
            if !take_open(item_id) {
                events.push(AgentEvent::NewEntry {
                    vendor_id: item_id.to_string(),
                    tool: "file_change".to_string(),
                    category: "write".into(),
                    raw_cmd,
                    file_paths: paths,
                });
            }
            events.push(AgentEvent::CloseEntry {
                vendor_id: item_id.to_string(),
                exit_code,
                output_lines: summary,
            });
            return (STATE_NONE, events);
        }
        "web_search" => {
            let query = item.get("query").and_then(|q| q.as_str()).unwrap_or("web search");
            if !take_open(item_id) {
                events.push(AgentEvent::NewEntry {
                    vendor_id: item_id.to_string(),
                    tool: query.to_string(),
                    category: "search".into(),
                    raw_cmd: query.to_string(),
                    file_paths: Vec::new(),
                });
            }
            events.push(AgentEvent::CloseEntry {
                vendor_id: item_id.to_string(),
                exit_code: 0,
                output_lines: Vec::new(),
            });
            return (STATE_NONE, events);
        }
        "todo_list" => {
            let rendered = item
                .get("items")
                .and_then(|i| i.as_array())
                .map(|list| {
                    list.iter()
                        .map(|t| {
                            let text = t.get("text").and_then(|x| x.as_str()).unwrap_or("");
                            let done = t.get("completed").and_then(|c| c.as_bool()).unwrap_or(false);
                            format!("{} {text}", if done { "✓" } else { "☐" })
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            if !is_done {
                if mark_open(item_id) {
                    events.push(AgentEvent::NewEntry {
                        vendor_id: item_id.to_string(),
                        tool: "todo_list".to_string(),
                        category: "list".into(),
                        raw_cmd: rendered,
                        file_paths: Vec::new(),
                    });
                } else if !rendered.trim().is_empty() {
                    events.push(AgentEvent::AppendToEntry {
                        vendor_id: item_id.to_string(),
                        text: rendered,
                    });
                }
                return (STATE_NONE, events);
            }
            if !take_open(item_id) {
                events.push(AgentEvent::NewEntry {
                    vendor_id: item_id.to_string(),
                    tool: "todo_list".to_string(),
                    category: "list".into(),
                    raw_cmd: rendered.clone(),
                    file_paths: Vec::new(),
                });
            }
            events.push(AgentEvent::CloseEntry {
                vendor_id: item_id.to_string(),
                exit_code: 0,
                output_lines: if rendered.is_empty() {
                    Vec::new()
                } else {
                    vec![rendered]
                },
            });
            return (STATE_NONE, events);
        }
        // Non-fatal warning surfaced as an item: visible, but not terminal.
        "error" => {
            let msg = item.get("message").and_then(|m| m.as_str()).unwrap_or("tool warning");
            let cleaned = strip_ansi(msg);
            if !take_open(item_id) {
                events.push(AgentEvent::NewEntry {
                    vendor_id: item_id.to_string(),
                    tool: cleaned.chars().take(80).collect(),
                    category: "message".into(),
                    raw_cmd: cleaned.clone(),
                    file_paths: Vec::new(),
                });
            }
            events.push(AgentEvent::CloseEntry {
                vendor_id: item_id.to_string(),
                exit_code: 1,
                output_lines: vec![cleaned],
            });
            return (STATE_NONE, events);
        }
        // Unrecognized item payloads are metadata — ignore rather than
        // polluting the chat log with raw JSON.
        _ => {}
    }

    (open_entry, events)
}

#[no_mangle]
pub extern "C" fn basalt_agent_parse_line(
    line_ptr: *const u8,
    line_len: u32,
    state_ptr: *const u8,
    state_len: u32,
) -> u64 {
    if line_ptr.is_null() || line_len == 0 {
        return pack_output(encode_agent_parse_output(&[], &[]));
    }
    let line_slice = unsafe { std::slice::from_raw_parts(line_ptr, line_len as usize) };
    let line_str = match std::str::from_utf8(line_slice) {
        Ok(s) => s.trim(),
        Err(_) => return pack_output(encode_agent_parse_output(&[], &[])),
    };

    if line_str.is_empty() {
        return pack_output(encode_agent_parse_output(&[], &[]));
    }

    let open_entry = if !state_ptr.is_null() && state_len > 0 {
        let state_slice = unsafe { std::slice::from_raw_parts(state_ptr, state_len as usize) };
        state_slice[0]
    } else {
        STATE_NONE
    };

    let (new_state, events) = parse_codex_line(line_str, open_entry);
    pack_output(encode_agent_parse_output(&[new_state], &events))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that share the global `OPEN_TOOLS` state: parallel
    /// tests would otherwise clear each other's in-flight tool ids.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn reset_open() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_open();
        guard
    }

    #[test]
    fn test_settings_schema_is_empty_binary_wire_format() {
        // The host decodes `basalt_agent_settings_schema` as binary wire
        // format ([count: u16 LE], then per-field records) — not JSON. Codex
        // model discovery is host-side (static fallback: `codex` exposes no
        // `models` subcommand), so the plugin declares zero fields.
        let bytes = encode_agent_settings_schema(&[]);
        assert_eq!(bytes, vec![0u8, 0u8]);
    }

    #[test]
    fn test_prepare_codex_launch_effort_and_mcp() {
        let req = AgentLaunchRequest {
            mcp_url: Some("http://127.0.0.1:8080".into()),
            variant: Some("high".into()),
            ..Default::default()
        };
        let prep = prepare_codex_launch(&req);
        // Effort and MCP ride on `-c` overrides; no workspace files needed.
        assert_eq!(
            prep.extra_args,
            vec![
                "-c",
                "model_reasoning_effort=\"high\"",
                "-c",
                "mcp_servers.basalt.url=\"http://127.0.0.1:8080\"",
            ]
        );
        assert!(prep.workspace_files.is_empty());
    }

    #[test]
    fn test_prepare_codex_launch_default_variant_emits_no_effort() {
        // `codex` fatally rejects an empty `model_reasoning_effort`, so a
        // default variant must not emit the override at all.
        for variant in [None, Some("".to_string()), Some("default".to_string())] {
            let req = AgentLaunchRequest {
                mcp_url: None,
                variant,
                ..Default::default()
            };
            let prep = prepare_codex_launch(&req);
            assert!(prep.extra_args.is_empty());
        }
    }

    #[test]
    fn test_parse_thread_started() {
        let _guard = reset_open();
        let line = r#"{"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a53"}"#;
        let (state, events) = parse_codex_line(line, STATE_NONE);
        assert_eq!(state, STATE_NONE);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::SessionIDAvailable(id) => {
                assert_eq!(id, "0199a213-81c0-7800-8aa1-bbab2a035a53")
            }
            _ => panic!("expected SessionIDAvailable"),
        }
    }

    #[test]
    fn test_parse_command_execution_lifecycle() {
        let _guard = reset_open();
        let started = r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"bash -lc ls","aggregated_output":"","status":"in_progress"}}"#;
        let (st, evs) = parse_codex_line(started, STATE_NONE);
        assert_eq!(st, STATE_NONE);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::NewEntry { vendor_id, tool, category, .. } => {
                assert_eq!(vendor_id, "item_1");
                assert_eq!(tool, "bash -lc ls");
                assert_eq!(category, "run");
            }
            _ => panic!("expected NewEntry for command start"),
        }

        let done = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"bash -lc ls","aggregated_output":"docs\nsrc\n","exit_code":0,"status":"completed"}}"#;
        let (st, evs) = parse_codex_line(done, st);
        assert_eq!(st, STATE_NONE);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::CloseEntry { vendor_id, exit_code, output_lines } => {
                assert_eq!(vendor_id, "item_1");
                assert_eq!(*exit_code, 0);
                assert_eq!(output_lines, &vec!["docs".to_string(), "src".to_string()]);
            }
            _ => panic!("expected CloseEntry for command completion"),
        }
    }

    #[test]
    fn test_parse_command_completed_without_start() {
        let _guard = reset_open();
        let done = r#"{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"bash -lc false","aggregated_output":"","exit_code":1,"status":"failed"}}"#;
        let (_, evs) = parse_codex_line(done, STATE_NONE);
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[0], AgentEvent::NewEntry { .. }));
        match &evs[1] {
            AgentEvent::CloseEntry { exit_code, .. } => assert_eq!(*exit_code, 1),
            _ => panic!("expected CloseEntry"),
        }
    }

    #[test]
    fn test_parse_agent_message_stateful() {
        let _guard = reset_open();
        let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"Done."}}"#;
        let (st, evs) = parse_codex_line(line, STATE_NONE);
        assert_eq!(st, STATE_MSG_OPEN);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::NewEntry { category, raw_cmd, .. } => {
                assert_eq!(category, "message");
                assert_eq!(raw_cmd, "Done.");
            }
            _ => panic!("expected NewEntry message"),
        }

        let (st, evs) = parse_codex_line(line, st);
        assert_eq!(st, STATE_MSG_OPEN);
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], AgentEvent::AppendToEntry { .. }));
    }

    #[test]
    fn test_parse_reasoning() {
        let _guard = reset_open();
        let line = r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"Scanning docs"}}"#;
        let (st, evs) = parse_codex_line(line, STATE_NONE);
        assert_eq!(st, STATE_THOUGHT_OPEN);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::NewEntry { category, .. } => assert_eq!(category, "thought"),
            _ => panic!("expected thought entry"),
        }
    }

    #[test]
    fn test_parse_mcp_tool_call() {
        let _guard = reset_open();
        let started = r#"{"type":"item.started","item":{"id":"item_5","type":"mcp_tool_call","server":"basalt","tool":"read_file","arguments":{"path":"src/lib.rs"},"status":"in_progress"}}"#;
        let (_, evs) = parse_codex_line(started, STATE_NONE);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::NewEntry { tool, category, file_paths, .. } => {
                assert_eq!(tool, "basalt/read_file");
                assert_eq!(category, "read");
                assert_eq!(file_paths, &vec!["src/lib.rs".to_string()]);
            }
            _ => panic!("expected NewEntry for mcp start"),
        }

        let done = r#"{"type":"item.completed","item":{"id":"item_5","type":"mcp_tool_call","server":"basalt","tool":"read_file","arguments":{"path":"src/lib.rs"},"result":{"content":[{"type":"text","text":"fn main() {}"}]},"status":"completed"}}"#;
        let (_, evs) = parse_codex_line(done, STATE_NONE);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::CloseEntry { vendor_id, exit_code, output_lines } => {
                assert_eq!(vendor_id, "item_5");
                assert_eq!(*exit_code, 0);
                assert_eq!(output_lines, &vec!["fn main() {}".to_string()]);
            }
            _ => panic!("expected CloseEntry for mcp completion"),
        }
    }

    #[test]
    fn test_parse_file_change_and_search() {
        let _guard = reset_open();
        let line = r#"{"type":"item.completed","item":{"id":"item_4","type":"file_change","changes":[{"path":"docs/a.md","kind":"add"},{"path":"docs/b.md","kind":"update"}],"status":"completed"}}"#;
        let (_, evs) = parse_codex_line(line, STATE_NONE);
        assert_eq!(evs.len(), 2);
        match &evs[0] {
            AgentEvent::NewEntry { category, file_paths, .. } => {
                assert_eq!(category, "write");
                assert_eq!(file_paths.len(), 2);
            }
            _ => panic!("expected NewEntry for file_change"),
        }

        let line = r#"{"type":"item.completed","item":{"id":"item_7","type":"web_search","query":"codex exec json schema"}}"#;
        let (_, evs) = parse_codex_line(line, STATE_NONE);
        assert_eq!(evs.len(), 2);
        match &evs[0] {
            AgentEvent::NewEntry { category, .. } => assert_eq!(category, "search"),
            _ => panic!("expected NewEntry for web_search"),
        }
    }

    #[test]
    fn test_parse_turn_lifecycle() {
        let _guard = reset_open();
        let (_, evs) = parse_codex_line(r#"{"type":"turn.started"}"#, STATE_MSG_OPEN);
        assert!(evs.is_empty());

        let (_, evs) = parse_codex_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":2}}"#,
            STATE_NONE,
        );
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::SessionEnded { success } => assert!(success),
            _ => panic!("expected successful SessionEnded"),
        }

        let (_, evs) = parse_codex_line(
            r#"{"type":"turn.failed","error":{"message":"boom"}}"#,
            STATE_NONE,
        );
        assert_eq!(evs.len(), 2);
        match &evs[1] {
            AgentEvent::SessionEnded { success } => assert!(!success),
            _ => panic!("expected failed SessionEnded"),
        }
    }

    #[test]
    fn test_parse_error_events() {
        let _guard = reset_open();
        // Transient reconnect notice: visible, non-terminal.
        let (_, evs) = parse_codex_line(
            r#"{"type":"error","message":"Reconnecting... 1/5"}"#,
            STATE_NONE,
        );
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], AgentEvent::NewEntry { .. }));

        // Fatal stream error ends the turn.
        let (_, evs) = parse_codex_line(
            r#"{"type":"error","message":"stream error: broken pipe"}"#,
            STATE_NONE,
        );
        assert_eq!(evs.len(), 2);
        match &evs[1] {
            AgentEvent::SessionEnded { success } => assert!(!success),
            _ => panic!("expected failed SessionEnded"),
        }

        // Non-fatal item warning closes its own entry, not the turn.
        let (_, evs) = parse_codex_line(
            r#"{"type":"item.completed","item":{"id":"item_9","type":"error","message":"command output truncated"}}"#,
            STATE_NONE,
        );
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[1], AgentEvent::CloseEntry { .. }));
    }

    #[test]
    fn test_parse_todo_list() {
        let _guard = reset_open();
        let started = r#"{"type":"item.started","item":{"id":"item_8","type":"todo_list","items":[{"text":"Scan","completed":false}]}}"#;
        let (_, evs) = parse_codex_line(started, STATE_NONE);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::NewEntry { category, .. } => assert_eq!(category, "list"),
            _ => panic!("expected NewEntry for todo_list"),
        }

        let done = r#"{"type":"item.completed","item":{"id":"item_8","type":"todo_list","items":[{"text":"Scan","completed":true}]}}"#;
        let (_, evs) = parse_codex_line(done, STATE_NONE);
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], AgentEvent::CloseEntry { .. }));
    }

    #[test]
    fn test_parse_ignores_unknown_shapes() {
        let _guard = reset_open();
        let (_, evs) = parse_codex_line(r#"{"type":"item.completed","item":{"id":"x","type":"something_new"}}"#, STATE_NONE);
        assert!(evs.is_empty());
    }
}
