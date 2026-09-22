use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{
    binary_exists, generate_diff, home_dir, is_git_ai_checkpoint_command,
    normalize_windows_path_for_shell, write_atomic,
};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const ZCODE_CHECKPOINT_CMD: &str = "checkpoint zcode --hook-input stdin";
const ZCODE_CATCH_ALL_MATCHER: &str = "*";

/// The ZCode hook events we install into (see ZCode "Hooks" doc).
const HOOK_EVENTS: &[&str] = &["PreToolUse", "PostToolUse"];

/// Hook installer for ZCode.
///
/// ZCode reads hook configuration from `~/.zcode/cli/config.json`, a JSON
/// file with a nested `hooks.events.{EventName}` structure (see ZCode
/// "Hooks" doc). Each event maps to an array of matcher blocks, each
/// containing a `matcher` string and a `hooks` array of hook entries.
/// Hook entries use `type: "command"` with a shell command string.
///
/// The config must also set `hooks.enabled: true` for hooks to execute.
pub struct ZCodeInstaller;

impl ZCodeInstaller {
    fn config_path() -> PathBuf {
        home_dir().join(".zcode").join("cli").join("config.json")
    }

    fn desired_command(binary_path: &Path) -> String {
        format!(
            "{} {}",
            normalize_windows_path_for_shell(binary_path),
            ZCODE_CHECKPOINT_CMD
        )
    }

