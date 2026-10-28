//! Command-line help and input validation never need credentials or a model server.

use std::process::{Command, Output, Stdio};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bhai"))
        .args(args)
        .env("BHAI_MODE", "bypass")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn help_and_version_exit_successfully_without_starting_a_session() {
    for args in [
        vec!["--help"],
        vec!["--tart", "--help"],
        vec!["exec", "--help"],
        vec!["sessions", "--help"],
        vec!["sessions", "prune", "--help"],
        vec!["mcp", "login", "--help"],
    ] {
        let output = run(&args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
        assert!(output.stderr.is_empty());
    }
    let output = run(&["--version"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("bhai {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn invalid_arguments_exit_two_with_errors_only_on_stderr() {
    for args in [
        vec!["--headless"],
        vec!["--json"],
        vec!["exec"],
        vec!["exec", "review", "--mode", "invalid"],
        vec!["sessions", "prune", "nope"],
        vec!["mcp", "login"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("error:"), "{error}");
        assert!(error.contains("--help"), "{error}");
    }
}

#[test]
fn tart_failure_never_falls_back_to_host_session_setup() {
    let dir = std::env::temp_dir().join(format!("bhai-tart-cli-{}", uuid::Uuid::new_v4()));
    let home = dir.join("home");
    let project = dir.join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(project.join(".bhai")).unwrap();
    std::fs::write(project.join(".bhai/config.toml"), "invalid toml [").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bhai"))
        .args(["--tart", "-C", "/missing/guest-only/path", "exec", "-"])
        .env("HOME", &home)
        .current_dir(&project)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    #[cfg(target_os = "macos")]
    assert!(error.contains("set [tart] vm"), "{error}");
    #[cfg(not(target_os = "macos"))]
    assert!(error.contains("--tart requires macOS"), "{error}");
    assert!(!project.join(".bhai/sessions").exists());
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn empty_stdin_fails_before_configuration_or_model_preflight() {
    for args in [vec!["exec", "-"], vec!["--print"]] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("the prompt must not be empty"));
    }
}
