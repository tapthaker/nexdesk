# Stateful Fake Conventions

Nexdesk scenario tests use hand-written stateful fakes for application boundaries. These fakes are intended to model behavior and ordering, not to reproduce implementation details of libraries such as Quinn, reqwest, or `std::process::Command`.

## Core rules

1. **Fake semantic boundaries.** Prefer `TrustStore::trust` or `Clipboard::write_text` over mocking individual filesystem calls or command-builder methods.
2. **Make defaults explicit and successful.** A rig may provide a documented happy-path configuration, but each fake must expose the state that produced it.
3. **Script exceptional behavior.** Tests must be able to queue failures, closures, delays, and blocked operations at each meaningful call.
4. **Record every meaningful call.** Use the shared `ObservationLog` when ordering across multiple fakes matters.
5. **Fail unexpected calls.** Silently returning a default from an unconfigured operation hides orchestration defects.
6. **Keep clones stateful.** Clones of a fake represent handles to the same simulated external resource, not independent copies.
7. **Do not branch production behavior for tests.** Production and fake adapters implement the same port.

## Scripted results

A fake operation that can produce different outcomes should consume a FIFO script:

```text
read_text:
  1. Ok("first")
  2. Err(Unavailable)
  3. Block(gate-1), then Ok("second")
```

Conventions:

- Each invocation consumes exactly one scripted action unless the fake documents a reusable fallback.
- An empty script is an unexpected call and should fail with the fake name, operation, and call index.
- Scripts should contain owned domain values or stable test error types, not production-library errors.
- A test that intentionally allows unlimited calls should configure an explicit repeat/fallback action.

## Blocking gates

Blocking behavior is needed to test operation ordering and responsiveness.

- A gate must tell the test when the operation has reached the blocked point.
- The test explicitly releases or fails the gate.
- Dropping the test controller must unblock the operation with a clear cancellation error; it must not leave a task hanging indefinitely.
- Async ports use async gates. Blocking OS adapters should be exercised through controlled blocking workers rather than blocking a Tokio executor thread.
- Every blocked-operation test uses a bounded harness timeout as a final test-failure guard, not as application timing behavior.

## Observation recording

`nexdesk::testing::ObservationLog<E>` supplies a total sequence across all fake handles sharing the same typed event log.

Events should describe domain-visible behavior:

```text
InputGrabChanged(false)
ControlSend(SwitchScreen(Left))
ClipboardWrite(bytes=42)
RestartRequested
```

Avoid events tied to private implementation details such as mutex acquisition or a specific helper function call. Sensitive or potentially huge payloads should be summarized and bounded.

Use sequence order for safety assertions, for example:

```text
InputGrabChanged(false) occurs before ControlSend(ReleaseScreen)
```

Snapshots are non-destructive. Draining is reserved for tests that intentionally divide execution into phases.

## Call history and state inspection

Each fake should expose only the inspection needed for stable assertions:

- call count and bounded argument history
- current simulated state
- typed observation snapshot
- remaining scripted actions
- active blocking gates

Rigs should assert that required scripts were consumed and that no unexplained actions remain. Avoid tests that inspect internal mutexes, channel capacity, or adapter-specific objects.

## Task lifetime

Every scenario-owned background task must be registered with `TaskTracker` for its full lifetime.

- Start tracking inside the spawned future so task registration matches actual execution.
- Keep the guard alive until cleanup is complete, not merely until shutdown is requested.
- At scenario teardown, call `wait_for_idle` with a bounded harness timeout and then `ensure_idle` for a useful leak report.
- A detached production task that cannot be tracked is a design issue to resolve, not an exception to hide in the rig.

## Time

Application delays and intervals will use an injected clock or Tokio virtual time.

- Never add real sleeps to deterministic scenario tests.
- Advance only enough virtual time to cross the behavior boundary being tested.
- Record timeout/retry decisions as observations when they are relevant.
- Wall-clock timeouts may wrap the complete test solely to prevent a broken test from hanging CI.

## Errors and assertions

- Use small typed fake errors with stable categories.
- Do not assert complete human-formatted error reports when a category or specific bounded fragment is sufficient.
- A fake should not log-and-ignore an error on behalf of the application.
- Tests should assert both the returned outcome and important cleanup side effects.
- Panic messages for harness misuse should identify the fake, operation, and expected script configuration.

## Filesystem behavior

Prefer real files under `tempfile` directories for atomic replacement, permissions, checksums, file identity, and collision behavior. Introduce a repository fake only when a test must inject a failure that cannot be produced reliably with a temporary filesystem.

## Thread safety and poisoning

Fakes shared by async tasks use `Arc` plus a narrowly scoped lock. They must not hold a synchronous lock across `.await`. Test-support inspection should recover poisoned locks where doing so produces a better failure report, while production application ports should retain their intended error semantics.

## Naming

Use names that communicate behavior:

- `RecordingInjector`
- `ScriptedPeerLink`
- `MemoryTrustStore`
- `BlockingClipboard`
- `FakeUpdateInstaller`

Avoid ambiguous names such as `MockApi`, `TestService`, or `DummyClient`.
