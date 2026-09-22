use super::parse;
use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext,
};
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::{self, Agent, ToolClass};
use crate::error::GitAiError;
use std::collections::HashMap;
use std::path::PathBuf;

/// Preset for the TRAE (TraeCode) hook system.
///
/// TraeCode fires Claude-Code-style hook events with TRAE-specific payloads
/// (see TRAE "Hook 配置详解"): stdin JSON carries `session_id`, `cwd`,
/// `hook_event_name`, `tool_use_id`, the standardized `tool_name`
/// (`Write`/`Edit` for file edits, `RunCommand` for terminal commands), and
/// `tool_input` with the file path or command. TRAE payloads do not carry a
/// transcript path or model name, so no stream source is attached and the
/// model stays "unknown" until transcript streaming support is added.
pub struct TraePreset;

impl AgentPreset for TraePreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let data: serde_json::Value = serde_json::from_str(hook_input)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let tool_class = parse::optional_str(&data, "tool_name")
            .map(|name| bash_tool::classify_tool(Agent::Trae, name))
            .unwrap_or(ToolClass::Skip);
        if tool_class == ToolClass::Skip {
            return Ok(Vec::new());
        }

        let cwd = parse::required_str(&data, "cwd")?;
        let session_id = parse::str_or_default(&data, "session_id", "unknown");
        let hook_event = parse::optional_str(&data, "hook_event_name");
        let tool_use_id = parse::str_or_default(&data, "tool_use_id", "bash");

        let is_bash = tool_class == ToolClass::Bash;

        let context = PresetContext {
            agent_id: AgentId {
                tool: "trae".to_string(),
                id: session_id.to_string(),
                // TRAE hook payloads do not include the model name.
                model: "unknown".to_string(),
            },
            external_session_id: session_id.to_string(),
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(cwd),
            metadata: HashMap::new(),
        };

        let event = match (hook_event, is_bash) {
            (Some("PreToolUse"), true) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id: tool_use_id.to_string(),
                command: parse::bash_command_from_hook_input(&data),
            }),
            (Some("PreToolUse"), false) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths: parse::file_paths_from_tool_input(&data, cwd),
                dirty_files: None,
                tool_use_id: Some(tool_use_id.to_string()),
            }),
            (Some("PostToolUse"), true) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id: tool_use_id.to_string(),
                command: parse::bash_command_from_hook_input(&data),
                stream_source: None,
            }),
            (Some("PostToolUse"), false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths: parse::file_paths_from_tool_input(&data, cwd),
                dirty_files: None,
                stream_source: None,
                tool_use_id: Some(tool_use_id.to_string()),
            }),
            // Other TRAE hook events (SessionStart, UserPromptSubmit, Stop,
            // Notification) are not edit checkpoints.
            _ => return Ok(Vec::new()),
        };

        Ok(vec![event])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_trae_hook_input(event: &str, tool: &str) -> String {
        json!({
            "session_id": "sess-1",
            "cwd": "/home/user/project",
            "hook_event_name": event,
            "workspace_roots": ["/home/user/project"],
            "tool_use_id": "tu-1",
            "tool_name": tool,
            "llm_tool_name": tool,
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string()
    }

    #[test]
    fn test_trae_pre_file_edit() {
        let input = make_trae_hook_input("PreToolUse", "Write");
        let events = TraePreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "trae");
                assert_eq!(e.context.external_session_id, "sess-1");
                assert_eq!(e.context.agent_id.model, "unknown");
                assert_eq!(e.context.trace_id, "t_test123456789a");
                assert_eq!(e.context.cwd, PathBuf::from("/home/user/project"));
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                assert_eq!(e.tool_use_id.as_deref(), Some("tu-1"));
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_trae_post_file_edit() {
        let input = make_trae_hook_input("PostToolUse", "Edit");
        let events = TraePreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(e) => {
                assert_eq!(e.context.agent_id.tool, "trae");
                assert_eq!(e.context.external_session_id, "sess-1");
                assert_eq!(
                    e.file_paths,
                    vec![PathBuf::from("/home/user/project/src/main.rs")]
                );
                assert!(e.stream_source.is_none());
                assert_eq!(e.tool_use_id.as_deref(), Some("tu-1"));
            }
            _ => panic!("Expected PostFileEdit"),
        }
    }

    #[test]
    fn test_trae_pre_bash_call() {
        let input = json!({
            "session_id": "sess-1",
            "cwd": "/home/user/project",
            "hook_event_name": "PreToolUse",
            "tool_use_id": "tu-2",
            "tool_name": "RunCommand",
            "tool_input": {"command": "cargo test"}
        })
        .to_string();
        let events = TraePreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "trae");
                assert_eq!(e.tool_use_id, "tu-2");
                assert_eq!(e.command.as_deref(), Some("cargo test"));
            }
            _ => panic!("Expected PreBashCall"),
        }
    }

    #[test]
    fn test_trae_post_bash_call() {
        let input = json!({
            "session_id": "sess-1",
            "cwd": "/home/user/project",
            "hook_event_name": "PostToolUse",
            "tool_use_id": "tu-2",
            "tool_name": "RunCommand",
            "tool_input": {"command": "cargo test"},
            "tool_response": {"exit_code": 0}
        })
        .to_string();
        let events = TraePreset.parse(&input, "t_test123456789a").unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(e.context.agent_id.tool, "trae");
                assert_eq!(e.tool_use_id, "tu-2");
                assert_eq!(e.command.as_deref(), Some("cargo test"));
                assert!(e.stream_source.is_none());
            }
            _ => panic!("Expected PostBashCall"),
        }
    }

    #[test]
    fn test_trae_ignores_read_only_and_unsupported_tools() {
        for hook_event in ["PreToolUse", "PostToolUse"] {
            for tool_name in [
                "Read",
                "Glob",
                "Grep",
                "LS",
                "WebSearch",
                "WebFetch",
                "AskUserQuestion",
                "Skill",
                "mcp__Git__git_status",
                "UnknownTool",
            ] {
                let input = json!({
                    "hook_event_name": hook_event,
                    "tool_name": tool_name,
                    "session_id": "sess-1",
                    "cwd": "/home/user/project",
                    "tool_input": {}
                })
                .to_string();

                let events = TraePreset.parse(&input, "t_test123456789a").unwrap();
                assert!(
                    events.is_empty(),
                    "{hook_event} {tool_name} unexpectedly produced events"
                );
            }
        }
    }

    #[test]
    fn test_trae_skips_non_tool_events() {
        for hook_event in ["SessionStart", "UserPromptSubmit", "Stop", "Notification"] {
            let input = json!({
                "hook_event_name": hook_event,
                "tool_name": "Write",
                "session_id": "sess-1",
                "cwd": "/home/user/project",
                "tool_input": {"file_path": "src/main.rs"}
            })
            .to_string();

            let events = TraePreset.parse(&input, "t_test123456789a").unwrap();
            assert!(
                events.is_empty(),
                "{hook_event} unexpectedly produced events"
            );
        }
    }

    #[test]
    fn test_trae_defaults_session_id_and_tool_use_id() {
        let input = json!({
            "cwd": "/home/user/project",
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        let events = TraePreset.parse(&input, "t_test123456789a").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(e) => {
                assert_eq!(e.context.external_session_id, "unknown");
                assert_eq!(e.tool_use_id.as_deref(), Some("bash"));
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_trae_missing_cwd_is_error() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "src/main.rs"}
        })
        .to_string();
        assert!(TraePreset.parse(&input, "t_test123456789a").is_err());
    }

    #[test]
    fn test_trae_ignored_hook_produces_no_checkpoint_requests() {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Read",
            "session_id": "sess-1",
            "cwd": "/home/user/project"
        })
        .to_string();

        let requests = crate::commands::checkpoint_agent::orchestrator::execute_preset_checkpoint(
            "trae", &input,
        )
        .unwrap();
        assert!(requests.is_empty());
    }
}
