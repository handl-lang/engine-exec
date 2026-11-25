# exec

`hnl-exec` — the abstraction crate for the eval operation: the
trait/type surface for evaluation that concrete engines implement.

## Layout

| path | role |
| --- | --- |
| `src/lib.rs` | crate root / `EvalEngine` surface |
| `src/{coverage,io,native_ops,pattern_match,services}.rs` | shared eval machinery |
| `xtask/` | vendored gate crate (`fmt`, `clippy`, `test`, `doc`, `gate`) |
| `workflows/gate.yaml` | per-push gate — `handl-gates` rust campaign via bupha |

## Contract

- Implemented by the sibling `exec-*` engines: `exec-tree-walk`
  (reference interpreter — the oracle), `exec-bytecode` (bytecode VM),
  `exec-cranelift` (JIT/native, checked value-for-value against
  tree-walk).
- Deps resolve from git — a plain clone test-runs as-is; no sibling
  checkouts needed.

## Verification

```bash
cargo run --manifest-path xtask/Cargo.toml -- gate   # fmt → clippy → test → doc
```

<!-- myco:agent-awareness -->

## Agent awareness

- Session start / any work discussion: run `myco status --prompt`
  before proposing next steps — know the board first.
- User describes new work → offer to capture it (`myco todo add`,
  `issue new`, `proposal new`). Don't wait to be asked.
- Picking up a task: `myco resume <ID>` is the briefing.
- Wrapping up: `myco handoff <ID>` anything resumable; keep statuses
  honest (claimed → in-progress → blocked/done).

<!-- /myco:agent-awareness -->
