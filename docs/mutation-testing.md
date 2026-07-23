# Mutation testing

## Transition and protocol baseline

The initial targeted run used `cargo-mutants 27.1.0` against the protocol and transition state machines at commit `1a36b26`:

```bash
cargo mutants \
  --file 'src/net/{transition,protocol}.rs' \
  -j 4 \
  --baseline skip \
  --cargo-test-arg=--skip \
  --cargo-test-arg=client_update_uses_injected_repository_and_installer
```

The copied cargo-mutants workspace reused stale build-script version metadata, causing the unrelated version-sensitive updater test to fail before mutation testing began. The test passed separately in the repository working tree, so the targeted rerun skipped only that test.

Results:

| Outcome | Count |
|---|---:|
| Total generated | 220 |
| Caught by tests | 209 |
| Survived | 0 |
| Timed out | 0 |
| Unviable | 11 |

The 11 unviable mutations attempted to return `Default::default()` values for protocol or transition types that intentionally do not implement `Default`; they failed to compile. There were no surviving mutants to carry into the high-value test follow-up.

Future mutation runs should record the command, tool version, tested commit, counts, and every surviving mutant here. A surviving mutant should receive either a focused test or an explicit explanation before mutation coverage is considered complete.
