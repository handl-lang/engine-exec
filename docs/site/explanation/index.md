# Explanation

## Why this crate exists

Splitting eval into *abstraction* (`engine-exec`) and *engines*
(`…-tree-walk`, `…-bytecode`) is what makes the backend swappable: a kit registers
an engine under one operation name and the same language definition runs on the
interpreter or the VM. The abstraction owns the parts that must not diverge —
handler/journal keys, native dispatch, pattern matching — so conformance between
backends is structural rather than coincidental.

## Where it sits

This crate belongs to the `engine-*` family. Plugins, kits and hosts reach it
through the kit's operation slot, so the engine can be swapped without touching a
language definition.

See the [reference](/engine-exec/reference/) for the API and the
[how-to guides](/engine-exec/how-to/) for tasks.
