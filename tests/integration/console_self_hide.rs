//! Startup console self-hide behavior (Windows only).
//!
//! When a console-less parent (e.g. OpenCode's Bun runtime spawning `git`
//! without `CREATE_NO_WINDOW`/`windowsHide`) launches the git-ai binary, the
//! OS allocates a fresh console whose window pops up for the whole command
//! duration, titled with the executable path. The binary hides that window at
//! startup when it is the sole occupant of the console and stdin is not an
//! interactive terminal. These tests lock in that behavior via the
//! `GIT_AI_TEST_CONSOLE_REPORT` stderr report emitted by the binary.

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
use std::process::{Command, Stdio};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[cfg(windows)]
const CREATE_NEW_CONSOLE: u32 = 0x00000010;

#[cfg(windows)]
fn console_report(stderr_or_stdout: &str) -> (bool, u32, bool, bool) {
    let re = regex::Regex::new(
        r"console self-hide: window=(\d) attached=(\d+) stdin_terminal=(\d) hidden=(\d)",
    )
    .unwrap();
    let caps = re.captures(stderr_or_stdout).unwrap_or_else(|| {
        panic!("console self-hide report line not found in:\n{stderr_or_stdout}")
    });
    (
        &caps[1] == "1",
        caps[2].parse().unwrap(),
        &caps[3] == "1",
        &caps[4] == "1",
    )
}

/// Sole occupant of a freshly allocated *windowless* console (direct
/// `CREATE_NO_WINDOW` spawn from a piped parent): nothing to hide, and the
/// report must reflect that.
#[cfg(windows)]
#[test]
fn sole_occupant_of_windowless_console_is_not_hidden() {
    let out = Command::new(crate::repos::test_repo::get_binary_path())
        .arg("version")
        .env("GIT_AI_TEST_CONSOLE_REPORT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("spawn git-ai version");
    let (has_window, attached, stdin_terminal, hidden) =
        console_report(&String::from_utf8_lossy(&out.stderr));
    assert!(!has_window, "CREATE_NO_WINDOW consoles have no window");
    assert_eq!(
        attached, 1,
        "child is the sole process on its fresh console"
    );
    assert!(!stdin_terminal, "stdin is a null/pipe handle");
    assert!(!hidden, "no window means nothing to hide");
}

/// Console shared with a spawning shell (PowerShell wrapper that owns the
/// console): the window must never be hidden, regardless of stdin.
#[cfg(windows)]
#[test]
fn console_shared_with_shell_is_never_hidden() {
    let script = format!(
        "& '{}' version",
        crate::repos::test_repo::get_binary_path().display()
    );
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &script])
        .env("GIT_AI_TEST_CONSOLE_REPORT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("spawn powershell wrapper");
    let (_, attached, _, hidden) = console_report(&String::from_utf8_lossy(&out.stderr));
    assert!(
        attached >= 2,
        "console is shared with powershell, got attached={attached}"
    );
    assert!(!hidden, "shared consoles must never be hidden");
}

/// The OpenCode/Bun popup scenario, simulated directly: `CREATE_NEW_CONSOLE`
/// reproduces exactly what the OS does when a console-less parent launches the
/// binary without window suppression -- a fresh console, owned solely by
/// git-ai, whose window would pop up titled with the executable path. The
/// startup self-hide must hide it. On headless CI without a visible window
/// station there is no window to hide, so we assert the invariant
/// `hidden == has_window`.
#[cfg(windows)]
#[test]
fn sole_occupant_console_window_from_console_less_parent_is_hidden() {
    let out = Command::new(crate::repos::test_repo::get_binary_path())
        .arg("version")
        .env("GIT_AI_TEST_CONSOLE_REPORT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NEW_CONSOLE)
        .output()
        .expect("spawn git-ai with a fresh console");
    let (has_window, attached, stdin_terminal, hidden) =
        console_report(&String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        attached, 1,
        "git-ai must be the sole occupant of the fresh console"
    );
    assert!(!stdin_terminal, "stdin is a null/pipe handle");
    assert_eq!(
        hidden, has_window,
        "whenever a window exists it must be hidden (has_window={has_window})"
    );
}