    /// Install hooks into a config.json file, returning a diff if changes
    /// were made.
    fn install_hooks_at(
        config_path: &Path,
        desired_cmd: &str,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        if let Some(dir) = config_path.parent() {
            fs::create_dir_all(dir)?;
        }

        let existing_content = if config_path.exists() {
            fs::read_to_string(config_path)?
        } else {
            String::new()
        };

        let existing: Value = if existing_content.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&existing_content)?
        };

        let mut merged = existing.clone();

        // Ensure hooks.enabled is true.
        let hooks_obj = merged
            .as_object_mut()
            .map(|root| root.entry("hooks").or_insert(json!({})).clone())
            .unwrap_or_else(|| json!({}));

        let mut hooks_obj = hooks_obj;

        if let Some(obj) = hooks_obj.as_object_mut() {
            obj.insert("enabled".to_string(), json!(true));
        }

        // Navigate to hooks.events.{EventName} and ensure our hook is
        // present in the catch-all matcher block.
        let events_obj = hooks_obj
            .get("events")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let mut events_obj = events_obj;

        for hook_type in HOOK_EVENTS {
            let mut type_array = events_obj
                .get(*hook_type)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            // Find or create the "*" catch-all matcher block.
            let catch_all_idx = type_array
                .iter()
                .position(|b| {
                    b.get("matcher")
                        .and_then(|m| m.as_str())
                        .map(|m| m == ZCODE_CATCH_ALL_MATCHER)
                        .unwrap_or(false)
                })
                .unwrap_or_else(|| {
                    type_array.push(json!({
                        "matcher": ZCODE_CATCH_ALL_MATCHER,
                        "hooks": []
                    }));
                    type_array.len() - 1
                });

            // Ensure exactly one git-ai command in the catch-all block.
            let mut hooks_array = type_array[catch_all_idx]
                .get("hooks")
                .and_then(|h| h.as_array())
                .cloned()
                .unwrap_or_default();

            let mut found_idx: Option<usize> = None;
            let mut needs_update = false;

            for (idx, hook) in hooks_array.iter().enumerate() {
                if let Some(cmd) = hook.get("command").and_then(|c| c.as_str())
                    && is_git_ai_checkpoint_command(cmd)
                    && found_idx.is_none()
                {
                    found_idx = Some(idx);
                    if cmd != desired_cmd {
                        needs_update = true;
                    }
                }
            }

            match found_idx {
                Some(idx) => {
                    if needs_update {
                        hooks_array[idx] = json!({
                            "type": "command",
                            "command": desired_cmd,
                            "timeoutMs": 30000
                        });
                    }
                    // Remove duplicates: keep the first, drop any subsequent
                    // git-ai entries.
                    let keep_idx = idx;
                    let mut current_idx = 0;
                    hooks_array.retain(|hook| {
                        if current_idx == keep_idx {
                            current_idx += 1;
                            true
                        } else if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                            let is_dup = is_git_ai_checkpoint_command(cmd);
                            current_idx += 1;
                            !is_dup
                        } else {
                            current_idx += 1;
                            true
                        }
                    });
                }
                None => {
                    hooks_array.push(json!({
                        "type": "command",
                        "command": desired_cmd,
                        "timeoutMs": 30000
                    }));
                }
            }

            if let Some(matcher_block) = type_array[catch_all_idx].as_object_mut() {
                matcher_block.insert("hooks".to_string(), Value::Array(hooks_array));
            }

            if let Some(obj) = events_obj.as_object_mut() {
                obj.insert(hook_type.to_string(), Value::Array(type_array));
            }
        }

        if let Some(obj) = hooks_obj.as_object_mut() {
            obj.insert("events".to_string(), events_obj);
        }

        if let Some(root) = merged.as_object_mut() {
            root.insert("hooks".to_string(), hooks_obj);
        }

        if existing == merged {
            return Ok(None);
        }

        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(config_path, &existing_content, &new_content);

        if !dry_run {
            write_atomic(config_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }

    /// Remove hooks from a config.json file, returning a diff if changes
    /// were made.
    fn uninstall_hooks_at(config_path: &Path, dry_run: bool) -> Result<Option<String>, GitAiError> {
        if !config_path.exists() {
            return Ok(None);
        }

        let existing_content = fs::read_to_string(config_path)?;
        let existing: Value = serde_json::from_str(&existing_content)?;

        let mut merged = existing.clone();
        let mut hooks_obj = match merged.get("hooks").cloned() {
            Some(h) => h,
            None => return Ok(None),
        };

        let mut events_obj = match hooks_obj.get("events").cloned() {
            Some(e) => e,
            None => return Ok(None),
        };

        let mut changed = false;

        for hook_type in HOOK_EVENTS {
            if let Some(type_array) = events_obj
                .get_mut(*hook_type)
                .and_then(|v| v.as_array_mut())
            {
                let mut emptied_block_indices = Vec::new();
                for (block_idx, matcher_block) in type_array.iter_mut().enumerate() {
                    if let Some(hooks_array) = matcher_block
                        .get_mut("hooks")
                        .and_then(|h| h.as_array_mut())
                    {
                        let original_len = hooks_array.len();
                        hooks_array.retain(|hook| {
                            if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                                !is_git_ai_checkpoint_command(cmd)
                            } else {
                                true
                            }
                        });
                        if hooks_array.len() != original_len {
                            changed = true;
                            if hooks_array.is_empty() {
                                emptied_block_indices.push(block_idx);
                            }
                        }
                    }
                }
                for block_idx in emptied_block_indices.into_iter().rev() {
                    type_array.remove(block_idx);
                }
            }
        }

        if !changed {
            return Ok(None);
        }

        if let Some(obj) = hooks_obj.as_object_mut() {
            obj.insert("events".to_string(), events_obj);
        }
        if let Some(root) = merged.as_object_mut() {
            root.insert("hooks".to_string(), hooks_obj);
        }

        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(config_path, &existing_content, &new_content);

        if !dry_run {
            write_atomic(config_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }
}

impl HookInstaller for ZCodeInstaller {
    fn name(&self) -> &str {
        "ZCode"
    }

    fn id(&self) -> &str {
        "zcode"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["zcode"]
    }

    fn check_hooks(&self, _params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let config_path = Self::config_path();

        if !config_path.exists() {
            // Tool may still be installed but without config.
            let tool_installed = binary_exists("zcode") || home_dir().join(".zcode").exists();
            return Ok(HookCheckResult {
                tool_installed,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let content = fs::read_to_string(&config_path)?;
        let existing: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));

        let has_hooks = HOOK_EVENTS.iter().all(|event| {
            existing
                .get("hooks")
                .and_then(|h| h.get("events"))
                .and_then(|e| e.get(*event))
                .and_then(|v| v.as_array())
                .map(|blocks| {
                    blocks.iter().any(|block| {
                        block
                            .get("hooks")
                            .and_then(|h| h.as_array())
                            .map(|hooks| {
                                hooks.iter().any(|hook| {
                                    hook.get("command")
                                        .and_then(|c| c.as_str())
                                        .map(is_git_ai_checkpoint_command)
                                        .unwrap_or(false)
                                })
                            })
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        });

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: has_hooks,
            hooks_up_to_date: has_hooks,
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let desired_cmd = Self::desired_command(&params.binary_path);
        Self::install_hooks_at(&Self::config_path(), &desired_cmd, dry_run)
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        Self::uninstall_hooks_at(&Self::config_path(), dry_run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_test_env() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        (temp_dir, config_path)
    }

    fn binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    fn expected_cmd() -> String {
        ZCodeInstaller::desired_command(&binary_path())
    }

    fn read_config(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn hooks_in_catch_all<'a>(hooks: &'a Value, event: &str) -> Vec<&'a Value> {
        let Some(blocks) = hooks
            .get("hooks")
            .and_then(|h| h.get("events"))
            .and_then(|e| e.get(event))
            .and_then(|v| v.as_array())
        else {
            return Vec::new();
        };
        blocks
            .iter()
            .find(|b| {
                b.get("matcher")
                    .and_then(|m| m.as_str())
                    .map(|m| m == ZCODE_CATCH_ALL_MATCHER)
                    .unwrap_or(false)
            })
            .and_then(|b| b.get("hooks").and_then(|h| h.as_array()))
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    #[test]
    fn test_zcode_installer_id() {
        assert_eq!(ZCodeInstaller.id(), "zcode");
        assert_eq!(ZCodeInstaller.name(), "ZCode");
    }

    #[test]
    fn s1_fresh_install_creates_hooks_block() {
        let (_td, path) = setup_test_env();

        let diff = ZCodeInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();
        assert!(diff.is_some(), "should produce a diff");

        let config = read_config(&path);
        assert_eq!(
            config.get("hooks").and_then(|h| h.get("enabled")),
            Some(&json!(true))
        );
        for event in HOOK_EVENTS {
            let catch_all = hooks_in_catch_all(&config, event);
            assert_eq!(catch_all.len(), 1, "{event}: expected 1 hook in catch-all");
            assert_eq!(
                catch_all[0]
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap(),
                expected_cmd()
            );
            assert_eq!(
                catch_all[0].get("type").and_then(|t| t.as_str()).unwrap(),
                "command"
            );
        }
    }

    #[test]
    fn s2_idempotent_already_installed() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": cmd, "timeoutMs": 30000}]}],
                        "PostToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": cmd, "timeoutMs": 30000}]}]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = ZCodeInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();
        assert!(diff.is_none(), "should return None when already up-to-date");
    }

    #[test]
    fn s3_install_preserves_user_hooks_and_other_matchers() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "PreToolUse": [{
                            "matcher": "Write|Edit",
                            "hooks": [{"type": "command", "command": "echo before"}]
                        }],
                        "PostToolUse": [{
                            "matcher": "*",
                            "hooks": [{"type": "command", "command": "prettier --write"}]
                        }]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ZCodeInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();

        let config = read_config(&path);

        // PreToolUse: user matcher block untouched, catch-all created.
        let pre_blocks = config
            .get("hooks")
            .and_then(|h| h.get("events"))
            .and_then(|e| e.get("PreToolUse"))
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(pre_blocks.len(), 2, "user block + catch-all block");

        // PostToolUse: user hook in catch-all preserved alongside ours.
        let post_catch_all = hooks_in_catch_all(&config, "PostToolUse");
        assert_eq!(post_catch_all.len(), 2, "user hook + git-ai hook");
        assert!(
            post_catch_all
                .iter()
                .any(|h| h.get("command").and_then(|c| c.as_str()).unwrap_or("")
                    == "prettier --write")
        );
        assert!(
            post_catch_all
                .iter()
                .any(|h| h.get("command").and_then(|c| c.as_str()).unwrap_or("") == cmd)
        );
    }

    #[test]
    fn s4_install_updates_stale_command() {
        let (_td, path) = setup_test_env();
        let stale_cmd = "/old/path/git-ai checkpoint zcode --hook-input stdin";
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": stale_cmd}]}],
                        "PostToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": stale_cmd}]}]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ZCodeInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();

        let config = read_config(&path);
        for event in HOOK_EVENTS {
            let catch_all = hooks_in_catch_all(&config, event);
            assert_eq!(
                catch_all.len(),
                1,
                "{event}: expected exactly one git-ai hook"
            );
            assert_eq!(
                catch_all[0]
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap(),
                expected_cmd()
            );
        }
    }

    #[test]
    fn s5_install_dedupes_git_ai_hooks() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        let dup_cmd = "/other/path/git-ai checkpoint zcode --hook-input stdin";
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "PreToolUse": [{"matcher": "*", "hooks": [
                            {"type": "command", "command": cmd},
                            {"type": "command", "command": dup_cmd}
                        ]}],
                        "PostToolUse": [{"matcher": "*", "hooks": [
                            {"type": "command", "command": cmd}
                        ]}]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ZCodeInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();

        let config = read_config(&path);
        let pre_catch_all = hooks_in_catch_all(&config, "PreToolUse");
        assert_eq!(pre_catch_all.len(), 1, "duplicate git-ai hook removed");
    }

    #[test]
    fn s6_install_sets_enabled_true() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "enabled": false,
                    "events": {}
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ZCodeInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();

        let config = read_config(&path);
        assert_eq!(
            config.get("hooks").and_then(|h| h.get("enabled")),
            Some(&json!(true)),
            "hooks.enabled must be set to true"
        );
    }

    #[test]
    fn s7_dry_run_does_not_write() {
        let (_td, path) = setup_test_env();

        let diff = ZCodeInstaller::install_hooks_at(&path, &expected_cmd(), true).unwrap();
        assert!(diff.is_some());
        assert!(!path.exists(), "dry run must not create the file");
    }

    #[test]
    fn s8_uninstall_removes_git_ai_hooks_only() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "PreToolUse": [{"matcher": "*", "hooks": [
                            {"type": "command", "command": "echo user-hook"},
                            {"type": "command", "command": cmd}
                        ]}],
                        "PostToolUse": [{"matcher": "*", "hooks": [
                            {"type": "command", "command": cmd}
                        ]}]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = ZCodeInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_some(), "should produce a diff");

        let config = read_config(&path);
        let pre_catch_all = hooks_in_catch_all(&config, "PreToolUse");
        assert_eq!(pre_catch_all.len(), 1, "user hook preserved");
        assert_eq!(
            pre_catch_all[0]
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap(),
            "echo user-hook"
        );

        // PostToolUse catch-all block only held our hook; it must be removed.
        let post_blocks = config
            .get("hooks")
            .and_then(|h| h.get("events"))
            .and_then(|e| e.get("PostToolUse"))
            .and_then(|v| v.as_array())
            .unwrap();
        assert!(
            post_blocks.is_empty(),
            "emptied matcher block should be removed"
        );
    }

    #[test]
    fn s9_uninstall_no_hooks_is_noop() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "PreToolUse": [{"matcher": "*", "hooks": [
                            {"type": "command", "command": "echo user-hook"}
                        ]}]
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let diff = ZCodeInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(
            diff.is_none(),
            "should return None when nothing to uninstall"
        );
    }

    #[test]
    fn s10_uninstall_missing_file_is_noop() {
        let (_td, path) = setup_test_env();
        let diff = ZCodeInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_none());
    }
}
