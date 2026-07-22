# Testing Nexdesk

This document describes the repository's current verification commands and known platform limitations. The broader testability roadmap is tracked in [`testability-plan.md`](testability-plan.md), and test doubles follow the conventions in [`testing-fakes.md`](testing-fakes.md).

## Required toolchain

- Stable Rust toolchain
- Cargo
- Platform-native development libraries listed below

### Linux packages

On Ubuntu runners and development machines, install:

```bash
sudo apt-get update
sudo apt-get install -y \
  libxcb1-dev \
  libxcb-shape0-dev \
  libxcb-xfixes0-dev \
  libx11-dev \
  libevdev-dev \
  pkg-config
```

These are compile-time dependencies. The unit tests do not require a running X11 or Wayland session.

### macOS

Use a macOS host with the stable Rust toolchain. Unit tests should not request Accessibility permission or inject real input. Linux-only X11, evdev, and Wayland modules are excluded by target configuration; macOS input modules are only compiled and tested on macOS.

## Local verification commands

Run formatting verification:

```bash
cargo fmt --all -- --check
```

Run the target platform's complete unit-test suite:

```bash
cargo test --all-targets
```

Run the selected required lint rules:

```bash
cargo clippy --all-targets -- \
  -D clippy::await_holding_lock \
  -D clippy::let_underscore_future \
  -D clippy::zombie_processes \
  -D clippy::suspicious_open_options \
  -D clippy::ineffective_open_options
```

Run the explicit deterministic scenario gate:

```bash
scripts/test-deterministic.sh
```

A broad `cargo clippy --all-targets` run remains useful for advisory quality findings until T5.4 resolves the existing warning baseline.

Before committing a change, run the narrowest relevant test command while developing, followed by formatting and `cargo test --all-targets` when the change can affect shared behavior.

A single test can be run with a fully qualified filter, for example:

```bash
cargo test net::transition::tests::server_switch_back_releases_held_keys
```

The normal suite includes generated transition and client/server session properties. Proptest shrinks failures and writes replay seeds under `proptest-regressions/`; commit each generated seed with its fix so the minimized case remains in every subsequent run.

Run a bounded fuzz target after installing nightly Rust and `cargo-fuzz`:

```bash
cargo +nightly fuzz run protocol_decode -- -max_total_time=60 -timeout=10
cargo +nightly fuzz run framed_chunks -- -max_total_time=60 -timeout=10
cargo +nightly fuzz run file_transfer_sequence -- -max_total_time=60 -timeout=10
```

Scheduled CI runs each target for two minutes and retains any files under `fuzz/artifacts/<target>` for 30 days. Reproduce and minimize a retained crash before committing it to the target's corpus with its fix.

Targeted mutation-test commands and results are recorded in [`mutation-testing.md`](mutation-testing.md).

Generate an LCOV coverage report after installing [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov):

```bash
cargo llvm-cov --all-targets --lcov --output-path lcov.info
```

CI stores `lcov.info` and `coverage-summary.md` as the `coverage-lcov` artifact. The job summary reports line and function coverage per core/orchestration module and grouped area totals. Generate the same summary locally with:

```bash
python scripts/coverage-summary.py lcov.info
```

The deterministic Linux baseline currently reports 88.3% core line coverage and 64.2% orchestration line coverage. CI enforces conservative regression floors of 85% and 60%, respectively; it does not gate on function coverage yet. Reproduce the gate locally with:

```bash
python scripts/coverage-summary.py lcov.info \
  --fail-under-core-lines 85 \
  --fail-under-orchestration-lines 60
```

Run the opt-in live GitHub update contract smoke test only when external network access is intended:

```bash
cargo test live_github_update_contract_smoke -- --ignored --nocapture
```

This test checks the latest-release response and begins streaming the matching platform asset. It is ignored by normal local and CI test runs.

## Current baseline

At the start of the testability project on 2026-07-19:

- `cargo fmt --all -- --check` passes.
- Linux `cargo test --all-targets` runs 233 tests successfully.
- The source contains additional target-gated macOS tests that do not run on Linux.
- There are no tests in a top-level `tests/` integration-test directory yet.
- `cargo clippy --all-targets` succeeds with warnings.
- Strict clippy is not yet a passing gate:

  ```bash
  cargo clippy --all-targets -- -D warnings
  ```

  Current findings include dead code in target-specific input mappings, unused Wayland event fields, type-complexity suggestions, collapsible matches, items placed after test modules, and other style findings. These are existing quality debt tracked separately in Phase 5 of the testability plan; structural test work should not silently mix in unrelated lint cleanup.

## Platform coverage rules

- A Linux run does not establish that macOS adapters compile or behave correctly.
- A macOS run does not exercise Linux X11, evdev, or Wayland adapters.
- Pure protocol, transition, configuration, and orchestration behavior should remain platform-independent and run on both platforms.
- Adapter tests may be target-gated when they genuinely require platform APIs.
- Tests must not rely on the developer's active clipboard, display server, service manager, home-directory configuration, or network peers.
- Real-network smoke tests must bind ephemeral localhost ports. Multicast or live-service tests must be opt-in and excluded from the normal deterministic suite.

## Test categories during migration

The repository will incrementally add these layers:

1. Pure unit tests in source modules.
2. Deterministic session scenarios using stateful fakes.
3. Integration tests through the public library API.
4. Localhost QUIC and local HTTP adapter contract tests.
5. Platform-specific adapter smoke tests.
6. Property, fuzz, coverage, and mutation-testing jobs.

Until those layers exist, a green unit suite should not be interpreted as complete end-to-end coverage.
