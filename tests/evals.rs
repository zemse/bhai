//! `evals/run.py` against the checked-in tasks, with the reference solutions and with fake
//! `bhai` and `codex` scripts in place of the agents, so no model is called.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

struct Scratch {
    dir: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn scratch() -> Option<Scratch> {
    let found = Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        eprintln!("skipped: python3 is not available");
        return None;
    }
    let dir = std::env::temp_dir().join(format!("bhai-evals-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    Some(Scratch { dir })
}

/// An executable shell script at `dir/name`.
fn fake(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/usr/bin/env bash\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

/// Run the runner with `args`, returning its `results.jsonl` lines.
fn eval(scratch: &Scratch, args: &[&str]) -> Vec<Value> {
    let out = scratch.dir.join("out");
    let status = Command::new("python3")
        .arg(Path::new(ROOT).join("evals/run.py"))
        .args(args)
        .arg("--out")
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "run.py exited {status}");
    std::fs::read_to_string(out.join("results.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn every_task_passes_with_its_reference_solution() {
    let Some(s) = scratch() else { return };
    let tasks = std::fs::read_dir(Path::new(ROOT).join("evals/tasks"))
        .unwrap()
        .count();
    let results = eval(&s, &["--agent", "oracle"]);
    assert_eq!(results.len(), tasks);
    for r in &results {
        assert_eq!(r["reward"], 1.0, "{r}");
        assert_eq!(r["exit"], 0, "{r}");
    }
}

#[test]
fn bhai_gets_the_instruction_on_stdin_and_its_usage_is_summed() {
    let Some(s) = scratch() else { return };
    // Echoes what it was given to stderr, solves nothing, and reports two model calls and
    // a child's.
    let bhai = fake(
        &s.dir,
        "bhai",
        r#"echo "args: $*" >&2
echo "stdin: $(cat)" >&2
ls -A >&2
echo '{"type":"text","data":"done"}'
echo '{"type":"usage","data":{"input":100,"cached":40,"output":10,"reasoning":5}}'
echo 'not json'
echo '{"type":"usage","data":{"input":200,"cached":150,"output":20,"reasoning":0}}'
echo '{"type":"child_usage","data":{"input":7,"cached":0,"output":3,"reasoning":1}}'
echo '{"type":"turn_end"}'
"#,
    );
    let results = eval(
        &s,
        &[
            "--bhai",
            &bhai,
            "--bhai-model",
            "ollama:tiny",
            "--bhai-arg=--no-global",
            "fix-off-by-one",
        ],
    );
    let [r] = &results[..] else {
        panic!("{results:?}")
    };
    assert_eq!(r["agent"], "bhai");
    assert_eq!(r["model"], "ollama:tiny");
    assert_eq!(r["reward"], 0.0);
    assert_eq!(r["timed_out"], false);
    assert_eq!(
        r["tokens"],
        serde_json::json!({"input": 307, "cached": 190, "output": 33, "reasoning": 6})
    );
    let log = r["log"].as_str().unwrap();
    let stderr = std::fs::read_to_string(log.replace(".jsonl", ".stderr")).unwrap();
    assert!(
        stderr.contains("args: exec - --json --mode auto --model ollama:tiny --no-global"),
        "{stderr}"
    );
    assert!(
        stderr.contains("stdin: `total.py` has a function"),
        "{stderr}"
    );
    // The workspace holds the environment and not the hidden tests.
    assert!(
        stderr.contains("total.py") && !stderr.contains("tests"),
        "{stderr}"
    );
}

#[test]
fn codex_runs_in_the_workspace_and_its_turn_usage_is_summed() {
    let Some(s) = scratch() else { return };
    // Solves the task in the directory it was told to use, so a reward of 1 shows `-C`
    // pointed at the workspace the verifier checks.
    let codex = fake(
        &s.dir,
        "codex",
        r#"while [ "$1" != "-C" ]; do shift; done
cd "$2"
cat >/dev/null
echo 'Hello, world!' > hello.txt
echo '{"type":"thread.started","thread_id":"t"}'
echo '{"type":"turn.completed","usage":{"input_tokens":50,"cached_input_tokens":20,"output_tokens":8,"reasoning_output_tokens":2}}'
echo '{"type":"turn.completed","usage":{"input_tokens":5,"cached_input_tokens":0,"output_tokens":1}}'
"#,
    );
    let results = eval(
        &s,
        &[
            "--agent",
            "codex",
            "--codex",
            &codex,
            "--trials",
            "2",
            "write-greeting",
        ],
    );
    assert_eq!(results.len(), 2);
    for (n, r) in results.iter().enumerate() {
        assert_eq!(r["trial"], n + 1);
        assert_eq!(r["reward"], 1.0, "{r}");
        assert_eq!(
            r["tokens"],
            serde_json::json!({"input": 55, "cached": 20, "output": 9, "reasoning": 2})
        );
    }
}

#[test]
fn an_agent_past_its_timeout_is_killed_and_scores_zero() {
    let Some(s) = scratch() else { return };
    // The hung command sits in its own process group, as bhai's bash tool puts it, so a
    // kill of the agent's group alone leaves it running.
    let pid_file = s.dir.join("command.pid");
    let bhai = fake(
        &s.dir,
        "bhai",
        &format!(
            "echo 'Hello, world!' > hello.txt\n\
python3 -c 'import os, time; os.setpgid(0, 0); open(\"{}\", \"w\").write(str(os.getpid())); time.sleep(30)' &\n\
wait\n",
            pid_file.display()
        ),
    );
    let start = std::time::Instant::now();
    let results = eval(
        &s,
        &["--bhai", &bhai, "--agent-timeout", "1", "write-greeting"],
    );
    assert!(start.elapsed().as_secs() < 20);
    let [r] = &results[..] else {
        panic!("{results:?}")
    };
    assert_eq!(r["timed_out"], true);
    // A timed out agent gets no credit for work it left on disk.
    assert_eq!(r["reward"], 0.0);
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    let alive = || {
        Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    // Reaping the orphan is init's job, so give it a moment.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while alive() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if alive() {
        let _ = Command::new("kill").args(["-9", pid.trim()]).status();
        panic!("the command {} outlived the trial", pid.trim());
    }
}
