# Nexdesk Testability and Adversarial Testing Plan

## Purpose

Build a deterministic test system around Nexdesk's external boundaries so that complete client and server sessions can be exercised without a real display server, clipboard program, mDNS daemon, network peer, updater, service manager, or process exit.

The plan favors incremental extraction over a rewrite. Existing behavior and existing tests remain the compatibility baseline.

## Working agreement

- Complete subtasks in listed order unless the plan is updated with a reason.
- One independently verifiable subtask is one atomic commit.
- Update the checkbox for a subtask in the same commit as its implementation.
- Do not mix opportunistic bug fixes with structural extraction. Record newly found bugs in the findings table and add a dedicated test-first task.
- Keep real adapters thin. Put application decisions in testable session/orchestration modules.
- Use temporary directories for filesystem semantics where practical.
- Do not add production `cfg(test)` behavior switches.

## Status legend

- `[ ]` Not started
- `[x]` Complete
- Blocked work is left unchecked and annotated with `BLOCKED:` plus the reason.

## Success criteria

1. Client and server session behavior can run deterministically against in-memory peers and fake platform services.
2. Timers and retry loops can be advanced without wall-clock waiting.
3. Disconnect and shutdown tests can prove that grabs, injected inputs, cursor state, and background tasks are cleaned up.
4. Pairing, trust, updates, clipboard, discovery, and transport failures can be scripted at each meaningful boundary.
5. Real QUIC framing/TLS behavior is covered by localhost integration tests.
6. Linux and macOS CI run the appropriate test suites.
7. Protocol and state-machine fuzz/property tests enforce safety invariants.

## Architectural target

```text
src/
  lib.rs                     reusable crate entry point
  main.rs                    thin CLI composition root
  app/
    client/                  client orchestration and lifecycle
    server/                  server orchestration and lifecycle
    handshake.rs             role-specific handshake decisions
    reconnect.rs             retry policy
  ports/
    transport.rs             typed peer events and outbound operations
    platform.rs              input, session, display, and host capabilities
    clipboard.rs             text/file clipboard semantics
    discovery.rs             peer discovery semantics
    trust.rs                 fingerprint trust persistence
    update.rs                release source and installation semantics
    clock.rs                 deterministic time and intervals
    status.rs                runtime status sink
    lifecycle.rs             restart/shutdown decisions
  adapters/
    quinn/                   real transport and framing
    mdns.rs                  real discovery
    update.rs                GitHub/reqwest update adapter
    linux/                   Linux platform adapters
    macos/                   macOS platform adapters
  testing/
    client_rig.rs            deterministic client scenario builder
    server_rig.rs            deterministic server scenario builder
    fakes/                   stateful scripted fakes and recorders
tests/
  client_scenarios.rs
  server_scenarios.rs
  quic_loopback.rs
  cli.rs
```

This is a direction, not a requirement to create every file immediately. Modules should be introduced only when their extraction begins.

---

## Phase 0 — Baseline, crate shape, and CI

### T0.1 — Record and protect the baseline

- [x] **T0.1.1** Add a repository test command document covering formatting, unit tests, platform limitations, and the current strict-clippy baseline.
- [x] **T0.1.2** Add a test CI workflow that runs formatting and `cargo test --all-targets` on Linux with required native packages.
- [x] **T0.1.3** Add a macOS test job so macOS-only adapters and tests compile and run.
- [x] **T0.1.4** Add a non-blocking clippy job without `-D warnings`, then separately track the existing warning cleanup.
- [x] **T0.1.5** Add `cargo-llvm-cov` reporting and publish/store the coverage artifact without imposing an initial threshold.

### T0.2 — Expose a library without changing CLI behavior

- [x] **T0.2.1** Create `src/lib.rs`, move the shared module tree out of `main.rs`, and update the binary to consume the library without changing CLI behavior.
- [x] **T0.2.2** Move CLI dispatch into a library `run` function that accepts parsed CLI arguments while leaving process setup in `main.rs`.
- [x] **T0.2.3** Reduce the library's exposed module surface and leave `main.rs` as a thin composition root.
- [x] **T0.2.4** Add an integration smoke test proving public library access and CLI parsing.

### T0.3 — Establish test support conventions

- [x] **T0.3.1** Add dev dependencies for property testing, CLI assertions, and Tokio virtual-time support.
- [x] **T0.3.2** Create `src/testing` with a typed observation log shared by stateful fakes.
- [x] **T0.3.3** Add a task tracker capable of detecting background tasks that outlive a scenario.
- [x] **T0.3.4** Document fake behavior conventions: scripted results, blocking gates, call recording, and unexpected-call failures.

