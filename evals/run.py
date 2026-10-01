#!/usr/bin/env python3
"""Run the eval tasks against bhai and `codex exec`, and record reward, wall time and tokens.

A task is a directory in nanocodex's (Harbor's) shape:

    tasks/<name>/instruction.md     the prompt, given to the agent on stdin
    tasks/<name>/task.toml          [agent] and [verifier] timeout_sec
    tasks/<name>/environment/       optional, copied in as the starting workspace
    tasks/<name>/tests/test.sh      hidden from the agent; writes 1 or 0 to $REWARD_FILE
    tasks/<name>/solution/solve.sh  the reference answer, run by `--agent oracle`

Each trial copies `environment/` into a fresh temp dir, runs the agent there, then runs
`tests/test.sh` from a copy outside the workspace with the workspace as its cwd. An agent
that times out, no reward file, or a verifier that times out is a reward of 0.

The agent runs on this machine with no sandbox: `bhai exec --mode auto` and codex's
`workspace-write` sandbox are the only fences. Live runs spend quota, so smoke-test with
`--bhai-model ollama:<name>` and one trial.

    evals/run.py --agent oracle                    # checks the verifiers, no model call
    evals/run.py --agent bhai --bhai-model ollama:qwen3 write-greeting
    evals/run.py --agent bhai --agent codex --trials 3

Results go to `evals/results/<utc stamp>/`: `results.jsonl` (one line per trial) and each
trial's stdout and stderr.
"""

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

try:
    import tomllib
except ImportError:  # python < 3.11
    tomllib = None

HERE = Path(__file__).resolve().parent
AGENTS = ("bhai", "codex", "oracle")
DEFAULT_AGENT_TIMEOUT = 600
DEFAULT_VERIFIER_TIMEOUT = 120


def load_task(path):
    task = {"name": path.name, "path": path}
    instruction = path / "instruction.md"
    test = path / "tests" / "test.sh"
    if not instruction.is_file() or not test.is_file():
        raise SystemExit(f"{path}: a task needs instruction.md and tests/test.sh")
    task["instruction"] = instruction.read_text()
    config = {}
    if (path / "task.toml").is_file():
        if tomllib is None:
            print(f"{path.name}: no tomllib, so task.toml is ignored", file=sys.stderr)
        else:
            config = tomllib.loads((path / "task.toml").read_text())
    task["agent_timeout"] = config.get("agent", {}).get("timeout_sec", DEFAULT_AGENT_TIMEOUT)
    task["verifier_timeout"] = config.get("verifier", {}).get(
        "timeout_sec", DEFAULT_VERIFIER_TIMEOUT
    )
    return task


def run(cmd, cwd, timeout, stdin=None, stdout=None, stderr=None, env=None):
    """Run `cmd` in its own process group, killing the whole group on timeout. Returns
    (exit code, timed out)."""
    proc = subprocess.Popen(
        cmd,
        cwd=cwd,
        stdin=subprocess.PIPE if stdin is not None else subprocess.DEVNULL,
        stdout=stdout,
        stderr=stderr,
        env=env,
        start_new_session=True,
        text=True,
    )
    try:
        proc.communicate(stdin, timeout=timeout)
        return proc.returncode, False
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()
        return proc.returncode, True


def agent_command(args, agent, task, work):
    if agent == "oracle":
        return ["bash", str(task["path"] / "solution" / "solve.sh")]
    if agent == "bhai":
        cmd = [args.bhai, "exec", "-", "--json", "--mode", "auto"]
        if args.bhai_model:
            cmd += ["--model", args.bhai_model]
        return cmd + args.bhai_arg
    cmd = [args.codex, "exec", "--json", "--skip-git-repo-check", "--ephemeral"]
    cmd += ["--sandbox", "workspace-write", "-C", str(work)]
    if args.codex_model:
        cmd += ["--model", args.codex_model]
    return cmd + args.codex_arg + ["-"]


def tokens(agent, log):
    """Sum the token counts in an agent's JSONL output. bhai's `usage` and `child_usage`
    events carry `input`, `cached`, `output` and `reasoning`; codex's `turn.completed`
    carries `input_tokens`, `cached_input_tokens`, `output_tokens` and
    `reasoning_output_tokens`. Both count cached tokens inside input."""
    total = {"input": 0, "cached": 0, "output": 0, "reasoning": 0}
    for line in log.splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if not isinstance(event, dict):
            continue
        usage = None
        if agent == "bhai" and event.get("type") in ("usage", "child_usage"):
            usage = event.get("data") or {}
        elif agent == "codex" and event.get("type") == "turn.completed":
            u = event.get("usage") or {}
            usage = {
                "input": u.get("input_tokens", 0),
                "cached": u.get("cached_input_tokens", 0),
                "output": u.get("output_tokens", 0),
                "reasoning": u.get("reasoning_output_tokens", 0),
            }
        if usage:
            for key in total:
                total[key] += int(usage.get(key) or 0)
    return total


