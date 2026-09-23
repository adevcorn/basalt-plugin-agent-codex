//! OpenAI Codex CLI Agent plugin for Basalt.
//!
//! Provides `CAP_AGENT_LAUNCHER` for running OpenAI Codex CLI (`codex`) agent sessions
//! with dynamic model discovery, reasoning effort variants (`low`, `medium`, `high`),
//! MCP server wiring, and stream-json event parsing.

use basalt_plugin_sdk::prelude::*;

pub const PLUGIN_NAME: &str = "codex";
pub const PLUGIN_VERSION: &str = "0.1.0";

basalt_plugin_meta! {
    name:              "codex",
    version:           "0.1.0",
    hook_flags:        CAP_AGENT_LAUNCHER,
    provides:          "agent-launcher@codex/v1",
    requires:          "",
    optional_requires: "",
    file_globs:        "",
    activates_on:      "",
    activation_events: "",
}

#[no_mangle]
pub extern "C" fn basalt_agent_metadata() -> u64 {
    let meta = AgentMetadata {
        name: "OpenAI Codex CLI".into(),
        executable: "codex".into(),
        args: vec![
            "exec".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--model".into(),
            "{model}".into(),
            "--reasoning-effort".into(),
            "{variant}".into(),
            "[Workspace: .] MANDATORY: Use Basalt MCP tools for workspace file operations. User instruction: {prompt}".into(),
        ],
        resume_new_args: vec![
            "exec".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--model".into(),
            "{model}".into(),
            "--reasoning-effort".into(),
            "{variant}".into(),
            "[Workspace: .] MANDATORY: Use Basalt MCP tools for workspace file operations. User instruction: {prompt}".into(),
        ],
        resume_cont_args: vec![
            "exec".into(),
            "--continue".into(),
            "{session_id}".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--model".into(),
            "{model}".into(),
            "--reasoning-effort".into(),
            "{variant}".into(),
            "[Workspace: .] MANDATORY: Use Basalt MCP tools for workspace file operations. User instruction: {prompt}".into(),
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
    let schema = serde_json::json!({
        "plugin": "codex",
        "dynamic_models": true,
        "models_command": "codex models",
        "variants": [
            "default",
            "low",
            "medium",
            "high"
        ],
        "default_variant": "default"
    });
    let bytes = serde_json::to_vec(&schema).unwrap_or_default();
    pack_output(bytes)
}

pub fn prepare_codex_launch(req: &AgentLaunchRequest) -> AgentLaunchPreparation {
    let mut workspace_files = Vec::new();
    let mut extra_args = Vec::new();

    if let Some(ref mcp_url) = req.mcp_url {
        let mcp_config = serde_json::json!({
            "mcpServers": {
                "basalt": {
                    "url": mcp_url
                }
            }
        });
        let content = serde_json::to_string_pretty(&mcp_config).unwrap_or_default();
        workspace_files.push(AgentWorkspaceFile {
            relative_path: ".codex/mcp.json".to_string(),
            content: content.clone(),
        });
        workspace_files.push(AgentWorkspaceFile {
            relative_path: "codex_mcp.json".to_string(),
            content,
        });
        extra_args.push("--mcp-config".to_string());
        extra_args.push(".codex/mcp.json".to_string());
    }

    AgentLaunchPreparation {
        extra_args,
        env: std::collections::HashMap::new(),
        workspace_files,
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

const STATE_NONE: u8 = 0;
const STATE_MSG_OPEN: u8 = 1;

pub fn parse_codex_line(line_str: &str, open_entry: u8) -> (u8, Vec<AgentEvent>) {
    let mut events = Vec::new();

    if let Ok(val) = serde_json::from_str::<serde_json::Value>(line_str) {
        let event_type = val.get("type").or_else(|| val.get("event")).and_then(|t| t.as_str()).unwrap_or("");

        if event_type == "init" || event_type == "session_start" || event_type == "thread.created" {
            if let Some(cid) = val.get("thread_id")
                .or_else(|| val.get("session_id"))
                .or_else(|| val.get("sessionId"))
                .and_then(|s| s.as_str())
            {
                events.push(AgentEvent::SessionIDAvailable(cid.to_string()));
            }
            return (STATE_NONE, events);
        }

        if event_type == "item_start" || event_type == "tool_call" || event_type == "tool.call" {
            let tool_name = val.get("name")
                .or_else(|| val.get("tool"))
                .or_else(|| val.get("tool_name"))
                .and_then(|s| s.as_str())
                .unwrap_or("tool");

            let vendor_id = val.get("id")
                .or_else(|| val.get("call_id"))
                .and_then(|s| s.as_str())
                .unwrap_or(tool_name)
                .to_string();

            let raw_cmd = val.get("arguments")
                .or_else(|| val.get("args"))
                .map(|a| a.to_string())
                .unwrap_or_default();

            let lower = tool_name.to_lowercase();
            let category = if lower.contains("read") || lower.contains("view") {
                "read"
            } else if lower.contains("write") || lower.contains("edit") || lower.contains("replace") {
                "write"
            } else if lower.contains("search") || lower.contains("grep") || lower.contains("find") {
                "search"
            } else if lower.contains("exec") || lower.contains("run") || lower.contains("command") {
                "run"
            } else {
                "run"
            };

            let mut file_paths = Vec::new();
            if let Some(args) = val.get("arguments").or_else(|| val.get("args")) {
                if let Some(p) = args.get("path").or_else(|| args.get("file")).and_then(|s| s.as_str()) {
                    file_paths.push(p.to_string());
                }
            }

            events.push(AgentEvent::NewEntry {
                vendor_id,
                tool: tool_name.to_string(),
                category: category.to_string(),
                raw_cmd,
                file_paths,
            });
            return (STATE_NONE, events);
        }

        if event_type == "item_done" || event_type == "tool_done" || event_type == "tool.result" {
            let vendor_id = val.get("id")
                .or_else(|| val.get("call_id"))
                .or_else(|| val.get("name"))
                .and_then(|s| s.as_str())
                .unwrap_or("tool")
                .to_string();

            let exit_code = val.get("exit_code").and_then(|c| c.as_i64()).unwrap_or(0) as i32;

            events.push(AgentEvent::CloseEntry {
                vendor_id,
                exit_code,
                output_lines: vec![],
            });
            return (STATE_NONE, events);
        }

        if event_type == "message" || event_type == "assistant_message" {
            let text = val.get("content").or_else(|| val.get("text")).and_then(|s| s.as_str()).unwrap_or("");
            if open_entry != STATE_MSG_OPEN {
                events.push(AgentEvent::NewEntry {
                    vendor_id: "msg-main".into(),
                    tool: "assistant".into(),
                    category: "message".into(),
                    raw_cmd: String::new(),
                    file_paths: vec![],
                });
            }
            if !text.is_empty() {
                events.push(AgentEvent::AppendToEntry {
                    vendor_id: "msg-main".into(),
                    text: text.to_string(),
                });
            }
            return (STATE_MSG_OPEN, events);
        }

        if event_type == "session_end" || event_type == "turn_complete" || event_type == "completed" {
            let success = val.get("success").and_then(|s| s.as_bool()).unwrap_or(true);
            events.push(AgentEvent::SessionEnded {
                success,
                error: val.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()),
            });
            return (STATE_NONE, events);
        }
    }

    (open_entry, events)
}

#[no_mangle]
pub extern "C" fn agent_init_state() -> u64 {
    pack_output(vec![STATE_NONE])
}

#[no_mangle]
pub extern "C" fn agent_parse_line(
    state_ptr: *const u8,
    state_len: u32,
    line_ptr: *const u8,
    line_len: u32,
) -> u64 {
    let state_byte = if !state_ptr.is_null() && state_len > 0 {
        unsafe { *state_ptr }
    } else {
        STATE_NONE
    };

    let line_str = if !line_ptr.is_null() && line_len > 0 {
        let slice = unsafe { std::slice::from_raw_parts(line_ptr, line_len as usize) };
        std::str::from_utf8(slice).unwrap_or_default()
    } else {
        ""
    };

    let (new_state, events) = parse_codex_line(line_str, state_byte);
    let bytes = encode_agent_parse_output(&[new_state], &events);
    pack_output(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prepare_codex_launch_mcp() {
        let req = AgentLaunchRequest {
            mcp_url: Some("http://127.0.0.1:8080".into()),
            ..Default::default()
        };
        let prep = prepare_codex_launch(&req);
        assert_eq!(prep.extra_args, vec!["--mcp-config", ".codex/mcp.json"]);
        assert_eq!(prep.workspace_files.len(), 2);
    }

    #[test]
    fn test_parse_codex_line_session_start() {
        let line = r#"{"type":"session_start","thread_id":"th-12345"}"#;
        let (state, events) = parse_codex_line(line, STATE_NONE);
        assert_eq!(state, STATE_NONE);
        assert_eq!(events.len(), 1);
        if let AgentEvent::SessionIDAvailable(id) = &events[0] {
            assert_eq!(id, "th-12345");
        } else {
            panic!("expected SessionIDAvailable");
        }
    }
}