---

## Phase 1 — Client session vertical slice

### T1.1 — Make client lifecycle decisions testable

- [x] **T1.1.1** Introduce `SessionExit` values for clean disconnect, retry, restart request, and fatal termination.
- [x] **T1.1.2** Change client internals to return restart intent instead of invoking `process::exit` below the composition root.
- [x] **T1.1.3** Add tests proving update and latency paths request restart only under intended conditions.
- [x] **T1.1.4** Move reconnect delay calculation into a pure retry-policy type with unit tests.
- [x] **T1.1.5** Make the client reconnect loop cancellable and test cancellation during resolution, connection, session, and backoff.

### T1.2 — Inject client platform behavior

- [x] **T1.2.1** Add an injectable `InputInjectorFactory` while retaining the existing `InputInjector` interface.
- [x] **T1.2.2** Implement a recording injector that tracks moves, events, cursor visibility, screen size, pressed keys/buttons, and scripted failures.
- [x] **T1.2.3** Pass the injector factory into the client connection/session path instead of constructing the platform injector internally.
- [x] **T1.2.4** Add a display/session-control port for wake and sleep inhibition.
- [x] **T1.2.5** Implement fake display/session control with observation recording and blocking/error injection.
- [x] **T1.2.6** Replace direct client wake/inhibitor calls with the injected port.

### T1.3 — Extract trust and pairing decisions

- [x] **T1.3.1** Introduce a focused `TrustStore` interface.
- [x] **T1.3.2** Adapt the existing TLS/config trust functions behind the production store.
- [x] **T1.3.3** Add an in-memory trust store supporting read/write failures.
- [x] **T1.3.4** Extract client handshake decision logic from Quinn stream mechanics.
- [x] **T1.3.5** Add table-driven tests for trusted reconnect, OTP success/failure, fingerprint mismatch, malformed messages, and stream closure at each handshake stage.
- [x] **T1.3.6** Extract pairing-code input behind a prompt interface and test interactive/non-interactive behavior without real stdin.

### T1.4 — Extract updater behavior

- [x] **T1.4.1** Introduce semantic `ReleaseRepository` and `UpdateInstaller` interfaces rather than a generic HTTP mock.
- [x] **T1.4.2** Separate update policy from download/install mechanics.
- [x] **T1.4.3** Add fakes for release lookup, asset streaming, installation, and restart observation.
- [x] **T1.4.4** Route client protocol-mismatch and post-handshake updates through update policy.
- [x] **T1.4.5** Add scenarios for trusted/untrusted sources, dirty versions, downgrade/equal versions, download failure, install failure, and successful restart request.

### T1.5 — Introduce typed client transport events

- [x] **T1.5.1** Define typed channels/events for control, input, clipboard, transport closure, and transport failure.
- [x] **T1.5.2** Extract message framing into one shared codec/stream module and remove duplicate framing implementations where practical.
- [x] **T1.5.3** Build a Quinn client adapter that translates concrete streams into typed events.
- [x] **T1.5.4** Build an in-memory scripted peer link with per-channel delay, blocking, closure, and failure injection.
- [x] **T1.5.5** Move the post-handshake client loop to the typed peer link.
- [x] **T1.5.6** Add tests for closure and partial failure of each logical channel.

### T1.6 — Extract client clipboard behavior

- [x] **T1.6.1** Introduce a semantic clipboard interface for text and files.
- [x] **T1.6.2** Move Linux/macOS clipboard commands behind production adapters.
- [x] **T1.6.3** Add a memory clipboard with scripted reads/writes, blocking gates, and change history.
- [x] **T1.6.4** Inject clipboard behavior into `ClipboardSync` and the client session.
- [x] **T1.6.5** Add tests for no-echo behavior, oversized data, read/write failure, blocked clipboard operations, and shutdown.
- [x] **T1.6.6** Ensure blocking clipboard work cannot block the async session loop and prove it with a scenario test.

### T1.7 — Build the deterministic client rig

