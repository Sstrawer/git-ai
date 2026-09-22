use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use serde_json::json;
use std::fs;
use std::path::PathBuf;

/// Build a ZCode hook payload in the documented stdin format.
fn zcode_hook_input(
    event: &str,
    tool: &str,
    tool_input: serde_json::Value,
    repo_dir: &str,
) -> String {
    json!({
        "session_id": "zcode-session-1",
        "transcript_path": "/tmp/zcode-hook/transcript.jsonl",
        "cwd": repo_dir,
        "permission_mode": "default",
        "hook_event_name": event,
        "tool_use_id": "toolu_01ABC",
        "tool_name": tool,
        "tool_input": tool_input
    })
    .to_string()
}

fn parse_zcode(hook_input: &str) -> Result<Vec<ParsedHookEvent>, git_ai::error::GitAiError> {
    resolve_preset("zcode")?.parse(hook_input, "t_test")
}

// ---------------------------------------------------------------------------
// Parse-level tests
// ---------------------------------------------------------------------------

#[test]
fn test_zcode_pre_file_edit_parse() {
    let hook_input = zcode_hook_input(
        "PreToolUse",
        "Write",
        json!({"file_path": "src/main.rs"}),
        "/home/user/project",
    );
    let events = parse_zcode(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "zcode");
            assert_eq!(e.context.external_session_id, "zcode-session-1");
            assert_eq!(e.context.cwd, PathBuf::from("/home/user/project"));
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/home/user/project/src/main.rs")]
            );
            assert_eq!(e.tool_use_id.as_deref(), Some("toolu_01ABC"));
        }
        _ => panic!("Expected PreFileEdit"),
    }
}

#[test]
fn test_zcode_post_file_edit_parse() {
    let hook_input = zcode_hook_input(
        "PostToolUse",
        "Edit",
        json!({"file_path": "src/main.rs"}),
        "/home/user/project",
    );
    let events = parse_zcode(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "zcode");
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/home/user/project/src/main.rs")]
            );
            assert!(e.stream_source.is_none());
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_zcode_bash_parse() {
    for event in ["PreToolUse", "PostToolUse"] {
        let hook_input = zcode_hook_input(
            event,
            "Bash",
            json!({"command": "cargo test"}),
            "/home/user/project",
        );
        let events = parse_zcode(&hook_input).unwrap();
        assert_eq!(events.len(), 1);
        let expected_variant = if event == "PreToolUse" {
            "PreBashCall"
        } else {
            "PostBashCall"
        };
        match &events[0] {
            ParsedHookEvent::PreBashCall(e) => {
                assert_eq!(expected_variant, "PreBashCall");
                assert_eq!(e.tool_use_id, "toolu_01ABC");
                assert_eq!(e.command.as_deref(), Some("cargo test"));
            }
            ParsedHookEvent::PostBashCall(e) => {
                assert_eq!(expected_variant, "PostBashCall");
                assert_eq!(e.tool_use_id, "toolu_01ABC");
                assert_eq!(e.command.as_deref(), Some("cargo test"));
            }
            _ => panic!("Expected a bash call event for {event}"),
        }
    }
}

