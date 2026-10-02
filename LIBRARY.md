# Library support: first milestone

The CLI and library compile the same runtime. `Runtime::new(model, session_id, prompt,
policy)` takes an `Arc<dyn Model>`, an already-built `SystemPrompt` and an `Arc<Policy>`.
It does not parse arguments, load credentials, start MCP servers or open a terminal.
`client::Client` implements `Model`; hosts can implement it themselves too.

## API and lifecycle

- `Runtime::run(user, control, events, cancel)` consumes the runtime. The channels carry
  `UserInput`, `Control` and `AgentEvent`; `Cancel` interrupts the current turn.
- Drain events concurrently. Answer each `AgentEvent::Approval` through its `reply`
  oneshot with `permissions::Answer`, or connect the existing `session::pump` hub.
  Errors are events, not a return value. `TurnEnd` closes a turn, not the runtime.
- Drop the user-input sender to end the loop, and await its task. Interrupt an active
  turn first if needed. Hosts own any MCP hubs they start and must shut them down.
- A bash command still running after its yield time stays alive as a session in its
  own process group, and sessions are process-wide: ending a runtime or exiting the
  process does not kill them. Call `tools::bash::kill_all()` once the host's runtimes
  are done, `tools::bash::kill_all_panicking()` from a panic hook, and
  `tools::bash::kill_all_on_signal()` at startup to cover hangup, interrupt and
  terminate.
- `Runtime` fields opt into persistence (`Saved`), delegation, judging, naming, usage
  logging, compaction limits and a queued-input inbox. These are off by default except
  for compaction and the existing built-in tools.
- `runtime.registry = Some(factory)` replaces the main loop's complete registry.
  `RegistryFactory` receives the prompt and current model at startup and model switches.
  Use `Registry::empty().with_tool(...)` for host-only tools. No built-ins, goal, plan,
  history or delegation tools are added to a replacement. Names must be unique and
  names/schemas stable across rebuilds. Host registries are not identity-filtered;
  the host chooses the tools, and the runtime still applies permission policy.
- `Roots { home, codex_home, cwd }`, `config::Config::load`, `identity::discover`,
  `identity::build` and `Policy::new` support explicit discovery/policy roots without
  changing the process directory. `Runtime` itself does not discover files.

## Remaining seams

This is not yet an in-process multi-project sandbox. Built-in tools and environment
messages still use the process directory; credentials and some backend caches/settings,
bash sandbox and child environment settings remain process-scoped. Set a worker's cwd
and environment before starting it, rather than changing them around concurrent runtimes.
A replacement registry does not reach built-in children or workflow steps; those still
build their own registries. UI/server dependencies are not feature-gated yet. The exposed
subsystem modules are an initial API, not a stability promise. `cli::entry` is only the
binary's frontend and may exit the process; embedding hosts should use `Runtime`.

## Offline tests

`tests/library.rs` tests the external API with a scripted model: host-only schemas,
execution after approval, rejection without execution, independent model/tool state and
registry construction. Existing unit and fake-backend CLI tests cover the shared loop.
No model server or credentials are needed for these tests.

```sh
cargo test --offline --locked --test library
cargo test --offline --locked --lib
cargo test --offline --locked --test fake_backend
cargo test --offline --locked --doc
```