- [x] **T1.7.1** Create `ClientRig` with sensible trusted-peer defaults and access to every fake.
- [x] **T1.7.2** Add `run_until_idle`, explicit shutdown, and virtual-time advancement helpers.
- [x] **T1.7.3** Add assertions for pressed inputs, cursor state, status history, outbound peer messages, and task completion.
- [x] **T1.7.4** Add disconnect-while-key-held and disconnect-while-button-held scenarios.
- [x] **T1.7.5** Add switch-back/key-up race and duplicate-release scenarios.
- [x] **T1.7.6** Add screen resize, invalid screen, injector failure, and control-stream failure scenarios.
- [x] **T1.7.7** Add heartbeat latency, delayed acknowledgement, watchdog recovery, and watchdog restart scenarios using virtual time.
- [x] **T1.7.8** Add assertions that every client exit restores input and cursor state and terminates all tasks.

---

## Phase 2 — Server session vertical slice

### T2.1 — Inject server input and platform behavior

- [x] **T2.1.1** Add an injectable `InputCaptureFactory` around the existing capture interface.
- [x] **T2.1.2** Implement a scripted capturer supporting positions, buttons, keys, screen sizes, grab history, and failures.
- [x] **T2.1.3** Inject capture creation into the server connection path.
- [x] **T2.1.4** Introduce an injectable local-session lock source.
- [x] **T2.1.5** Route server lock checks and display wake through platform ports.
- [x] **T2.1.6** Add tests proving local input release happens before potentially blocking network notification.

### T2.2 — Move server sessions onto typed transport

- [x] **T2.2.1** Build a Quinn server adapter producing the same typed peer-link abstraction.
- [x] **T2.2.2** Separate server handshake decisions from concrete streams.
- [x] **T2.2.3** Add server handshake scenarios for OTP, trusted certificates, absent certificates, version mismatch, malformed messages, and disconnects.
- [x] **T2.2.4** Move the server event loop to typed transport operations.
- [x] **T2.2.5** Make all connection-owned background tasks supervised and joined during shutdown.
- [x] **T2.2.6** Add tests for failure/closure of each server logical channel.

### T2.3 — Build the deterministic server rig

- [ ] **T2.3.1** Create `ServerRig` with scripted capture, peer, clipboard, lock source, status sink, clock, and task tracker.
- [ ] **T2.3.2** Add edge activation and shortcut activation scenarios.
- [ ] **T2.3.3** Add safety escape and switch-back cleanup scenarios.
- [ ] **T2.3.4** Add local-lock-during-sharing scenarios for polling and layer-shell modes.
- [ ] **T2.3.5** Add blocked-send, input-capture failure, peer resize, and disconnect scenarios.
- [ ] **T2.3.6** Assert every server exit releases pointer/keyboard grabs and all held remote inputs.
- [ ] **T2.3.7** Assert all server connection tasks terminate on shutdown.

---

## Phase 3 — Discovery, persistence, services, and file transfer

### T3.1 — Discovery

- [ ] **T3.1.1** Introduce a discovery interface supporting browse streams and single-peer resolution.
- [ ] **T3.1.2** Adapt `mdns-sd` behind the production discovery adapter.
- [ ] **T3.1.3** Add scripted discovery with delayed peers, malformed peers, closure, and failures.
- [ ] **T3.1.4** Test discovery retry, timeout, cancellation, deduplication, and address selection with virtual time.
- [ ] **T3.1.5** Add a real mDNS smoke test marked for environments where multicast is available.

### T3.2 — Config, trust, certificates, and status

- [ ] **T3.2.1** Make config/certificate/status roots explicitly injectable while keeping production defaults.
- [ ] **T3.2.2** Add repository interfaces only around operations requiring fault injection; retain real temp filesystem semantics otherwise.
- [ ] **T3.2.3** Add atomic-write failure tests covering create, write, flush, sync, persist, and directory-sync stages.
- [ ] **T3.2.4** Add concurrent trust/config update tests.
- [ ] **T3.2.5** Add stale, corrupt, oversized, permission, and process-reuse status scenarios.

### T3.3 — Service manager and command execution

- [ ] **T3.3.1** Introduce a bounded command-runner interface shared by command-based adapters.
- [ ] **T3.3.2** Implement real and scripted command runners with stdout/stderr limits, hangs, signals, and exit statuses.
- [ ] **T3.3.3** Route systemd, launchd, session query, and clipboard command adapters through the runner where appropriate.
- [ ] **T3.3.4** Add Linux service install/start/stop/status scenario tests without invoking systemd or sudo.
- [ ] **T3.3.5** Add macOS service install/start/stop/status scenario tests without invoking launchctl.
- [ ] **T3.3.6** Add command timeout and child-process cleanup tests.

### T3.4 — File transfer

