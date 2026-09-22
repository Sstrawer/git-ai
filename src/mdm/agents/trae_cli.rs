use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{
    binary_exists, generate_diff, home_dir, is_git_ai_checkpoint_command,
    normalize_windows_path_for_shell, write_atomic,
};
use std::fs;
use std::path::{Path, PathBuf};

const TRAE_CLI_CHECKPOINT_CMD: &str = "checkpoint trae --hook-input stdin";
const TRAE_CLI_HOOK_TIMEOUT: &str = "30s";

/// The TraeCLI lifecycle events we install into (see the TraeCLI "Hooks"
/// doc). Matcher event names accept snake_case.
const HOOK_EVENTS: &[&str] = &["pre_tool_use", "post_tool_use"];

/// Indent used when rendering a fresh `hooks:` block.
const DEFAULT_ENTRY_INDENT: usize = 2;

/// Hook installer for TRAE CLI (`trae-cli`).
///
/// TraeCLI reads hooks from a YAML config: a top-level `hooks:` list in
/// `~/.trae/trae_cli.yaml` whose entries are maps with
/// `type`/`command`/`timeout`/`matchers` keys. The stdin payloads are
/// Claude-Code-style and are handled by the shared `trae` preset, so the
/// installed command reuses `checkpoint trae`.
///
/// The config is edited line-by-line rather than through a YAML
/// serializer: real trae_cli.yaml files are small flat maps, and surgical
/// line editing preserves user formatting and comments. Unsupported
/// layouts (flow-style `hooks: []`, a nested `hooks:` key) fail closed
/// with an error instead of risking file corruption.
pub struct TraeCliInstaller;

/// A parsed top-level `hooks:` block in a trae_cli.yaml file.
struct HooksBlock {
    /// Line index of the `hooks:` key.
    key_idx: usize,
    /// Index one past the last line of the block's body.
    end_idx: usize,
    /// Indent width shared by the block's sequence entries.
    entry_indent: usize,
    /// `[start, end)` line ranges of each hook entry, in file order.
    entries: Vec<(usize, usize)>,
}

impl TraeCliInstaller {
    /// TraeCLI reads user-level hooks from `~/.trae/trae_cli.yaml`
    /// (project-level `.trae/trae_cli.yaml` overrides it, but machine-wide
    /// installs target the user config).
    fn config_path() -> PathBuf {
        home_dir().join(".trae").join("trae_cli.yaml")
    }

    fn desired_command(binary_path: &Path) -> String {
        format!(
            "{} {}",
            normalize_windows_path_for_shell(binary_path),
            TRAE_CLI_CHECKPOINT_CMD
        )
    }

