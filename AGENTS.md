# Repository Agent Instructions

## Atomic changes and commits

- Break work into independently verifiable atomic changes before implementation.
- Finish one atomic change at a time.
- After finishing an atomic change, run its relevant checks and create a git commit before starting the next change.
- When work is tracked in a plan or checklist, update the corresponding status in the same commit as the implementation.
- Stage only files that belong to the atomic change. Never stage, discard, overwrite, or otherwise modify unrelated working-tree changes.
- Use concise imperative commit messages that describe the completed atomic change.
- Do not amend or rewrite existing commits unless the user explicitly asks.
- If an atomic change cannot be verified or committed, stop and report the blocker rather than accumulating unrelated work.

## Change discipline

- Read the relevant code and repository documentation before editing.
- Preserve existing behavior unless the task explicitly requires a behavior change.
- Add or update tests for behavior changes and bug fixes.
- Keep refactors separate from unrelated fixes.
- Prefer focused changes over broad rewrites.
- Record newly discovered out-of-scope issues instead of silently expanding the current change.
- Do not modify user-owned working-tree changes unless they are explicitly part of the task.
