//! The `io` module — the `IO<sig>` effect former's surface ops
//! (ADR-0153 §4).
//!
//! `perform` is the op-emission primitive: `perform(op, arg)` mints the
//! `IO` former object `{"__former__": <type.io sym>, "indices":
//! [{op: arg}]}` — exactly the shape [`native_ops::interpret_io_former`]
//! dispatches under `handle <body> with <handler>`: each sig entry's op
//! name routes to the handler's matching impl with the sig entry's value
//! as the operand, and the entry is replaced by the impl's result.
//!
//! `perform` is a *host function*, not a `Term::Op`: the surface form is
//! an ordinary `call` (`perform(puts, "hi")`), whose callee resolves the
//! same way `sym.seeded`/`type.instanceof` do — the unbound `var`
//! evaluates to its name string and `call_host` reaches this module
//! through [`handl_platform::ModuleLookup`] (flat name for `perform`,
//! qualified `io.perform` through the module-name split). Every eval
//! engine reaches it through that single path — no per-backend arm, no
//! grammar or eval-scheme change.
//!
//! Op-name operand spellings:
//!
//! - `perform(puts, "hi")` — a bare identifier. Unbound vars evaluate to
//!   their name string (`proj_env`'s unbound-to-name rule), so `puts`
//!   arrives as `"puts"`. (A `puts` binding bound to a different value
//!   passes that value through instead — bind op names to their own name
//!   or spell them as strings.)
//! - `perform("puts", "hi")` — the explicit string spelling.
//! - `perform(sym("puts"), "hi")` / `perform(sym.seeded("puts"), "hi")`
//!   — a nominal `{"__sym__": …}` operand; the sym's display name is the
//!   op name.
//!
//! The mint is *interpret-in-place* data: a `perform` value with no
//! covering handler stays an `IO` object for an outer handler or user
//! destructuring. Each `perform` mints a fresh single-entry sig —
//! `do { perform(puts, "a"); perform(puts, "b") }` yields two `IO`
//! values, not a merged `{puts: …}`; sig merging is a `handle`-time
//! concern the v1 surface deliberately does not model.

use handl_platform::{EvalContext, EvalError, Function, Module};
use handl_value::{Sym, TaggedValue, tagged_to_value};
use serde_json::{Map, Value, json};

/// `perform(op, arg)` — mint an `IO` former object carrying the single
/// op entry `{op: arg}` (see the module docs). `arg` defaults to `null`
/// (`perform(op)` mints `{op: null}`).
#[derive(Clone, Copy, Debug)]
pub struct PerformFn;

impl Function for PerformFn {
    fn call(&self, args: &[Value], _ctx: &mut EvalContext) -> Result<Value, EvalError> {
        let op_val = args.first().ok_or_else(|| {
            EvalError::Other(format!(
                "perform: expected 1–2 arguments (op, arg?), got {}",
                args.len()
            ))
        })?;
        let op = match op_val {
            Value::String(s) => s.clone(),
            other => match TaggedValue::from(other.clone()) {
                TaggedValue::Sym(s) => s.name().to_string(),
                _ => {
                    return Err(EvalError::TypeMismatch(
                        "perform: op must be an op name (a bare identifier or string) \
                         or a sym value"
                            .to_string(),
                    ));
                }
            },
        };
        if args.len() > 2 {
            return Err(EvalError::Other(format!(
                "perform: expected 1–2 arguments (op, arg?), got {}",
                args.len()
            )));
        }
        let arg = args.get(1).cloned().unwrap_or(Value::Null);
        // The runtime `IO(sig)` spelling `interpret_io_former` reads:
        // `__former__` is the seeded `type.io` nominal (a `__sym__`
        // encoding — reserved-key minting is a host capability), the sig
        // record rides in `indices[0]` with the op name as the field key
        // and the operand as the field value.
        let mut sig = Map::new();
        sig.insert(op, arg);
        Ok(json!({
            "__former__": tagged_to_value(&TaggedValue::Sym(Sym::seeded("type.io"))),
            "indices": [Value::Object(sig)],
        }))
    }
}

/// Name of the module: `io.perform(puts, "hi")` — and the flat
/// `perform(puts, "hi")` spelling through `ModuleLookup`'s all-module
/// function search (mirroring `sym`'s `sym`/`seeded` aliases).
pub const IO_MODULE_NAME: &str = "io";

/// The `io` module: the `IO<sig>` former's surface ops. A `Send + Sync`
/// shell over stateless [`Function`] impls (the same shape
/// `vm_primitives::SymModule` uses for the `sym` floor natives).
pub struct IoModule {
    perform_fn: PerformFn,
}

impl IoModule {
    /// Create a fresh `io` module.
    pub fn new() -> Self {
        IoModule {
            perform_fn: PerformFn,
        }
    }
}

impl Default for IoModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for IoModule {
    fn name(&self) -> &str {
        IO_MODULE_NAME
    }

    fn function(&self, name: &str) -> Option<&dyn Function> {
        match name {
            "perform" => Some(&self.perform_fn as &dyn Function),
            _ => None,
        }
    }

    fn functions(&self) -> Vec<(&str, &dyn Function)> {
        vec![("perform", &self.perform_fn as &dyn Function)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::result_large_err)]
    fn call(module: &IoModule, name: &str, args: &[Value]) -> Result<Value, EvalError> {
        module
            .function(name)
            .expect("function bound")
            .call(args, &mut EvalContext::new())
    }

    #[test]
    fn perform_mints_io_former_with_sig_entry() {
        let m = IoModule::new();
        let v = call(&m, "perform", &[json!("puts"), json!("hi")]).unwrap();
        // The minted sig must round-trip into the exact TaggedValue shape
        // `interpret_io_former` recognizes: a `Sym` `__former__` whose
        // handle is `type.io`'s seeded handle, and `indices[0]` carrying
        // `{puts: "hi"}`.
        let tv = TaggedValue::from(v);
        let TaggedValue::Obj(map) = &tv else {
            panic!("perform should mint an object, got {tv:?}");
        };
        let Some(TaggedValue::Sym(s)) = map.get("__former__") else {
            panic!("__former__ should decode to a Sym, got {map:?}");
        };
        assert_eq!(s.handle(), Sym::seeded("type.io").handle());
        let Some(TaggedValue::Arr(indices)) = map.get("indices") else {
            panic!("indices should be an array");
        };
        let Some(TaggedValue::Obj(sig)) = indices.first() else {
            panic!("indices[0] should be the sig record");
        };
        assert_eq!(sig.get("puts"), Some(&TaggedValue::from(json!("hi"))));
    }

    #[test]
    fn perform_bare_op_arg_defaults_null() {
        let m = IoModule::new();
        let v = call(&m, "perform", &[json!("fs")]).unwrap();
        assert_eq!(v["indices"][0]["fs"], Value::Null);
    }

    #[test]
    fn perform_sym_op_uses_sym_name() {
        let m = IoModule::new();
        let sym_json = tagged_to_value(&TaggedValue::Sym(Sym::seeded("puts")));
        let v = call(&m, "perform", &[sym_json, json!(42)]).unwrap();
        assert_eq!(v["indices"][0]["puts"], json!(42));
    }

    #[test]
    fn perform_rejects_bad_op_operand() {
        let m = IoModule::new();
        assert!(call(&m, "perform", &[json!(7), json!("x")]).is_err());
        assert!(call(&m, "perform", &[json!({}), json!("x")]).is_err());
        assert!(call(&m, "perform", &[]).is_err());
        assert!(call(&m, "perform", &[json!("a"), json!(1), json!(2)]).is_err());
    }
}