- [ ] **T3.4.1** Move file-transfer protocol flow onto reusable typed message streams independent of Quinn concrete stream types.
- [ ] **T3.4.2** Add deterministic transfer identifiers through injected entropy.
- [ ] **T3.4.3** Add sender scenarios for mutation, truncation, growth, identity change, cancel, timeout, and mid-frame disconnect.
- [ ] **T3.4.4** Add receiver scenarios for malformed offers, offsets, duplicates, checksum errors, collisions, cancellation, timeout, and disk failure.
- [ ] **T3.4.5** Add concurrent transfer limit and shutdown tests.
- [ ] **T3.4.6** Add end-to-end in-memory file transfer tests using real temporary files.

---

## Phase 4 — Real-adapter and CLI integration

### T4.1 — QUIC loopback

- [ ] **T4.1.1** Create localhost Quinn test endpoints with ephemeral ports and temporary certificates.
- [ ] **T4.1.2** Test successful client/server handshake and all logical stream setup.
- [ ] **T4.1.3** Test invalid TLS identity, fingerprint mismatch, and untrusted pairing.
- [ ] **T4.1.4** Test split frames, mid-frame closure, oversized frames, and malformed payloads.
- [ ] **T4.1.5** Test control/input/clipboard independence under delay and closure.
- [ ] **T4.1.6** Test graceful shutdown and verify endpoints/tasks become idle.

### T4.2 — HTTP/update adapter contracts

- [ ] **T4.2.1** Add a local HTTP server fixture for the reqwest adapter only.
- [ ] **T4.2.2** Test release lookup statuses, malformed JSON, chunked bodies, size limits, and timeouts.
- [ ] **T4.2.3** Test binary download statuses, empty/truncated/chunked bodies, declared/actual size limits, and timeouts.
- [ ] **T4.2.4** Test successful atomic installation in a temporary executable root.
- [ ] **T4.2.5** Add an opt-in live GitHub contract smoke test that is excluded from normal CI.

### T4.3 — CLI and setup

- [ ] **T4.3.1** Add `assert_cmd` tests for help, invalid arguments, invalid configured roles/edges, and non-interactive errors.
- [ ] **T4.3.2** Extract setup state transitions from terminal rendering/input.
- [ ] **T4.3.3** Add setup workflow tests for server/client roles, discovery/manual address, back navigation, and cancellation.
- [ ] **T4.3.4** Add Ratatui buffer rendering tests for each setup screen.
- [ ] **T4.3.5** Add service-install setup scenarios using fake service, trust, discovery, and pairing ports.

---

## Phase 5 — Property, fuzz, mutation, and quality gates

### T5.1 — Property/model testing

- [ ] **T5.1.1** Add generated `ClientTransition` event sequences with cursor/input safety invariants.
- [ ] **T5.1.2** Add generated `ServerTransition` sequences with grab and held-key invariants.
- [ ] **T5.1.3** Add generated full client-session sequences over the fake peer link.
- [ ] **T5.1.4** Add generated full server-session sequences over the fake peer link.
- [ ] **T5.1.5** Persist minimal regression cases for every property-test defect found.

### T5.2 — Fuzzing

- [ ] **T5.2.1** Add a protocol decode/validation fuzz target.
- [ ] **T5.2.2** Add a framed-stream chunk-boundary fuzz target.
- [ ] **T5.2.3** Add a file-transfer message-sequence fuzz target.
- [ ] **T5.2.4** Add bounded fuzz runs to scheduled CI and retain crashing artifacts.

### T5.3 — Mutation and coverage gates

- [ ] **T5.3.1** Run `cargo-mutants` against transition and protocol modules and record surviving mutants.
- [ ] **T5.3.2** Add tests that kill high-value surviving mutants.
- [ ] **T5.3.3** Establish per-module coverage visibility for core and orchestration code.
- [ ] **T5.3.4** Introduce a coverage threshold only after deterministic suites stabilize.
- [ ] **T5.3.5** Make formatting, tests, selected clippy rules, and deterministic scenario suites required CI checks.

### T5.4 — Existing quality debt

- [ ] **T5.4.1** Resolve or explicitly annotate current dead-code warnings.
- [ ] **T5.4.2** Resolve current strict-clippy findings without mixing behavior changes.
- [ ] **T5.4.3** Enable `cargo clippy --all-targets -- -D warnings` as a required check.

---

## Initial scenario acceptance suite

The first complete client/server rigs must cover these release-blocking invariants:

- [ ] **A1** Client disconnect releases all tracked keys and buttons.
- [ ] **A2** Client disconnect restores cursor visibility.
- [ ] **A3** Server disconnect releases all input grabs.
- [ ] **A4** Local session lock releases grabs before any network await.
- [ ] **A5** Inactive client never injects key/button presses.
- [ ] **A6** Switch-back races cannot leave synthetic input held.
- [ ] **A7** Shutdown terminates all session-owned tasks.
- [ ] **A8** Clipboard blocking cannot stall input/control processing.
- [ ] **A9** Untrusted peers cannot trigger an update.
- [ ] **A10** Successful updates request restart; failed updates do not.
- [ ] **A11** Every outbound protocol message passes semantic validation.
- [ ] **A12** Reconnect/discovery/file-transfer timeouts run under virtual time.

## Findings discovered during implementation

| ID | Status | Finding | Planned test/fix |
|---|---|---|---|
| — | — | No implementation findings recorded yet. | — |

## Progress log

| Date | Commit | Completed item | Notes |
|---|---|---|---|
| 2026-07-19 | `bef8a99` | Plan created | Baseline before implementation. |
| 2026-07-19 | `013906d` | T0.1.1 | Documented test commands, platform limits, and lint baseline. |
| 2026-07-19 | `0855e7d` | T0.1.2 | Added Linux formatting and test CI. |
| 2026-07-19 | `2943e1f` | T0.1.3 | Added macOS test CI for target-gated adapters. |
| 2026-07-19 | `5ece775` | T0.1.4 | Added advisory Linux clippy CI without a warning gate. |
| 2026-07-19 | `3107586` | T0.1.5 | Added informational LCOV generation and artifact storage. |
| 2026-07-19 | `a3e6fd8` | T0.2.1 | Introduced the library target and shared module tree. |
| 2026-07-19 | `e4a517d` | T0.2.2 | Moved parsed CLI dispatch into the library. |
| 2026-07-19 | `338284f` | T0.2.3 | Restricted internal modules and kept the binary as composition root. |
| 2026-07-19 | `4e6e560` | T0.2.4 | Added public library and CLI parsing integration smoke tests. |
| 2026-07-19 | `3779f78` | T0.3.1 | Added property, CLI assertion, and virtual-time test dependencies. |
| 2026-07-19 | `23b3f25` | T0.3.2 | Added a shared typed observation log for stateful fakes. |
| 2026-07-19 | `ae2a887` | T0.3.3 | Added background task lifetime tracking for scenarios. |
| 2026-07-19 | `86c874d` | T0.3.4 | Documented stateful fake scripting and lifecycle conventions. |
| 2026-07-19 | `3998d95` | T1.1.1 | Added typed session exit and restart outcomes. |
| 2026-07-19 | `8e4651a` | T1.1.2 | Propagated client restart intent to the binary composition root. |
| 2026-07-19 | `6bcd135` | T1.1.3 | Covered update and latency restart decision paths. |
| 2026-07-19 | `dcb0bf9` | T1.1.4 | Moved reconnect delay calculation into a pure policy. |
| 2026-07-19 | `0f286b1` | T1.1.5 | Added staged reconnect cancellation and deterministic coverage. |
| 2026-07-19 | `a97338b` | T1.2.1 | Added an object-safe input injector factory boundary. |
| 2026-07-19 | `5778c6b` | T1.2.2 | Added a stateful recording injector and scripted factory. |
| 2026-07-19 | `4a2620c` | T1.2.3 | Injected the input factory through client connection sessions. |
| 2026-07-19 | `8c522ef` | T1.2.4 | Added display wake and sleep-inhibition platform ports. |
| 2026-07-19 | `4da1a86` | T1.2.5 | Added scripted display control with blocking gates and observations. |
| 2026-07-19 | `7a0f65b` | T1.2.6 | Routed client wake and sleep inhibition through the platform port. |
| 2026-07-19 | `7795f1a` | T1.3.1 | Added an object-safe peer trust persistence port. |
| 2026-07-19 | `055301e` | T1.3.2 | Adapted normalized config trust behind the production store. |
| 2026-07-19 | `d7f9439` | T1.3.3 | Added observable in-memory trust with read/write failures. |
| 2026-07-19 | `5d35bbc` | T1.3.4 | Extracted client hello validation and trust-based pairing decisions. |
| 2026-07-19 | `578461a` | T1.3.5 | Added table-driven client handshake and closure coverage. |
| 2026-07-19 | `ca18904` | T1.3.6 | Injected pairing prompts and tested terminal modes without stdio. |
| 2026-07-19 | `fe49332` | T1.4.1 | Added semantic release lookup, asset streaming, and installation ports. |
| 2026-07-19 | `e14286b` | T1.4.2 | Extracted pure update eligibility policy from updater mechanics. |
| 2026-07-19 | `ffdb47b` | T1.4.3 | Added scripted release, asset, installer, and restart fakes. |
| 2026-07-19 | `3abd6be` | T1.4.4 | Routed client update paths through policy and injected adapters. |
| 2026-07-19 | `5c46f8c` | T1.4.5 | Covered update eligibility, failure, install, and restart scenarios. |
| 2026-07-19 | `7ee36f9` | T1.5.1 | Defined typed client transport channels, events, and commands. |
| 2026-07-19 | `8eb426e` | T1.5.2 | Centralized validated framing across QUIC and file transfer streams. |
| 2026-07-19 | `cddbb50` | T1.5.3 | Added Quinn translation between wire streams and typed client events. |
| 2026-07-19 | `9edc689` | T1.5.4 | Added a scripted typed peer with delay, blocking, closure, and failures. |
| 2026-07-19 | `14cee51` | T1.5.5 | Moved post-handshake client I/O onto the typed peer link. |
| 2026-07-19 | `98d9ec0` | T1.5.6 | Covered closure and failure dispositions for every client channel. |
| 2026-07-19 | `74ba5a8` | T1.6.1 | Added semantic clipboard text and file access port. |
| 2026-07-19 | `681635d` | T1.6.2 | Routed Linux/macOS text and file commands through a production adapter. |
| 2026-07-19 | `31e4aca` | T1.6.3 | Added stateful clipboard scripting, blocking gates, and history. |
| 2026-07-19 | `85ee8ab` | T1.6.4 | Injected clipboard access into sync logic and the client session. |
| 2026-07-19 | `ed7491d` | T1.6.5 | Covered clipboard echo, limits, failures, blocking, and shutdown. |
| 2026-07-19 | `908b1aa` | T1.6.6 | Moved clipboard OS calls to workers and proved async responsiveness. |
| 2026-07-19 | `ee5b0d9` | T1.7.1 | Added a trusted-peer client rig exposing stateful test dependencies. |
| 2026-07-19 | `06e62d7` | T1.7.2 | Added client rig settling, shutdown, and virtual-time controls. |
| 2026-07-19 | `75294b7` | T1.7.3 | Added client state, status, peer-message, and task assertions. |
| 2026-07-19 | `d64834b` | T1.7.4 | Covered disconnect cleanup with held keys and mouse buttons. |
| 2026-07-19 | `2d05d75` | T1.7.5 | Covered late key-up races and duplicate release suppression. |
| 2026-07-19 | `12e2b3c` | T1.7.6 | Covered resize validation, injector errors, and control failures. |
| 2026-07-19 | `63fa581` | T1.7.7 | Added virtual-time latency, recovery, and watchdog scenarios. |
| 2026-07-19 | `dbf7254` | T1.7.8 | Restored client input/cursor state and joined tasks on every exit. |
| 2026-07-19 | `f2f2a05` | T2.1.1 | Added an object-safe server input capture factory. |
| 2026-07-19 | `2599383` | T2.1.2 | Added scripted server input state, grabs, and failures. |
| 2026-07-19 | `9b90b79` | T2.1.3 | Injected capture creation through the server connection path. |
| 2026-07-19 | `6f06fa3` | T2.1.4 | Added an injectable local graphical-session lock source. |
| 2026-07-19 | `96e0332` | T2.1.5 | Routed server lock checks and display wake through platform ports. |
| 2026-07-19 | `9e6382e` | T2.1.6 | Released local input before awaiting lock notifications. |
| 2026-07-19 | `1c70c1d` | T2.2.1 | Added a typed Quinn server peer-link adapter. |
| 2026-07-19 | `71ad09b` | T2.2.2 | Separated server handshake policy from Quinn streams. |
| 2026-07-19 | `c2388c7` | T2.2.3 | Covered server OTP, certificate, version, malformed, and disconnect handshakes. |
| 2026-07-19 | `81e6d2c` | T2.2.4 | Moved server channel events and sends onto typed transport operations. |
| 2026-07-19 | `a343f61` | T2.2.5 | Supervised and joined server connection tasks during shutdown. |
| 2026-07-19 | this commit | T2.2.6 | Covered closure and failure policy for every server channel. |