#[test]
fn test_zcode_apply_patch_parse() {
    let hook_input = zcode_hook_input(
        "PostToolUse",
        "ApplyPatch",
        json!({
            "patch": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** Update File: src/lib.rs\n@@\n-a\n+b\n*** End Patch"
        }),
        "/home/user/project",
    );
    let events = parse_zcode(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "zcode");
            assert_eq!(
                e.file_paths,
                vec![
                    PathBuf::from("/home/user/project/src/main.rs"),
                    PathBuf::from("/home/user/project/src/lib.rs")
                ]
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_zcode_ignores_unsupported_tools() {
    for tool in [
        "Read",
        "Glob",
        "Grep",
        "LS",
        "WebSearch",
        "Skill",
        "mcp__server__tool",
    ] {
        let hook_input = zcode_hook_input(
            "PostToolUse",
            tool,
            json!({"file_path": "src/main.rs"}),
            "/home/user/project",
        );
        assert!(
            parse_zcode(&hook_input).unwrap().is_empty(),
            "{tool} unexpectedly produced events"
        );
    }
}

#[test]
fn test_zcode_skips_non_tool_events() {
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "Stop",
        "PermissionRequest",
        "PostToolUseFailure",
    ] {
        let hook_input = zcode_hook_input(
            event,
            "Write",
            json!({"file_path": "src/main.rs"}),
            "/home/user/project",
        );
        assert!(
            parse_zcode(&hook_input).unwrap().is_empty(),
            "{event} unexpectedly produced events"
        );
    }
}

// ---------------------------------------------------------------------------
// End-to-end attribution tests
// ---------------------------------------------------------------------------

#[test]
fn test_zcode_file_edit_e2e_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("test.txt");

    fs::write(&file_path, "original line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("test.txt");
    file.assert_committed_lines(crate::lines!["original line".unattributed_human()]);

    let repo_dir = repo.path().to_string_lossy().to_string();

    let pre_hook_input = zcode_hook_input(
        "PreToolUse",
        "Write",
        json!({"file_path": "test.txt"}),
        &repo_dir,
    );
    repo.git_ai(&["checkpoint", "zcode", "--hook-input", &pre_hook_input])
        .unwrap();

    fs::write(&file_path, "original line\nAI added line\n").unwrap();

    let post_hook_input = zcode_hook_input(
        "PostToolUse",
        "Write",
        json!({"file_path": "test.txt"}),
        &repo_dir,
    );
    repo.git_ai(&["checkpoint", "zcode", "--hook-input", &post_hook_input])
        .unwrap();

    repo.stage_all_and_commit("AI edit").unwrap();
    file.assert_committed_lines(crate::lines![
        "original line".unattributed_human(),
        "AI added line".ai(),
    ]);
}

#[test]
fn test_zcode_bash_e2e_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("script-output.txt");

    fs::write(&file_path, "base line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("script-output.txt");
    file.assert_committed_lines(crate::lines!["base line".unattributed_human()]);

    let repo_dir = repo.canonical_path().to_string_lossy().to_string();
    let command = "printf 'created by zcode bash\\n' >> script-output.txt";

    let pre_hook_input =
        zcode_hook_input("PreToolUse", "Bash", json!({"command": command}), &repo_dir);
    repo.git_ai(&["checkpoint", "zcode", "--hook-input", &pre_hook_input])
        .unwrap();

    fs::write(&file_path, "base line\ncreated by zcode bash\n").unwrap();

    let post_hook_input = zcode_hook_input(
        "PostToolUse",
        "Bash",
        json!({"command": command}),
        &repo_dir,
    );
    repo.git_ai(&["checkpoint", "zcode", "--hook-input", &post_hook_input])
        .unwrap();

    repo.stage_all_and_commit("ZCode bash edit").unwrap();
    file.assert_committed_lines(crate::lines![
        "base line".unattributed_human(),
        "created by zcode bash".ai(),
    ]);
}

#[test]
fn test_zcode_apply_patch_e2e_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("patched.txt");

    fs::write(&file_path, "original line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("patched.txt");
    file.assert_committed_lines(crate::lines!["original line".unattributed_human()]);

    let repo_dir = repo.canonical_path().to_string_lossy().to_string();
    let patch = "*** Begin Patch\n*** Update File: patched.txt\n@@\n-original line\n+original line\nadded via apply_patch\n*** End Patch";

    let pre_hook_input = zcode_hook_input(
        "PreToolUse",
        "ApplyPatch",
        json!({"patch": patch}),
        &repo_dir,
    );
    repo.git_ai(&["checkpoint", "zcode", "--hook-input", &pre_hook_input])
        .unwrap();

    fs::write(&file_path, "original line\nadded via apply_patch\n").unwrap();

    let post_hook_input = zcode_hook_input(
        "PostToolUse",
        "ApplyPatch",
        json!({"patch": patch}),
        &repo_dir,
    );
    repo.git_ai(&["checkpoint", "zcode", "--hook-input", &post_hook_input])
        .unwrap();

    repo.stage_all_and_commit("ZCode apply patch edit").unwrap();
    file.assert_committed_lines(crate::lines![
        "original line".unattributed_human(),
        "added via apply_patch".ai(),
    ]);
}

#[test]
fn test_zcode_ignored_hook_produces_no_checkpoint_requests() {
    let hook_input = zcode_hook_input(
        "PostToolUse",
        "Read",
        json!({"file_path": "test.txt"}),
        "/home/user/project",
    );

    let requests = git_ai::commands::checkpoint_agent::orchestrator::execute_preset_checkpoint(
        "zcode",
        &hook_input,
    )
    .unwrap();
    assert!(requests.is_empty());
}
