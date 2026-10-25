# engine-exec

The eval-operation contract: `Eval`, `EvalRequest`, `EvalEngine`.

## What it is

The **abstraction crate** for the eval operation (ADR-0044 §3, §4, §7):
the `Eval` operation marker, the `EvalRequest` an engine receives, and the
`EvalEngine` trait it implements. **No interpreter lives here.** The concrete
engines are `engine-exec-tree-walk` (reference interpreter) and
`engine-exec-bytecode` (stack VM), which must agree value-for-value.

## Modules
- `io` — the `IO` effect surface: handler frames, resume, accumulation
- `native_ops` — the native operation table an engine dispatches to
- `pattern_match` — `try_pattern` — shared pattern matching on a growable stack


## Start here

- **Reference** — [public API](/engine-exec/reference/)
- **How-to** — [task recipes](/engine-exec/how-to/)
- **Explanation** — [design and rationale](/engine-exec/explanation/)

No tutorials: this repo is a building block used from Rust. End-to-end
walkthroughs live with the product that composes it — see the
[Handl platform](/handl/).