def verify(task, root, work, log_path):
    """Run the task's verifier on `work`. Returns (reward, timed out)."""
    # The tests are copied in only now, outside the workspace, so the agent never saw them.
    tests = root / "tests"
    shutil.copytree(task["path"] / "tests", tests)
    reward_file = root / "reward.txt"
    env = dict(os.environ, REWARD_FILE=str(reward_file), TESTS_DIR=str(tests))
    with open(log_path, "w") as log:
        _, timed_out = run(
            ["bash", str(tests / "test.sh")],
            work,
            task["verifier_timeout"],
            stdout=log,
            stderr=subprocess.STDOUT,
            env=env,
        )
    if timed_out:
        return 0.0, True
    try:
        return float(reward_file.read_text().strip()), False
    except (OSError, ValueError):
        return 0.0, False


def trial(args, agent, task, n, out):
    root = Path(tempfile.mkdtemp(prefix=f"bhai-eval-{task['name']}-"))
    work = root / "work"
    environment = task["path"] / "environment"
    if environment.is_dir():
        shutil.copytree(environment, work)
    else:
        work.mkdir()
    stem = f"{task['name']}.{agent}.{n}"
    log_path = out / f"{stem}.jsonl"
    timeout = args.agent_timeout or task["agent_timeout"]
    start = time.monotonic()
    with open(log_path, "w") as stdout, open(out / f"{stem}.stderr", "w") as stderr:
        code, timed_out = run(
            agent_command(args, agent, task, work),
            work,
            timeout,
            stdin=task["instruction"],
            stdout=stdout,
            stderr=stderr,
        )
    wall = time.monotonic() - start

    reward = 0.0
    verifier_timed_out = False
    if not timed_out:
        reward, verifier_timed_out = verify(task, root, work, out / f"{stem}.verifier")

    if args.keep:
        print(f"{stem}: kept {root}", file=sys.stderr)
    else:
        shutil.rmtree(root, ignore_errors=True)
    model = {"bhai": args.bhai_model, "codex": args.codex_model}.get(agent)
    return {
        "task": task["name"],
        "agent": agent,
        "model": model,
        "trial": n,
        "reward": reward,
        "wall_secs": round(wall, 3),
        "exit": code,
        "timed_out": timed_out,
        "verifier_timed_out": verifier_timed_out,
        "tokens": tokens(agent, log_path.read_text()),
        "log": str(log_path),
    }


def main(argv=None):
    p = argparse.ArgumentParser(description="Run the eval tasks against bhai and codex.")
    p.add_argument("tasks", nargs="*", help="task names or paths; all of --tasks-dir if none")
    p.add_argument("--agent", action="append", choices=AGENTS, help="repeatable; bhai if none")
    p.add_argument("--trials", type=int, default=1)
    p.add_argument("--tasks-dir", type=Path, default=HERE / "tasks")
    p.add_argument("--out", type=Path, help="default evals/results/<utc stamp>")
    p.add_argument("--bhai", default="bhai", help="the bhai binary")
    p.add_argument("--bhai-model")
    p.add_argument("--bhai-arg", action="append", default=[], help="repeatable: --bhai-arg=--bare")
    p.add_argument("--codex", default="codex", help="the codex binary")
    p.add_argument("--codex-model")
    p.add_argument("--codex-arg", action="append", default=[], help="repeatable, as --bhai-arg")
    p.add_argument("--agent-timeout", type=float, help="overrides task.toml's")
    p.add_argument("--keep", action="store_true", help="keep each trial's temp dir")
    args = p.parse_args(argv)

    if args.tasks:
        paths = [Path(t) if os.sep in t else args.tasks_dir / t for t in args.tasks]
    else:
        paths = sorted(d for d in args.tasks_dir.iterdir() if d.is_dir())
    tasks = [load_task(path) for path in paths]
    agents = args.agent or ["bhai"]
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out = args.out or HERE / "results" / stamp
    out.mkdir(parents=True, exist_ok=True)

    results = []
    with open(out / "results.jsonl", "a") as ledger:
        for task in tasks:
            for agent in agents:
                for n in range(1, args.trials + 1):
                    result = trial(args, agent, task, n, out)
                    results.append(result)
                    ledger.write(json.dumps(result) + "\n")
                    ledger.flush()
                    t = result["tokens"]
                    print(
                        f"{task['name']:<24} {agent:<6} #{n} reward {result['reward']:g}"
                        f"  {result['wall_secs']:.1f}s  in {t['input']} out {t['output']}"
                        + ("  (timed out)" if result["timed_out"] else ""),
                        flush=True,
                    )
    for agent in agents:
        mine = [r for r in results if r["agent"] == agent]
        if mine:
            mean = sum(r["reward"] for r in mine) / len(mine)
            wall = sum(r["wall_secs"] for r in mine)
            print(f"{agent}: mean reward {mean:.2f} over {len(mine)} trials, {wall:.1f}s")
    print(f"results in {out}")


if __name__ == "__main__":
    main()