    /// Install hooks into a trae_cli.yaml file, returning a diff if changes
    /// were made.
    fn install_hooks_at(
        config_path: &Path,
        desired_cmd: &str,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let existing = if config_path.exists() {
            fs::read_to_string(config_path)?
        } else {
            String::new()
        };

        let new_content = merge_hooks_entry(&existing, desired_cmd)?;
        let Some(new_content) = new_content else {
            return Ok(None);
        };

        let diff_output = generate_diff(config_path, &existing, &new_content);

        if !dry_run {
            write_atomic(config_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }

    /// Remove hooks from a trae_cli.yaml file, returning a diff if changes
    /// were made.
    fn uninstall_hooks_at(config_path: &Path, dry_run: bool) -> Result<Option<String>, GitAiError> {
        if !config_path.exists() {
            return Ok(None);
        }

        let existing = fs::read_to_string(config_path)?;
        let new_content = remove_hooks_entries(&existing)?;
        let Some(new_content) = new_content else {
            return Ok(None);
        };

        let diff_output = generate_diff(config_path, &existing, &new_content);

        if !dry_run {
            write_atomic(config_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }
}

// ---------------------------------------------------------------------------
// trae_cli.yaml `hooks:` block editing
// ---------------------------------------------------------------------------

/// Split file content into lines (trailing `\r` stripped) plus whether the
/// file uses CRLF endings. The final empty element from a trailing newline
/// is preserved so the content can be rebuilt byte-identically.
fn split_lines(content: &str) -> (Vec<String>, bool) {
    let crlf = content.contains("\r\n");
    let lines = content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect();
    (lines, crlf)
}

fn join_lines(lines: &[String], crlf: bool) -> String {
    let sep = if crlf { "\r\n" } else { "\n" };
    lines.join(sep)
}

/// Find the top-level `hooks:` block in a trae_cli.yaml file.
///
/// Returns `Ok(None)` when the file has no `hooks:` key. Returns an error
/// for layouts that cannot be edited safely (flow-style values, nested or
/// duplicate `hooks:` keys).
fn parse_hooks_block(lines: &[String]) -> Result<Option<HooksBlock>, GitAiError> {
    let mut key_idx: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("hooks:") {
            let rest = rest.trim();
            if !rest.is_empty() && !rest.starts_with('#') {
                return Err(GitAiError::Generic(
                    "trae_cli.yaml uses a flow-style `hooks:` value; refusing to edit it automatically"
                        .to_string(),
                ));
            }
            if key_idx.replace(idx).is_some() {
                return Err(GitAiError::Generic(
                    "trae_cli.yaml has duplicate top-level `hooks:` keys".to_string(),
                ));
            }
        } else if line.trim_start().starts_with("hooks:") {
            return Err(GitAiError::Generic(
                "trae_cli.yaml has a nested `hooks:` key; refusing to edit it automatically"
                    .to_string(),
            ));
        }
    }
    let Some(key_idx) = key_idx else {
        return Ok(None);
    };

    // The block body extends until the next non-blank column-0 line.
    let mut end_idx = lines.len();
    for (idx, line) in lines.iter().enumerate().skip(key_idx + 1) {
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('\t') {
            end_idx = idx;
            break;
        }
    }

    // Split the body into sequence entries. Entries are lines starting
    // with `- ` at a shared indent; deeper `- ` lines belong to the
    // current entry (e.g. `matchers` items). The final entry range
    // excludes trailing blank lines so the file's trailing newline
    // marker survives edits.
    let mut entry_indent: Option<usize> = None;
    let mut entries: Vec<(usize, usize)> = Vec::new();
    let mut current_start: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate().take(end_idx).skip(key_idx + 1) {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();
        let is_entry_start = trimmed == "-" || trimmed.starts_with("- ");
        if !is_entry_start {
            continue;
        }
        match entry_indent {
            None => {
                entry_indent = Some(indent);
                current_start = Some(idx);
            }
            Some(ei) if indent == ei => {
                if let Some(start) = current_start.replace(idx) {
                    entries.push((start, idx));
                }
            }
            Some(ei) if indent > ei => {}
            Some(_) => {
                return Err(GitAiError::Generic(
                    "trae_cli.yaml `hooks:` entries have inconsistent indentation; refusing to edit it automatically"
                        .to_string(),
                ));
            }
        }
    }
    if let Some(start) = current_start {
        let mut end = end_idx;
        while end > start && lines[end - 1].trim().is_empty() {
            end -= 1;
        }
        entries.push((start, end));
    }

    Ok(Some(HooksBlock {
        key_idx,
        end_idx,
        entry_indent: entry_indent.unwrap_or(DEFAULT_ENTRY_INDENT),
        entries,
    }))
}

/// The raw value text of an entry's `command:` line, if it has one.
fn entry_command_raw(lines: &[String], entry: &(usize, usize)) -> Option<String> {
    let (start, end) = *entry;
    for line in &lines[start..end] {
        if let Some(rest) = line.trim_start().strip_prefix("command:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Whether an entry is a git-ai checkpoint command hook.
fn entry_is_git_ai(lines: &[String], entry: &(usize, usize)) -> bool {
    entry_command_raw(lines, entry)
        .map(|cmd| is_git_ai_checkpoint_command(&cmd))
        .unwrap_or(false)
}

/// The `command:` value with surrounding YAML quotes stripped.
fn unquote_yaml_value(value: &str) -> &str {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('\'') && value.ends_with('\''))
            || (value.starts_with('"') && value.ends_with('"')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Render our hook entry at the given indent.
fn render_entry(desired_cmd: &str, indent: usize) -> Vec<String> {
    let quoted = format!("'{}'", desired_cmd.replace('\'', "''"));
    let ind = " ".repeat(indent);
    let ind_body = " ".repeat(indent + 2);
    let ind_matcher = " ".repeat(indent + 4);
    let mut lines = vec![
        format!("{ind}- type: command"),
        format!("{ind_body}command: {quoted}"),
        format!("{ind_body}timeout: '{TRAE_CLI_HOOK_TIMEOUT}'"),
        format!("{ind_body}matchers:"),
    ];
    for event in HOOK_EVENTS {
        lines.push(format!("{ind_matcher}- event: {event}"));
    }
    lines
}

/// Rebuild the file with the `hooks:` block rewritten:
/// - the `replace_idx` entry is replaced by `replacement`,
/// - entries in `remove` are dropped,
/// - `replacement` is appended at the end of the block when `append` is set.
///
/// Preamble lines (comments before the first entry), blank separator lines,
/// and all lines outside the block are preserved.
fn rebuild_file(
    lines: &[String],
    block: &HooksBlock,
    remove: &[usize],
    replace_idx: Option<usize>,
    replacement: &[String],
    append: bool,
    crlf: bool,
) -> String {
    let first_entry_start = block
        .entries
        .first()
        .map(|(start, _)| *start)
        .unwrap_or(block.end_idx);
    let last_entry_end = block
        .entries
        .last()
        .map(|(_, end)| *end)
        .unwrap_or(first_entry_start);

    let mut out: Vec<String> = lines[..=block.key_idx].to_vec();
    // Preamble: non-entry lines (comments) before the first entry.
    out.extend(lines[block.key_idx + 1..first_entry_start].iter().cloned());

    for (idx, (start, end)) in block.entries.iter().enumerate() {
        if remove.contains(&idx) {
            continue;
        }
        if Some(idx) == replace_idx {
            out.extend(replacement.iter().cloned());
        } else {
            out.extend(lines[*start..*end].iter().cloned());
        }
    }
    if append {
        out.extend(replacement.iter().cloned());
    }
    // Blank lines between the last entry and the block end (including the
    // file's trailing newline marker), then all lines after the block.
    out.extend(lines[last_entry_end..block.end_idx].iter().cloned());
    out.extend(lines[block.end_idx..].iter().cloned());

    join_lines(&out, crlf)
}

/// Add or update our hook entry in trae_cli.yaml content.
///
/// Returns `Ok(None)` when the file already contains our entry in the
/// canonical form. All other content is preserved byte-for-byte.
fn merge_hooks_entry(existing: &str, desired_cmd: &str) -> Result<Option<String>, GitAiError> {
    let (lines, crlf) = split_lines(existing);
    let Some(block) = parse_hooks_block(&lines)? else {
        // No `hooks:` key yet: append a fresh block after the existing
        // content (preserving it and its trailing newline, if any).
        let mut new_content = existing.to_string();
        if !new_content.is_empty() && !new_content.ends_with('\n') {
            new_content.push('\n');
        }
        let mut block_lines = vec!["hooks:".to_string()];
        block_lines.extend(render_entry(desired_cmd, DEFAULT_ENTRY_INDENT));
        new_content.push_str(&join_lines(&block_lines, crlf));
        new_content.push_str(if crlf { "\r\n" } else { "\n" });
        return Ok(Some(new_content));
    };

    let desired_lines = render_entry(desired_cmd, block.entry_indent);
    let ours: Vec<usize> = block
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry_is_git_ai(&lines, entry))
        .map(|(idx, _)| idx)
        .collect();

    // Already installed in canonical form: nothing to do.
    if ours.len() == 1 {
        let (start, end) = block.entries[ours[0]];
        if lines[start..end] == desired_lines[..] {
            return Ok(None);
        }
    }

    // Replace the first git-ai entry with the canonical form and drop any
    // duplicates; append when the file has none yet.
    let replace_idx = ours.first().copied();
    let remove = &ours[1.min(ours.len())..];
    let new_content = rebuild_file(
        &lines,
        &block,
        remove,
        replace_idx,
        &desired_lines,
        ours.is_empty(),
        crlf,
    );

    Ok(Some(new_content))
}

/// Remove our hook entries from trae_cli.yaml content.
///
/// Returns `Ok(None)` when nothing needs removing. When the block holds
/// no other content afterwards, the `hooks:` key is removed too.
fn remove_hooks_entries(existing: &str) -> Result<Option<String>, GitAiError> {
    let (lines, crlf) = split_lines(existing);
    let Some(block) = parse_hooks_block(&lines)? else {
        return Ok(None);
    };

    let ours: Vec<usize> = block
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry_is_git_ai(&lines, entry))
        .map(|(idx, _)| idx)
        .collect();
    if ours.is_empty() {
        return Ok(None);
    }

    // Whether any non-blank content remains in the block outside the
    // removed entries (user entries, comments).
    let has_remaining_content = lines[block.key_idx + 1..block.end_idx]
        .iter()
        .enumerate()
        .any(|(offset, line)| {
            let abs = block.key_idx + 1 + offset;
            let inside_removed = block
                .entries
                .iter()
                .enumerate()
                .any(|(idx, (start, end))| ours.contains(&idx) && abs >= *start && abs < *end);
            !inside_removed && !line.trim().is_empty()
        });

    if !has_remaining_content {
        // Drop the `hooks:` key along with the emptied block, but keep the
        // blank lines that separated it from the following content.
        let last_entry_end = block
            .entries
            .last()
            .map(|(_, end)| *end)
            .unwrap_or(block.key_idx + 1);
        let mut out: Vec<String> = lines[..block.key_idx].to_vec();
        out.extend(lines[last_entry_end..].iter().cloned());
        return Ok(Some(join_lines(&out, crlf)));
    }

    Ok(Some(rebuild_file(
        &lines,
        &block,
        &ours,
        None,
        &[],
        false,
        crlf,
    )))
}

impl HookInstaller for TraeCliInstaller {
    fn name(&self) -> &str {
        "TRAE CLI"
    }

    fn id(&self) -> &str {
        "trae-cli"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["trae-cli"]
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let tool_installed = binary_exists("trae-cli");
        if !tool_installed {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let config_path = Self::config_path();
        let existing = if config_path.exists() {
            fs::read_to_string(&config_path)?
        } else {
            String::new()
        };
        let (lines, _) = split_lines(&existing);
        let Some(block) = parse_hooks_block(&lines)? else {
            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        };

        let desired_cmd = Self::desired_command(&params.binary_path);
        let mut any_installed = false;
        let mut up_to_date = false;
        for entry in &block.entries {
            if let Some(raw) = entry_command_raw(&lines, entry)
                && is_git_ai_checkpoint_command(&raw)
            {
                any_installed = true;
                if unquote_yaml_value(&raw) == desired_cmd {
                    up_to_date = true;
                }
            }
        }

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: any_installed,
            hooks_up_to_date: up_to_date,
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
        let config_path = temp_dir.path().join("trae_cli.yaml");
        (temp_dir, config_path)
    }

    fn binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    fn expected_cmd() -> String {
        TraeCliInstaller::desired_command(&binary_path())
    }

    fn expected_entry(cmd: &str) -> String {
        render_entry(cmd, DEFAULT_ENTRY_INDENT).join("\n")
    }

    fn read_config(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn command_lines(path: &Path) -> Vec<String> {
        read_config(path).lines().map(str::to_string).collect()
    }

    fn git_ai_command_lines(path: &Path) -> Vec<String> {
        command_lines(path)
            .into_iter()
            .filter(|l| l.trim_start().starts_with("command:"))
            .filter(|l| is_git_ai_checkpoint_command(l))
            .collect()
    }

    #[test]
    fn test_trae_cli_installer_id() {
        assert_eq!(TraeCliInstaller.id(), "trae-cli");
        assert_eq!(TraeCliInstaller.name(), "TRAE CLI");
    }

    #[test]
    fn s0_config_path_uses_underscore_filename() {
        // trae-cli reads `~/.trae/trae_cli.yaml` (underscore), matching the
        // file it writes at login; `trae_cli.yaml` (no underscore) is ignored.
        let path = TraeCliInstaller::config_path();
        assert_eq!(path.parent().unwrap(), home_dir().join(".trae"));
        assert_eq!(path.file_name().unwrap(), "trae_cli.yaml");
    }

    #[test]
    fn s1_fresh_install_creates_hooks_block() {
        let (_td, path) = setup_test_env();

        let diff = TraeCliInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();
        assert!(diff.is_some(), "should produce a diff");

        let content = read_config(&path);
        let expected = format!("hooks:\n{}\n", expected_entry(&expected_cmd()));
        assert_eq!(content, expected);
    }

    #[test]
    fn s2_fresh_install_preserves_existing_keys() {
        let (_td, path) = setup_test_env();
        fs::write(&path, "trae_login_base_url: https://example.com\n").unwrap();

        TraeCliInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();

        let content = read_config(&path);
        assert!(
            content.starts_with("trae_login_base_url: https://example.com\n"),
            "existing keys must be preserved"
        );
        assert!(content.contains("hooks:\n"));
        assert_eq!(git_ai_command_lines(&path).len(), 1);
    }

    #[test]
    fn s3_idempotent_already_installed() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            format!(
                "trae_login_base_url: https://example.com\nhooks:\n{}\n",
                expected_entry(&cmd)
            ),
        )
        .unwrap();

        let diff = TraeCliInstaller::install_hooks_at(&path, &cmd, false).unwrap();
        assert!(diff.is_none(), "should return None when already up-to-date");
    }

    #[test]
    fn s4_install_updates_stale_command() {
        let (_td, path) = setup_test_env();
        let stale_cmd = "/old/path/git-ai checkpoint trae --hook-input stdin";
        fs::write(
            &path,
            format!(
                "hooks:\n  - type: command\n    command: '{stale_cmd}'\n    timeout: '30s'\n    matchers:\n      - event: pre_tool_use\n      - event: post_tool_use\n"
            ),
        )
        .unwrap();

        TraeCliInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();

        let commands = git_ai_command_lines(&path);
        assert_eq!(commands.len(), 1, "expected exactly one git-ai command");
        assert!(commands[0].contains(&expected_cmd()));
    }

    #[test]
    fn s5_install_dedupes_git_ai_hooks() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        let dup_cmd = "/other/path/git-ai checkpoint trae --hook-input stdin";
        fs::write(
            &path,
            format!(
                "hooks:\n  - type: command\n    command: '{cmd}'\n    matchers:\n      - event: pre_tool_use\n  - type: command\n    command: '{dup_cmd}'\n    matchers:\n      - event: post_tool_use\n"
            ),
        )
        .unwrap();

        TraeCliInstaller::install_hooks_at(&path, &cmd, false).unwrap();

        assert_eq!(git_ai_command_lines(&path).len(), 1, "duplicate removed");
    }

    #[test]
    fn s6_install_preserves_user_hooks() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            "hooks:\n  - type: command\n    command: 'prettier --write .'\n    matchers:\n      - event: post_tool_use\n        tool: 'Write'\n  - type: http\n    url: 'https://example.com/hook'\n    matchers:\n      - event: stop\n",
        )
        .unwrap();

        let diff = TraeCliInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();
        assert!(diff.is_some());

        let content = read_config(&path);
        assert!(content.contains("command: 'prettier --write .'"));
        assert!(content.contains("url: 'https://example.com/hook'"));
        assert!(content.contains("- event: stop"));
        assert_eq!(git_ai_command_lines(&path).len(), 1);
    }

    #[test]
    fn s7_install_appends_to_existing_block() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            "hooks:\n  - type: command\n    command: 'echo hi'\n    matchers:\n      - event: stop\n",
        )
        .unwrap();

        TraeCliInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();

        let lines = command_lines(&path);
        let user_idx = lines.iter().position(|l| l.contains("echo hi")).unwrap();
        let ours_idx = lines
            .iter()
            .position(|l| is_git_ai_checkpoint_command(l))
            .unwrap();
        assert!(
            user_idx < ours_idx,
            "our entry must be appended after user entries"
        );
        assert_eq!(lines[0], "hooks:");
    }

