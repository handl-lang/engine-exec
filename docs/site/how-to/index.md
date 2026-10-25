# How-to guides

Task recipes. Each assumes sibling checkouts of this repo and of the platform
repos it patches.

## Implement an eval engine

Implement `EvalEngine` and register it under the kit's `eval` operation slot. The shipped engines are thin: they reuse this crate's dispatch and differ only in how they walk or execute the scheme.

## Pattern-match deep values safely

Use `pattern_match::try_pattern`; it grows the stack at `STACK_RED_ZONE` instead of overflowing, so deeply nested values are bounded by total growable stack rather than the platform stack.
