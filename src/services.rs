//! Ambient eval services shared by every exec backend.
//!
//! An arrow is self-contained (`body_def` + `binds` + `handlers`, ADR-0106);
//! what it needs from its host is *services*: the language that interprets its
//! schemes, the module chain for host-function dispatch, host-provided globals
//! and the import resolver. They are thread-locals because the `ValueCallable`
//! seam (host functions calling arrow values) has no engine handle.
//!
//! This module lives in the *contract* crate so no backend has to depend on
//! another backend just to install a frame.

use std::cell::RefCell;
use std::rc::Rc;

use handl_language::Language;
use handl_platform::ModuleLookup;
use handl_value::TaggedValue;
use rustc_hash::FxHashMap;

use crate::ImportResolver;

thread_local! {
    pub static CURRENT_LANG: RefCell<Option<Rc<Language>>> = const { RefCell::new(None) };
    pub static CURRENT_MODULES: RefCell<Option<Rc<dyn ModuleLookup>>> = RefCell::new(None);
    // Host-provided globals (e.g. `self`), shared via `Rc` so the
    // `TreeWalkCallable` path (host functions calling arrow-values) can
    // re-attach them to a fresh `ScopeStack` without a deep clone.
#[allow(clippy::type_complexity)]
    pub static CURRENT_GLOBALS: RefCell<Option<Rc<FxHashMap<Rc<str>, TaggedValue>>>> = const { RefCell::new(None) };
    // Import resolver for `import` native ops inside arrows that are called
    // across runtime boundaries (e.g. a `.hnl` module arrow invoked by the
    // test runner needs the module loader's resolver to re-resolve its home
    // AST's imports).
    pub static CURRENT_IMPORT_RESOLVER: RefCell<Option<Rc<dyn ImportResolver>>> = RefCell::new(None);
}

/// The ambient services an eval provides to host callbacks that apply
/// arrow values: the `Language` whose schemes interpret `body_def`
/// arenas, the module chain for host dispatch, the host-provided globals
/// (e.g. `self`), and the import resolver.
///
/// This is *not* lexical state — ADR-0106 arrows are self-contained
/// (`body_def` + `binds` + `handlers`); the frame only supplies the
/// ambient *services* a body needs to interpret its own data. A nested
/// runtime (e.g. a `.hnl` module function's bridge eval) enters a frame
/// for its own eval so `ctx.try_call_value` / `TreeWalkCallable` see the
/// innermost context.
#[derive(Clone)]
pub struct EvalFrame {
    /// The `Language` whose eval schemes interpret `body_def` arenas.
    pub lang: Rc<Language>,
    /// The module chain for host-function dispatch inside bodies.
    pub modules: Option<Rc<dyn ModuleLookup>>,
    /// The host-provided globals visible to bodies.
    pub globals: Option<Rc<FxHashMap<Rc<str>, TaggedValue>>>,
    /// The import resolver for `import` native ops inside bodies.
    pub import_resolver: Option<Rc<dyn ImportResolver>>,
}

impl EvalFrame {
    /// Install this frame in the eval thread-locals, run `f`, then restore
    /// the previous values — the same save/restore discipline as
    /// [`EvalEngine::eval`], so nested `ctx.try_call_value` /
    /// `TreeWalkCallable` calls inside the body see the innermost frame.
    ///
    /// Restoration is panic-safe: a `TlRestoreGuard` restores the previous
    /// thread-locals on drop, so an unwinding panic cannot leave this frame's
    /// language/modules/globals installed for whatever runs next on the
    /// thread (e.g. a test-runner worker that catches panics and reuses the
    /// thread for a subsequent job).
    pub fn enter<T>(&self, f: impl FnOnce() -> T) -> T {
        let _guard = TlRestoreGuard::install(
            Some(Rc::clone(&self.lang)),
            self.modules.clone(),
            self.globals.clone(),
            self.import_resolver.clone(),
        );
        f()
    }
}

/// RAII guard that restores saved eval thread-locals on drop — including
/// during panic unwinding, where a plain save/restore-after-call sequence
/// would skip restoration and leak the inner frame's context into the next
/// evaluation on the same thread.
pub struct TlRestoreGuard {
    lang: Option<Rc<Language>>,
    modules: Option<Rc<dyn ModuleLookup>>,
    globals: Option<Rc<FxHashMap<Rc<str>, TaggedValue>>>,
    import_resolver: Option<Rc<dyn ImportResolver>>,
}

impl TlRestoreGuard {
    /// Swap the frame's values into the thread-locals, returning a guard that
    /// restores the displaced values on drop.
    pub fn install(
        lang: Option<Rc<Language>>,
        modules: Option<Rc<dyn ModuleLookup>>,
        globals: Option<Rc<FxHashMap<Rc<str>, TaggedValue>>>,
        import_resolver: Option<Rc<dyn ImportResolver>>,
    ) -> Self {
        let prev_lang = CURRENT_LANG.with(|c| std::mem::replace(&mut *c.borrow_mut(), lang));
        let prev_modules =
            CURRENT_MODULES.with(|c| std::mem::replace(&mut *c.borrow_mut(), modules));
        let prev_globals =
            CURRENT_GLOBALS.with(|c| std::mem::replace(&mut *c.borrow_mut(), globals));
        let prev_import_resolver = CURRENT_IMPORT_RESOLVER
            .with(|c| std::mem::replace(&mut *c.borrow_mut(), import_resolver));
        Self {
            lang: prev_lang,
            modules: prev_modules,
            globals: prev_globals,
            import_resolver: prev_import_resolver,
        }
    }

    /// Replace the installed globals value. The value saved at `install` is
    /// still the one restored on drop.
    pub fn set_globals(&self, globals: Option<Rc<FxHashMap<Rc<str>, TaggedValue>>>) {
        CURRENT_GLOBALS.with(|c| *c.borrow_mut() = globals);
    }
}

impl Drop for TlRestoreGuard {
    fn drop(&mut self) {
        CURRENT_LANG.with(|c| *c.borrow_mut() = self.lang.take());
        CURRENT_MODULES.with(|c| *c.borrow_mut() = self.modules.take());
        CURRENT_GLOBALS.with(|c| *c.borrow_mut() = self.globals.take());
        CURRENT_IMPORT_RESOLVER.with(|c| *c.borrow_mut() = self.import_resolver.take());
    }
}