    #[test]
    fn s8_install_matches_custom_indent() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            "hooks:\n    - type: command\n        command: 'echo hi'\n        matchers:\n            - event: stop\n",
        )
        .unwrap();

        TraeCliInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();

        let lines = command_lines(&path);
        let ours_idx = lines
            .iter()
            .position(|l| is_git_ai_checkpoint_command(l))
            .unwrap();
        // The command line of a 4-space-indented entry body sits at indent 6.
        assert!(
            lines[ours_idx].starts_with("      command:"),
            "entry must match the block's 4-space indent, got: {}",
            lines[ours_idx]
        );
    }

    #[test]
    fn s9_install_handles_crlf_files() {
        let (_td, path) = setup_test_env();
        let content = "trae_login_base_url: https://example.com\r\nhooks:\r\n  - type: command\r\n    command: 'echo hi'\r\n    matchers:\r\n      - event: stop\r\n";
        fs::write(&path, content).unwrap();

        TraeCliInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();

        let written = read_config(&path);
        assert!(written.starts_with(content), "existing content preserved");
        for line in written.split('\n') {
            assert!(
                line.is_empty() || line.ends_with('\r'),
                "no bare LF line endings introduced: {:?}",
                line
            );
        }
    }

    #[test]
    fn s10_dry_run_does_not_write() {
        let (_td, path) = setup_test_env();

        let diff = TraeCliInstaller::install_hooks_at(&path, &expected_cmd(), true).unwrap();
        assert!(diff.is_some());
        assert!(!path.exists(), "dry run must not create the file");
    }

    #[test]
    fn s11_install_flow_style_hooks_errors() {
        let (_td, path) = setup_test_env();
        fs::write(&path, "hooks: []\n").unwrap();

        let result = TraeCliInstaller::install_hooks_at(&path, &expected_cmd(), false);
        assert!(result.is_err(), "flow-style hooks must fail closed");
    }

    #[test]
    fn s12_install_nested_hooks_key_errors() {
        let (_td, path) = setup_test_env();
        fs::write(&path, "agent:\n  hooks:\n    - type: command\n").unwrap();

        let result = TraeCliInstaller::install_hooks_at(&path, &expected_cmd(), false);
        assert!(result.is_err(), "nested hooks key must fail closed");
    }

    #[test]
    fn s13_uninstall_removes_git_ai_hooks_only() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            format!(
                "hooks:\n  - type: command\n    command: 'echo user-hook'\n    matchers:\n      - event: stop\n  - type: command\n    command: '{cmd}'\n    timeout: '30s'\n    matchers:\n      - event: pre_tool_use\n      - event: post_tool_use\n"
            ),
        )
        .unwrap();

        let diff = TraeCliInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_some(), "should produce a diff");

        let content = read_config(&path);
        assert!(content.contains("echo user-hook"), "user hook preserved");
        assert!(
            !is_git_ai_checkpoint_command(&content),
            "git-ai hook removed"
        );
        assert!(content.contains("hooks:"));
    }

    #[test]
    fn s14_uninstall_removes_emptied_block() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            format!(
                "trae_login_base_url: https://example.com\nhooks:\n  - type: command\n    command: '{cmd}'\n    timeout: '30s'\n    matchers:\n      - event: pre_tool_use\n      - event: post_tool_use\n",
            ),
        )
        .unwrap();

        let diff = TraeCliInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_some());

        let content = read_config(&path);
        assert_eq!(
            content, "trae_login_base_url: https://example.com\n",
            "other keys preserved, emptied block dropped, trailing newline kept"
        );
    }

    #[test]
    fn s15_uninstall_no_git_ai_hooks_is_noop() {
        let (_td, path) = setup_test_env();
        fs::write(
            &path,
            "hooks:\n  - type: command\n    command: 'echo user-hook'\n",
        )
        .unwrap();

        let diff = TraeCliInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(
            diff.is_none(),
            "should return None when nothing to uninstall"
        );
    }

    #[test]
    fn s16_uninstall_missing_file_is_noop() {
        let (_td, path) = setup_test_env();
        let diff = TraeCliInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_none());
    }

    #[test]
    fn s17_uninstall_keeps_block_with_only_comments_left() {
        let (_td, path) = setup_test_env();
        let cmd = expected_cmd();
        fs::write(
            &path,
            format!(
                "hooks:\n  # my hooks\n  - type: command\n    command: '{cmd}'\n    matchers:\n      - event: pre_tool_use\n",
            ),
        )
        .unwrap();

        let diff = TraeCliInstaller::uninstall_hooks_at(&path, false).unwrap();
        assert!(diff.is_some());

        let content = read_config(&path);
        assert!(content.contains("# my hooks"), "comment preserved");
        assert!(
            content.contains("hooks:"),
            "hooks key kept for remaining comment"
        );
        assert!(!is_git_ai_checkpoint_command(&content));
    }

    #[test]
    fn s18_install_without_trailing_newline() {
        let (_td, path) = setup_test_env();
        fs::write(&path, "trae_login_base_url: https://example.com").unwrap();

        TraeCliInstaller::install_hooks_at(&path, &expected_cmd(), false).unwrap();

        let content = read_config(&path);
        assert!(
            content.starts_with("trae_login_base_url: https://example.com\nhooks:\n"),
            "newline inserted before the appended block"
        );
    }
}
