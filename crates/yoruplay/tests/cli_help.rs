//! The root and group commands must render the termtree command tree, while
//! leaf commands keep clap's detailed argument help.

use std::process::Command;

#[test]
fn root_and_group_commands_render_trees() {
    let exe = env!("CARGO_BIN_EXE_yoruplay");

    let output = Command::new(exe).output().expect("run yoruplay");
    assert!(output.status.success());
    let root = String::from_utf8(output.stdout).unwrap();
    assert!(root.contains("yoruplay\n├──"), "{root}");
    assert!(root.contains("daemon"), "{root}");
    assert!(root.contains("control"), "{root}");
    assert!(!root.contains("Usage:"), "{root}");

    for group in ["daemon", "control", "queue", "library", "auth", "cache"] {
        let output = Command::new(exe)
            .arg(group)
            .output()
            .unwrap_or_else(|e| panic!("run {group}: {e}"));
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains(&format!("yoruplay {group}")), "{text}");
        assert!(!text.contains("Usage:"), "{text}");
    }
}

#[test]
fn leaf_help_still_shows_arguments() {
    let exe = env!("CARGO_BIN_EXE_yoruplay");
    let output = Command::new(exe)
        .args(["daemon", "start", "--help"])
        .output()
        .expect("run leaf help");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Start a detached daemon"), "{text}");
    assert!(text.contains("Usage: yoruplay daemon start"), "{text}");
}
