#!/usr/bin/env bash
set -euo pipefail

# Keep the generated lifecycle properties, named fake-adapter scenarios, and
# localhost QUIC contracts visible as an explicit required CI suite. The full
# all-target test run remains the broader compatibility gate.
cargo test generated_
cargo test scenario
cargo test quic_loopback
