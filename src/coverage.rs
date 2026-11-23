//! Source-coverage instrumentation for `.hnl` evaluation (opt-in).
//!
//! When a [`CoverageSession`] is installed on the evaluating thread, the
//! tree-walk engine records which source lines and top-level declarations
//! were executed:
//!
//! - **Line hits** — [`record_node_span`] is called from
//!   `eval_node_inner_unguarded` for every evaluated AST node that carries
//!   a [`handl_language::Span`]. A hit attributes to the line containing
//!   `span.start` of the *innermost active source* (see
//!   [`CoverageSession::push_source`]).
//! - **Declaration hits** — each [`CoverageSource`] carries the byte
//!   ranges of its top-level binding statements ([`scan_decls`]). A hit
//!   whose span start falls inside a declaration's range counts toward
//!   it; hits recorded while evaluating an arrow *body*
//!   ([`CoverageSession::push_arrow_body`]) count as *call* hits, which is
//!   what marks an arrow-valued declaration "covered" — minting a closure
//!   at module load is definition, not execution.
//! - **Arrow provenance** — `apply_arrow_body` resolves which source an
//!   arrow's `body_def` arena belongs to through a mint-time table
//!   ([`CoverageSession::note_arrow_body`], recorded by the `closure`
//!   op). This keeps hits attributed correctly when an arrow minted in
//!   module A runs while module B's eval is active.
//!
//! # Coordinate space
//!
//! Spans index the *prepared* source text — the exact string that was
//! lexed — which for canonical `.hnl` modules is the output of
//! `handl_runtime::module_prep::prepare_module` (and, for
//! `plugin-lib-handl`, the `analyze_source` transform plus a `(`…`)`
//! wrapper), **not** the file on disk. Both transforms re-serialize the
//! source.
//!
//! Producers that re-serialize can attach a provenance map —
//! [`CoverageSource::line_map`], one entry per prepared line giving the
//! 1-based origin-file line (`None` for generated lines) — built with
//! [`OriginLineMap`]. When present, [`CoverageReport`] resolves hits to
//! file lines (`fileLineHits` in JSON, `file_line` on declarations) in
//! addition to the prepared coordinates, which remain the canonical
//! span space. `origin` carries the on-disk path for reference.
//!
//! # Threading
//!
//! Coverage state is thread-local (like the eval services in
//! [`crate::services`]): each scheduler worker installs the shared
//! session via [`CoverageSession::install_local`]. Hit data lives behind
//! one `Arc<Mutex<…>>` so parallel workers merge into a single report;
//! arrow-provenance and the source stack stay per-thread because arrow
//! values never cross threads.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use rustc_hash::FxHashMap;

/// A top-level binding declaration discovered in a coverage source.
///
/// `start..end` are byte offsets into [`CoverageSource::text`] covering
/// the whole top-level statement. `is_arrow` is a heuristic (the
/// statement text contains a `~>` graded-arrow form) used only to decide
/// whether *call* hits (arrow-body execution) or *any* hits mark the
/// declaration covered.
#[derive(Debug, Clone)]
pub struct CoverageDecl {
    /// The bound name (`export` stripped; `"…"` quoted for non-ident
    /// names is preserved verbatim).
    pub name: String,
    /// Byte offset of the statement's first non-whitespace char.
    pub start: usize,
    /// Byte offset one past the statement's last char.
    pub end: usize,
    /// Whether the declaration's RHS looks like an arrow (`~>` present).
    pub is_arrow: bool,
}

/// A registered coverage source: the prepared text spans index into,
/// plus its top-level declarations.
#[derive(Debug, Clone)]
pub struct CoverageSource {
    /// Display name — a resolved file path or `module.function` style
    /// module name.
    pub name: String,
    /// The exact text AST spans index into (prepared source).
    pub text: String,
    /// Top-level declarations ([`scan_decls`]).
    pub decls: Vec<CoverageDecl>,
    /// The on-disk file this source was prepared from, when known.
    pub origin: Option<String>,
    /// Free-form coordinate-space label, e.g. `"prepared"` — surfaced in
    /// reports so consumers know line numbers are not file lines.
    pub coordinate_space: String,
    /// Prepared-line → origin-file-line provenance (1-based lines on
    /// both sides; `None` entries are generated lines with no file
    /// origin). Index `i` describes prepared line `i + 1`; lines past
    /// the map's end are unmapped. `None` when the producer cannot
    /// prove provenance.
    pub line_map: Option<Vec<Option<u32>>>,
    /// Total lines in the origin file, when known — the denominator for
    /// file-line coverage views.
    pub origin_lines: Option<u32>,
}

impl CoverageSource {
    /// A source for `name`/`text` with declarations auto-scanned and
    /// `coordinate_space` defaulting to `"prepared"`.
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            decls: scan_decls(&text),
            name: name.into(),
            text,
            origin: None,
            coordinate_space: "prepared".to_string(),
            line_map: None,
            origin_lines: None,
        }
    }

    /// Attach the on-disk origin path.
    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }

    /// Attach a prepared-line → origin-file-line map (see
    /// [`OriginLineMap`]) plus the origin file's total line count.
    pub fn with_file_lines(
        mut self,
        line_map: Vec<Option<u32>>,
        origin_lines: Option<u32>,
    ) -> Self {
        self.line_map = Some(line_map);
        self.origin_lines = origin_lines;
        self
    }
}

/// Builder for a [`CoverageSource::line_map`]: records, for every
/// prepared-source line *started*, the 1-based origin-file line it
/// derives from (`None` for generated/synthetic lines).
///
/// Producers append output text piecewise — [`push`](Self::push) takes
/// the origin line of the piece's *first* line and assumes the piece's
/// interior lines are contiguous in the origin (true for verbatim
/// slices); [`push_lines`](Self::push_lines) takes an explicit per-line
/// origin slice for re-ordered content (e.g. a hoisted preamble). Line
/// boundaries are `\n`; a piece may start mid-line (it then continues
/// the previous prepared line and contributes no map entry until its
/// first interior newline).
#[derive(Debug, Clone, Default)]
pub struct OriginLineMap {
    /// One entry per prepared line started so far.
    lines: Vec<Option<u32>>,
    /// Whether the next emitted char starts a new prepared line.
    at_line_start: bool,
}

impl OriginLineMap {
    /// An empty map — the first pushed char starts prepared line 1.
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            at_line_start: true,
        }
    }

    /// Append `text`; `first` is the 1-based origin line of `text`'s
    /// first line (`None` = generated). `text`'s k-th interior line
    /// (0-based, split on `\n`) maps to `first + k`.
    pub fn push(&mut self, text: &str, first: Option<u32>) {
        let mut line = first;
        for ch in text.chars() {
            if self.at_line_start {
                self.lines.push(line);
                self.at_line_start = false;
            }
            if ch == '\n' {
                self.at_line_start = true;
                line = line.map(|l| l.saturating_add(1));
            }
        }
    }

    /// Append `text` whose per-line origins are given explicitly:
    /// `origins[i]` is the origin of `text`'s i-th line (0-based, split
    /// on `\n`); missing entries map to `None`.
    pub fn push_lines(&mut self, text: &str, origins: &[Option<u32>]) {
        let mut idx = 0usize;
        for ch in text.chars() {
            if self.at_line_start {
                self.lines.push(origins.get(idx).copied().flatten());
                self.at_line_start = false;
            }
            if ch == '\n' {
                self.at_line_start = true;
                idx += 1;
            }
        }
    }

    /// The recorded map: index `i` → origin of prepared line `i + 1`.
    pub fn into_vec(self) -> Vec<Option<u32>> {
        self.lines
    }

    /// Number of prepared lines started so far.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether no prepared line has been started yet.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Per-declaration hit counters.
#[derive(Debug, Default)]
struct DeclHits {
    /// Hits while *not* inside an arrow body (definition-time eval).
    any: u64,
    /// Hits while inside an arrow body (the declaration's code ran).
    body: u64,
}

/// Mutable per-source hit state (behind the session mutex).
struct SourceState {
    spec: CoverageSource,
    /// Byte offset of each line start (`line_starts[0] == 0`).
    line_starts: Vec<usize>,
    /// Prepared-line (1-based) → evaluation hit count.
    line_hits: FxHashMap<u32, u64>,
    /// Parallel to `spec.decls`.
    decl_hits: Vec<DeclHits>,
    /// Arrow applications whose `body_def` resolved to this source.
    applications: u64,
}

impl SourceState {
    fn new(spec: CoverageSource) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in spec.text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        let decl_hits = spec.decls.iter().map(|_| DeclHits::default()).collect();
        Self {
            spec,
            line_starts,
            line_hits: FxHashMap::default(),
            decl_hits,
            applications: 0,
        }
    }

    /// 1-based prepared line containing byte `offset` (`text.len()` clamps
    /// to the last line).
    fn line_of(&self, offset: usize) -> u32 {
        let offset = offset.min(self.spec.text.len());
        let idx = self
            .line_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        (idx + 1) as u32
    }

    /// Index of the declaration whose `[start, end)` contains `offset`,
    /// or `None`. Decls are disjoint top-level statements, so a linear
    /// scan is fine (declaration counts are small).
    fn decl_index_at(&self, offset: usize) -> Option<usize> {
        self.spec
            .decls
            .iter()
            .position(|d| offset >= d.start && offset < d.end)
    }
}

/// Shared hit/source state — `Send` so scheduler workers merge into one
/// report. Locked per recorded hit; coverage runs are opt-in and eval
/// does far more work per node than a mutex lock.
#[derive(Default)]
struct CoverageShared {
    sources: Vec<SourceState>,
}

/// What the per-thread source stack top means for attribution.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StackEntry {
    /// Hits attribute to this source id; `in_body` marks hits recorded
    /// while evaluating an arrow `body_def` (declaration *call* hits).
    Source { id: usize, in_body: bool },
    /// Synthetic/evaluator-generated code (e.g. the `__mod.f(__arg_0)`
    /// applied-form wrapper): hits are dropped rather than misattributed.
    Opaque,
}

/// Per-thread coverage state.
struct CoverageTls {
    shared: Arc<Mutex<CoverageShared>>,
    /// Active source stack — innermost last. Mirrors the save/restore
    /// discipline of `EvalFrame`/`TlRestoreGuard`. `RefCell` because the
    /// `Rc<CoverageTls>` is shared between the TLS slot and live guards.
    stack: RefCell<Vec<StackEntry>>,
    /// Mint-time arrow provenance: `Rc::as_ptr(body_def)` (as usize) →
    /// source id, recorded by the `closure` op on this thread. Arrow
    /// values never cross threads, so a per-thread map is exact — no
    /// cross-thread pointer-reuse hazard.
    body_sources: RefCell<FxHashMap<usize, usize>>,
}

thread_local! {
    static CURRENT_COVERAGE: RefCell<Option<Rc<CoverageTls>>> = const { RefCell::new(None) };
}

/// A coverage run: the shared sink plus per-thread install handles.
///
/// Cheap to clone (`Arc` inside). Install on every thread that evaluates
/// `.hnl` code via [`install_local`](Self::install_local); collect the
/// merged [`CoverageReport`] via [`report`](Self::report).
#[derive(Clone)]
pub struct CoverageSession {
    shared: Arc<Mutex<CoverageShared>>,
}

impl CoverageSession {
    /// Start a new empty coverage session.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(CoverageShared::default())),
        }
    }

    /// Install this session's TLS on the *calling* thread.
    ///
    /// Idempotent per session: scheduler workers are reused across jobs
    /// and module/arrow caches persist between them, so the per-thread
    /// arrow-provenance map must survive re-install — reinstalling the
    /// *same* session keeps the existing `CoverageTls` (and its
    /// `body_sources`). Installing a *different* session replaces the
    /// state wholesale: arrows minted under the old session's source ids
    /// simply go unmapped (honest undercount, never misattribution).
    pub fn install_local(&self) {
        CURRENT_COVERAGE.with(|c| {
            let mut slot = c.borrow_mut();
            if let Some(existing) = slot.as_ref()
                && Arc::ptr_eq(&existing.shared, &self.shared)
            {
                return;
            }
            *slot = Some(Rc::new(CoverageTls {
                shared: Arc::clone(&self.shared),
                stack: RefCell::new(Vec::new()),
                body_sources: RefCell::new(FxHashMap::default()),
            }));
        });
    }

    /// Remove the coverage TLS from the calling thread.
    pub fn uninstall_local() {
        CURRENT_COVERAGE.with(|c| *c.borrow_mut() = None);
    }

    /// Whether a session is installed on the calling thread.
    pub fn is_active_local() -> bool {
        CURRENT_COVERAGE.with(|c| c.borrow().is_some())
    }

    /// Register `source` and return its id — or `None` when no session is
    /// installed (the common case: zero-cost when coverage is off).
    ///
    /// Dedup is by `(name, text)`: the same module evaluated on several
    /// scheduler workers registers once.
    pub fn register(source: CoverageSource) -> Option<usize> {
        CURRENT_COVERAGE.with(|c| {
            let tls = c.borrow();
            let tls = tls.as_ref()?;
            let mut shared = tls.shared.lock().ok()?;
            if let Some(i) = shared
                .sources
                .iter()
                .position(|s| s.spec.name == source.name && s.spec.text == source.text)
            {
                return Some(i);
            }
            shared.sources.push(SourceState::new(source));
            Some(shared.sources.len() - 1)
        })
    }

    /// Push `id` as the active coverage source until the returned guard
    /// drops. `None` when coverage is inactive or `id` is stale.
    pub fn push_source(id: usize) -> Option<CoverageGuard> {
        CURRENT_COVERAGE.with(|c| {
            let mut tls_ref = c.borrow_mut();
            let tls = tls_ref.as_mut()?;
            // `id` must name a live source in the shared table.
            if id >= tls.shared.lock().ok()?.sources.len() {
                return None;
            }
            tls.stack
                .borrow_mut()
                .push(StackEntry::Source { id, in_body: false });
            Some(CoverageGuard {
                tls: Rc::downgrade(tls),
                entry: StackEntry::Source { id, in_body: false },
            })
        })
    }

    /// Push an *arrow-body* source frame: hits recorded inside count as
    /// declaration *call* hits for [`SourceState::decl_hits`]. Used by
    /// `apply_arrow_body` once the arrow's `body_def` provenance
    /// resolves.
    #[doc(hidden)]
    pub fn push_arrow_source(id: usize) -> Option<CoverageGuard> {
        CURRENT_COVERAGE.with(|c| {
            let mut tls_ref = c.borrow_mut();
            let tls = tls_ref.as_mut()?;
            if id >= tls.shared.lock().ok()?.sources.len() {
                return None;
            }
            tls.stack
                .borrow_mut()
                .push(StackEntry::Source { id, in_body: true });
            Some(CoverageGuard {
                tls: Rc::downgrade(tls),
                entry: StackEntry::Source { id, in_body: true },
            })
        })
    }

    /// Push an opaque frame: hits inside are dropped (synthetic
    /// evaluator-generated code whose spans do not index any registered
    /// source).
    pub fn push_opaque() -> Option<CoverageGuard> {
        CURRENT_COVERAGE.with(|c| {
            let mut tls_ref = c.borrow_mut();
            let tls = tls_ref.as_mut()?;
            tls.stack.borrow_mut().push(StackEntry::Opaque);
            Some(CoverageGuard {
                tls: Rc::downgrade(tls),
                entry: StackEntry::Opaque,
            })
        })
    }

    /// Record `Rc::as_ptr(body_def)` → the currently active source, so a
    /// later [`source_for_body`] resolves which source an arrow's body
    /// arena spans index into. Called by the `closure` op at mint time.
    /// No-op when coverage is inactive.
    #[doc(hidden)]
    pub fn note_arrow_body(body_def_ptr: usize) {
        CURRENT_COVERAGE.with(|c| {
            let mut tls_ref = c.borrow_mut();
            let Some(tls) = tls_ref.as_mut() else {
                return;
            };
            // Innermost *source* entry wins; opaque frames contribute
            // nothing (a closure minted inside synthetic code has no
            // honest source).
            let top_source = tls.stack.borrow().iter().rev().find_map(|e| match e {
                StackEntry::Source { id, .. } => Some(*id),
                StackEntry::Opaque => None,
            });
            if let Some(id) = top_source {
                tls.body_sources.borrow_mut().insert(body_def_ptr, id);
            }
        });
    }

    /// Resolve the source a minted arrow's `body_def` belongs to.
    #[doc(hidden)]
    pub fn source_for_body(body_def_ptr: usize) -> Option<usize> {
        CURRENT_COVERAGE.with(|c| {
            c.borrow()
                .as_ref()?
                .body_sources
                .borrow()
                .get(&body_def_ptr)
                .copied()
        })
    }

    /// Snapshot the merged report. Locks the shared table once.
    pub fn report(&self) -> CoverageReport {
        let shared = self.shared.lock().expect("coverage mutex poisoned");
        CoverageReport {
            sources: shared
                .sources
                .iter()
                .map(|s| {
                    // Resolve prepared-line hits to origin-file lines
                    // when the source carries a provenance map. Several
                    // prepared lines can map to the same file line
                    // (e.g. a `name :=` wrapper sharing the decl line);
                    // counts merge.
                    let file_line_hits = s.spec.line_map.as_ref().map(|map| {
                        let mut acc: std::collections::BTreeMap<u32, u64> =
                            std::collections::BTreeMap::new();
                        for (&prepared, &n) in &s.line_hits {
                            if let Some(Some(orig)) =
                                map.get(prepared.saturating_sub(1) as usize)
                            {
                                *acc.entry(*orig).or_insert(0) += n;
                            }
                        }
                        acc.into_iter().collect()
                    });
                    SourceReport {
                        name: s.spec.name.clone(),
                        origin: s.spec.origin.clone(),
                        coordinate_space: s.spec.coordinate_space.clone(),
                        line_map: s.spec.line_map.clone(),
                        origin_lines: s.spec.origin_lines,
                        file_line_hits,
                        total_lines: s.spec.text.lines().filter(|l| !l.trim().is_empty()).count()
                            as u64,
                        line_hits: s.line_hits.iter().map(|(&l, &n)| (l, n)).collect(),
                        decls: s
                            .spec
                            .decls
                            .iter()
                            .zip(s.decl_hits.iter())
                            .map(|(d, h)| {
                                // The declaration's origin-file line:
                                // prepared line of `d.start` through the
                                // source's provenance map.
                                let file_line = s.spec.line_map.as_ref().and_then(|map| {
                                    let pl = s.line_of(d.start);
                                    map.get(pl.saturating_sub(1) as usize)
                                        .copied()
                                        .flatten()
                                });
                                DeclReport {
                                    name: d.name.clone(),
                                    start: d.start,
                                    end: d.end,
                                    is_arrow: d.is_arrow,
                                    any_hits: h.any,
                                    body_hits: h.body,
                                    file_line,
                                }
                            })
                            .collect(),
                        applications: s.applications,
                    }
                })
                .collect(),
        }
    }
}

impl Default for CoverageSession {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard restoring the coverage source stack on drop (panic-safe,
/// mirroring `TlRestoreGuard`).
pub struct CoverageGuard {
    tls: std::rc::Weak<CoverageTls>,
    entry: StackEntry,
}

impl Drop for CoverageGuard {
    fn drop(&mut self) {
        if let Some(tls) = self.tls.upgrade() {
            // Only pop when this guard's entry is still on top — a
            // re-installed session or out-of-order drop must not corrupt
            // the stack.
            if tls.stack.borrow().last() == Some(&self.entry) {
                tls.stack.borrow_mut().pop();
            }
        }
    }
}

/// Record one evaluated-node hit. Called from the tree-walk engine's
/// `eval_node_inner_unguarded`; a no-op (one TLS check) when no session
/// is installed or the top frame is [`StackEntry::Opaque`].
///
/// `span` is the node's byte range in the *prepared* source of the
/// innermost active source; only `span.start`'s line is counted (a
/// multi-line node — e.g. a whole `~>` arrow — must not mark every line
/// it touches as covered at definition time).
#[doc(hidden)]
pub fn record_node_span(span: Option<(usize, usize)>) {
    let Some((start, _end)) = span else {
        return;
    };
    CURRENT_COVERAGE.with(|c| {
        let tls_ref = c.borrow();
        let Some(tls) = tls_ref.as_ref() else {
            return;
        };
        let stack = tls.stack.borrow();
        let Some(&StackEntry::Source { id, in_body }) = stack.last() else {
            return;
        };
        let Ok(mut shared) = tls.shared.lock() else {
            return;
        };
        let Some(src) = shared.sources.get_mut(id) else {
            return;
        };
        // Spans must index inside the registered text — synthetic
        // wrappers (`({src}).name` projections, `__mod.f(…)` applied
        // forms) can produce offsets past the end; those are not honest
        // hits.
        if start >= src.spec.text.len() {
            return;
        }
        let line = src.line_of(start);
        *src.line_hits.entry(line).or_insert(0) += 1;
        if let Some(di) = src.decl_index_at(start) {
            let hits = &mut src.decl_hits[di];
            hits.any += 1;
            if in_body {
                hits.body += 1;
            }
        }
    });
}

/// Count one arrow application against the source its `body_def`
/// resolved to (see [`CoverageSession::source_for_body`]).
#[doc(hidden)]
pub fn record_arrow_application(source_id: usize) {
    CURRENT_COVERAGE.with(|c| {
        let tls_ref = c.borrow();
        let Some(tls) = tls_ref.as_ref() else {
            return;
        };
        let Ok(mut shared) = tls.shared.lock() else {
            return;
        };
        if let Some(src) = shared.sources.get_mut(source_id) {
            src.applications += 1;
        }
    });
}

/// Scan `src` for top-level binding declarations — a coverage-side port
/// of `handl_runtime::module_prep::top_level_segments` that keeps byte
/// offsets. Statement boundaries follow the same rules: `;` or a
/// non-indented newline at bracket depth 0, or an `export`/`import`/
/// `from` keyword at depth 0 mid-statement; strings and comments are
/// tracked.
///
/// A segment becomes a [`CoverageDecl`] when, after stripping a leading
/// `export`, it contains a binding separator (`::`, `:=`, `=`, or a
/// lone `:`) at depth 0 and the text before it is a plausible name.
/// `import`/`from` statements and bare expressions produce no decl.
pub fn scan_decls(src: &str) -> Vec<CoverageDecl> {
    let mut decls = Vec::new();
    scan_decls_in(src, 0, 0, &mut decls);
    decls
}

/// Recursive worker for [`scan_decls`]: `seg` is `src[base..]` sliced at
/// `seg_start` — declarations inside a top-level `do { … }` block (the
/// shape module prep emits) are found by recursing into the block's
/// interior with adjusted byte offsets. Depth-capped against pathological
/// nesting.
fn scan_decls_in(seg: &str, seg_start: usize, depth: usize, out: &mut Vec<CoverageDecl>) {
    if depth > 8 {
        return;
    }
    // A segment that is exactly a `do { … }` block hides its statements
    // one bracket level down — descend. Returned offsets are relative to
    // `seg` (leading whitespace included).
    if let Some((inner_off, inner_len)) = do_block_inner(seg) {
        let inner = &seg[inner_off..inner_off + inner_len];
        for (s, e) in top_level_segment_spans(inner) {
            scan_decls_in(&inner[s..e], seg_start + inner_off + s, depth + 1, out);
        }
        return;
    }
    for (s, e) in top_level_segment_spans(seg) {
        let stmt = &seg[s..e];
        let stmt_trimmed = stmt.trim();
        if stmt_trimmed.is_empty() || is_import_stmt(stmt_trimmed) {
            continue;
        }
        // Declarations start with a name (after an optional `export`);
        // record/object literals (`{ k: v }` — e.g. the export projection
        // module prep appends), arrays, strings, and parenthesized
        // expressions are not declarations.
        let first = stmt_trimmed.as_bytes()[0];
        if !(first.is_ascii_alphabetic() || first == b'_') {
            continue;
        }
        let body = match stmt_trimmed.strip_prefix("export") {
            Some(rest)
                if rest.is_empty()
                    || !(rest.as_bytes()[0].is_ascii_alphanumeric()
                        || rest.as_bytes()[0] == b'_') =>
            {
                rest.trim_start()
            }
            _ => stmt_trimmed,
        };
        let Some((sep_pos, _sep_len)) = find_binding_sep(body) else {
            continue;
        };
        let name = strip_decl_name(&body[..sep_pos]);
        if name.is_empty() {
            continue;
        }
        out.push(CoverageDecl {
            name,
            start: seg_start + s + (stmt.len() - stmt.trim_start().len()),
            end: seg_start + e,
            is_arrow: body.contains("~>"),
        });
    }
}

/// When `seg` (already known non-empty) is a `do { … }` block whose
/// closing brace is its last non-whitespace char, return the byte range
/// of the block's *interior* (after `{`, before the matching `}`).
fn do_block_inner(seg: &str) -> Option<(usize, usize)> {
    let bytes = seg.as_bytes();
    // Skip leading whitespace — segments can begin with the newline/`;`
    // boundary that ended the previous statement.
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    // Only an explicit `do { … }` block hides statements — a bare `{ … }`
    // segment is a record/object literal whose fields are not declarations.
    if !bytes[i..].starts_with(b"do") {
        return None;
    }
    let next = bytes.get(i + 2).copied();
    if next.is_none() || !(next == Some(b' ') || next == Some(b'\t') || next == Some(b'{')) {
        return None;
    }
    i += 2;
    // Find the opening `{`.
    while i < bytes.len() && bytes[i] != b'{' {
        if !bytes[i].is_ascii_whitespace() {
            return None;
        }
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let open = i;
    // Match braces, tracking strings/comments.
    let mut depth = 0i32;
    let mut in_str: Option<u8> = None;
    let mut in_line = false;
    let mut in_block = false;
    let mut j = open;
    let mut close = None;
    while j < bytes.len() {
        let c = bytes[j];
        if in_line {
            if c == b'\n' {
                in_line = false;
            }
            j += 1;
            continue;
        }
        if in_block {
            if c == b'-' && bytes.get(j + 1) == Some(&b'-') && bytes.get(j + 2) == Some(&b'/') {
                in_block = false;
                j += 3;
            } else {
                j += 1;
            }
            continue;
        }
        if let Some(q) = in_str {
            if c == b'\\' {
                j += 1;
            } else if c == q {
                in_str = None;
            }
            j += 1;
            continue;
        }
        if c == b'-' && bytes.get(j + 1) == Some(&b'-') {
            in_line = true;
            j += 2;
            continue;
        }
        if c == b'/' && bytes.get(j + 1) == Some(&b'-') && bytes.get(j + 2) == Some(&b'-') {
            in_block = true;
            j += 3;
            continue;
        }
        match c {
            b'"' | b'\'' => in_str = Some(c),
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(j);
                    break;
                }
            }
            _ => {}
        }
        j += 1;
    }
    let close = close?;
    // The block must end the segment (trailing whitespace allowed).
    if !seg[close + 1..].trim().is_empty() {
        return None;
    }
    Some((open + 1, close - open - 1))
}

/// Byte spans of top-level statements — see [`scan_decls`].
fn top_level_segment_spans(src: &str) -> Vec<(usize, usize)> {
    #[derive(Clone, Copy, PartialEq)]
    enum Lead {
        Pending,
        From,
        Import,
        Export,
        Other,
    }

    let bytes = src.as_bytes();
    let mut segs: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut depth: i32 = 0;
    let mut in_str: Option<u8> = None;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut lead = Lead::Pending;
    let mut tail_seen = false;

    macro_rules! cut {
        ($end:expr, $next:expr, $lead:expr) => {{
            let text = &src[start..$end];
            if !text.trim().is_empty() {
                segs.push((start, $end));
            }
            start = $next;
            lead = $lead;
            tail_seen = false;
        }};
    }

    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if in_line_comment {
            if c == b'\n' {
                in_line_comment = false;
            } else {
                i += 1;
                continue;
            }
        }
        if in_block_comment {
            if c == b'-' && bytes.get(i + 1) == Some(&b'-') && bytes.get(i + 2) == Some(&b'/') {
                in_block_comment = false;
                i += 3;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(q) = in_str {
            if c == b'\\' {
                i += 1;
            } else if c == q {
                in_str = None;
            }
            if lead == Lead::Pending {
                lead = Lead::Other;
            }
            i += 1;
            continue;
        }
        if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            in_line_comment = true;
            i += 2;
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'-') && bytes.get(i + 2) == Some(&b'-') {
            in_block_comment = true;
            if lead == Lead::Pending {
                lead = Lead::Other;
            }
            i += 3;
            continue;
        }
        match c {
            b'"' | b'\'' => {
                in_str = Some(c);
                if lead == Lead::Pending {
                    lead = Lead::Other;
                }
                i += 1;
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                if lead == Lead::Pending {
                    lead = Lead::Other;
                }
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth -= 1;
                if lead == Lead::Pending {
                    lead = Lead::Other;
                }
                i += 1;
            }
            b';' if depth == 0 => {
                cut!(i, i + 1, Lead::Pending);
                i += 1;
            }
            b'\n' if depth == 0 => {
                let next = bytes.get(i + 1).copied();
                if next != Some(b' ') && next != Some(b'\t') {
                    cut!(i, i + 1, Lead::Pending);
                }
                i += 1;
            }
            _ if depth == 0 && (c.is_ascii_alphabetic() || c == b'_') => {
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                let word = &src[i..j];
                let dotted = i > 0 && bytes[i - 1] == b'.';
                let kw = if !dotted && (word == "export" || word == "import" || word == "from") {
                    Some(word)
                } else {
                    None
                };
                match (lead, kw) {
                    (Lead::Pending, Some("export")) => lead = Lead::Export,
                    (Lead::Pending, Some("from")) => lead = Lead::From,
                    (Lead::Pending, Some("import")) => lead = Lead::Import,
                    (Lead::Pending, _) => lead = Lead::Other,
                    (Lead::From, Some("import" | "export")) if !tail_seen => {
                        tail_seen = true;
                    }
                    (Lead::Import, Some("from")) if !tail_seen => {
                        tail_seen = true;
                    }
                    (_, Some(k)) => {
                        let next_lead = match k {
                            "export" => Lead::Export,
                            "from" => Lead::From,
                            _ => Lead::Import,
                        };
                        cut!(i, i, next_lead);
                    }
                    (_, None) => {}
                }
                i = j;
            }
            _ => {
                if lead == Lead::Pending && !c.is_ascii_whitespace() {
                    lead = Lead::Other;
                }
                i += 1;
            }
        }
    }
    let text = &src[start..];
    if !text.trim().is_empty() {
        segs.push((start, src.len()));
    }
    segs
}

/// True when a trimmed top-level statement is an `import`/`from`
/// declaration (no coverage decl).
fn is_import_stmt(trimmed: &str) -> bool {
    for kw in ["import", "from"] {
        if let Some(rest) = trimmed.strip_prefix(kw) {
            if rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t') {
                return true;
            }
        }
    }
    false
}

/// First binding separator (`::`, `:=`, `=`, lone `:`) at bracket depth 0
/// — mirrors `module_prep::find_binding_sep`.
fn find_binding_sep(s: &str) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut depth: i32 = 0;
    let mut first_eq: Option<usize> = None;
    let mut first_colon: Option<usize> = None;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b':' if depth == 0 => {
                if i + 1 < bytes.len() && bytes[i + 1] == b':' {
                    return Some((i, 2));
                }
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    return Some((i, 2));
                }
                if first_colon.is_none() {
                    first_colon = Some(i);
                }
            }
            b'=' if depth == 0 => {
                let prev = if i > 0 { bytes[i - 1] } else { 0 };
                let next = if i + 1 < bytes.len() { bytes[i + 1] } else { 0 };
                if next == b'=' || prev == b'>' || prev == b'<' || prev == b'!' || next == b'>' {
                    i += 1;
                    continue;
                }
                if first_eq.is_none() {
                    first_eq = Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    if let Some(pos) = first_eq {
        return Some((pos, 1));
    }
    first_colon.map(|pos| (pos, 1))
}

/// The name before a binding separator: text up to the first `<`, `:` at
/// depth 0 (type params / annotations), trimmed — mirrors
/// `module_prep::strip_export_name`.
fn strip_decl_name(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut depth: i32 = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'<' | b':' if depth == 0 => return s[..i].trim().to_string(),
            _ => {}
        }
        i += 1;
    }
    s.trim().to_string()
}

// ---------------------------------------------------------------------------
// Report (owned snapshot for reporters / JSON export)
// ---------------------------------------------------------------------------

/// One declaration's coverage outcome.
#[derive(Debug, Clone)]
pub struct DeclReport {
    /// Bound name.
    pub name: String,
    /// Byte range in the prepared text.
    pub start: usize,
    /// Byte range end.
    pub end: usize,
    /// RHS looks like an arrow — coverage requires *call* hits.
    pub is_arrow: bool,
    /// Hits recorded outside arrow bodies (definition-time eval).
    pub any_hits: u64,
    /// Hits recorded inside arrow bodies.
    pub body_hits: u64,
    /// Origin-file line of the declaration's start, when the source
    /// carries a provenance map and the decl line is mapped.
    pub file_line: Option<u32>,
}

impl DeclReport {
    /// Whether the declaration counts as covered: arrow-valued decls need
    /// body (call) hits; other decls count any evaluation.
    pub fn covered(&self) -> bool {
        if self.is_arrow {
            self.body_hits > 0
        } else {
            self.any_hits > 0 || self.body_hits > 0
        }
    }
}

/// One source's coverage outcome.
#[derive(Debug, Clone)]
pub struct SourceReport {
    /// Display name.
    pub name: String,
    /// On-disk origin, when known.
    pub origin: Option<String>,
    /// Coordinate-space label (`"prepared"`).
    pub coordinate_space: String,
    /// Prepared-line → origin-file-line map, when the producer attached
    /// one ([`CoverageSource::line_map`]).
    pub line_map: Option<Vec<Option<u32>>>,
    /// Total lines in the origin file, when known.
    pub origin_lines: Option<u32>,
    /// (origin-file line, hit count) pairs resolved through `line_map`,
    /// sorted by file line. `None` when no map is attached.
    pub file_line_hits: Option<Vec<(u32, u64)>>,
    /// Non-blank lines in the prepared text (coverage denominator).
    pub total_lines: u64,
    /// (1-based prepared line, hit count) pairs, hit lines only.
    pub line_hits: Vec<(u32, u64)>,
    /// Per-declaration outcomes.
    pub decls: Vec<DeclReport>,
    /// Arrow applications resolved to this source.
    pub applications: u64,
}

impl SourceReport {
    /// Lines with ≥1 hit.
    pub fn lines_covered(&self) -> u64 {
        self.line_hits.len() as u64
    }
}

/// The merged coverage report.
#[derive(Debug, Clone, Default)]
pub struct CoverageReport {
    /// Per-source outcomes, in registration order.
    pub sources: Vec<SourceReport>,
}

impl CoverageReport {
    /// JSON rendering for `--coverage=out.json` / capability payloads.
    /// Line numbers are prepared-source coordinates — see the module docs.
    pub fn to_json(&self) -> serde_json::Value {
        let sources: Vec<serde_json::Value> = self
            .sources
            .iter()
            .map(|s| {
                let hits: serde_json::Map<String, serde_json::Value> = s
                    .line_hits
                    .iter()
                    .map(|(l, n)| (l.to_string(), serde_json::json!(n)))
                    .collect();
                let file_hits = s.file_line_hits.as_ref().map(|hits| {
                    hits.iter()
                        .map(|(l, n)| (l.to_string(), serde_json::json!(n)))
                        .collect::<serde_json::Map<String, serde_json::Value>>()
                });
                serde_json::json!({
                    "name": s.name,
                    "origin": s.origin,
                    "coordinateSpace": s.coordinate_space,
                    // Provenance: prepared line (1-based, index+1) →
                    // origin file line, `null` for generated lines.
                    "lineMap": s.line_map,
                    "originLines": s.origin_lines,
                    // Hits resolved to origin-file lines through
                    // `lineMap` (merged when several prepared lines
                    // share a file line); `null` when unmapped.
                    "fileLineHits": file_hits,
                    "totalLines": s.total_lines,
                    "linesCovered": s.lines_covered(),
                    "lineHits": hits,
                    "applications": s.applications,
                    "declarations": s.decls.iter().map(|d| serde_json::json!({
                        "name": d.name,
                        "start": d.start,
                        "end": d.end,
                        "isArrow": d.is_arrow,
                        "anyHits": d.any_hits,
                        "bodyHits": d.body_hits,
                        "covered": d.covered(),
                        "fileLine": d.file_line,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let total_lines: u64 = self.sources.iter().map(|s| s.total_lines).sum();
        let lines_covered: u64 = self.sources.iter().map(|s| s.lines_covered()).sum();
        let decls_total: u64 = self.sources.iter().map(|s| s.decls.len() as u64).sum();
        let decls_covered: u64 = self
            .sources
            .iter()
            .flat_map(|s| s.decls.iter())
            .filter(|d| d.covered())
            .count() as u64;
        // File-line totals only when at least one source resolved
        // provenance — otherwise the keys are omitted (null).
        let file_lines_covered: Option<u64> = self
            .sources
            .iter()
            .any(|s| s.file_line_hits.is_some())
            .then(|| {
                self.sources
                    .iter()
                    .filter_map(|s| s.file_line_hits.as_ref())
                    .map(|hits| hits.len() as u64)
                    .sum()
            });
        serde_json::json!({
            "coordinateSpace": "prepared-source (spans index the post-module-prep text, not the file on disk); per-source lineMap/fileLineHits resolve to origin-file lines when the producer attached provenance",
            "totals": {
                "sources": self.sources.len(),
                "linesTotal": total_lines,
                "linesCovered": lines_covered,
                "fileLinesCovered": file_lines_covered,
                "declarationsTotal": decls_total,
                "declarationsCovered": decls_covered,
            },
            "sources": sources,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_decls_finds_bindings() {
        let src =
            "do {\nhelper := (x) ~> x + 1;\nanswer := 42\n{ helper: helper, answer: answer }\n}";
        let decls = scan_decls(src);
        assert_eq!(decls.len(), 2, "{decls:?}");
        assert_eq!(decls[0].name, "helper");
        assert!(decls[0].is_arrow);
        assert_eq!(decls[1].name, "answer");
        assert!(!decls[1].is_arrow);
        // Ranges must slice back to the statement text.
        assert!(src[decls[0].start..decls[0].end].contains("helper :="));
        assert!(src[decls[1].start..decls[1].end].contains("answer := 42"));
    }

    #[test]
    fn scan_decls_skips_imports_and_bare_exprs() {
        let src = "import \"./foo.hnl\"\nfrom \"str\" import len\n40 + 2";
        assert!(scan_decls(src).is_empty());
    }

    #[test]
    fn session_records_line_and_decl_hits() {
        let session = CoverageSession::new();
        session.install_local();
        let src = CoverageSource::new("mod.hnl", "f := (x) ~> x + 1\ng := 2\n");
        let id = CoverageSession::register(src).expect("registered");
        // Dedup: same name+text returns the same id.
        assert_eq!(
            CoverageSession::register(CoverageSource::new(
                "mod.hnl",
                "f := (x) ~> x + 1\ng := 2\n"
            )),
            Some(id)
        );
        {
            let _g = CoverageSession::push_source(id).expect("guard");
            // "f := (x) ~> x + 1" — byte offsets: `f`@0, `~>` arrow body `x`@…
            // Simulate definition-time hits (not in arrow body).
            record_node_span(Some((0, 1)));
            record_node_span(Some((10, 11)));
        }
        // Arrow-body hits via push_arrow_source.
        {
            let _g = CoverageSession::push_arrow_source(id).expect("guard");
            record_node_span(Some((12, 13)));
        }
        let report = session.report();
        assert_eq!(report.sources.len(), 1);
        let s = &report.sources[0];
        assert_eq!(s.lines_covered(), 1, "all hits on line 1: {s:?}");
        assert_eq!(s.decls.len(), 2);
        // `f` is arrow-valued: only body hits count → covered.
        assert!(s.decls[0].covered(), "{:?}", s.decls[0]);
        assert!(s.decls[0].body_hits > 0);
        // `g` never hit → uncovered.
        assert!(!s.decls[1].covered());
        CoverageSession::uninstall_local();
    }

    #[test]
    fn inactive_session_is_noop() {
        CoverageSession::uninstall_local();
        assert!(!CoverageSession::is_active_local());
        assert!(CoverageSession::register(CoverageSource::new("x", "y")).is_none());
        assert!(CoverageSession::push_source(0).is_none());
        CoverageSession::note_arrow_body(0xdead);
        assert!(CoverageSession::source_for_body(0xdead).is_none());
    }

    #[test]
    fn origin_line_map_tracks_pieces() {
        let mut m = OriginLineMap::new();
        // Generated wrapper line, then a two-line verbatim piece from
        // file line 10, then a generated binding on file line 20.
        m.push("do {\n", None);
        m.push("f := 1;\n", Some(10));
        m.push("g :=\n  2;\n", Some(20));
        m.push("}\n", None);
        assert_eq!(
            m.into_vec(),
            vec![
                None,        // `do {`
                Some(10),    // `f := 1;`
                Some(20),    // `g :=`
                Some(21),    // `  2;` — contiguous interior line
                None,        // `}`
            ]
        );
    }

    #[test]
    fn origin_line_map_mid_line_and_explicit_lines() {
        let mut m = OriginLineMap::new();
        m.push("a := 1; ", Some(3)); // starts line 1, no newline yet
        m.push("b := 2;\n", Some(7)); // continues line 1 — no entry
        // `b := 2;` shares prepared line 1; the \n ends it.
        m.push_lines("x\ny\n", &[Some(4), None]);
        assert_eq!(
            m.into_vec(),
            vec![Some(3), Some(4), None]
        );
    }

    #[test]
    fn report_resolves_file_line_hits() {
        let session = CoverageSession::new();
        session.install_local();
        // Prepared text: line 1 generated, lines 2-3 map to file 10-11.
        let src = "do {\nf := 1\ng := 2\n}"
            .to_string();
        let mut map = OriginLineMap::new();
        map.push("do {\n", None);
        map.push("f := 1\n", Some(10));
        map.push("g := 2\n", Some(11));
        map.push("}", None);
        let spec = CoverageSource::new("mod.hnl", src)
            .with_origin("mod.hnl")
            .with_file_lines(map.into_vec(), Some(20));
        let id = CoverageSession::register(spec).expect("registered");
        {
            let _g = CoverageSession::push_source(id).expect("guard");
            // Hit on prepared line 2 (`f := 1`) → file line 10.
            record_node_span(Some((5, 6)));
        }
        let report = session.report();
        let s = &report.sources[0];
        assert_eq!(
            s.file_line_hits.as_deref(),
            Some(&[(10u32, 1u64)][..]),
            "{s:?}"
        );
        assert_eq!(s.origin_lines, Some(20));
        assert_eq!(s.line_map.as_ref().unwrap()[1], Some(10));
        // JSON exposes the resolved hits + per-decl file lines.
        let json = report.to_json();
        assert_eq!(json["sources"][0]["fileLineHits"]["10"], 1);
        assert_eq!(json["sources"][0]["lineMap"][1], 10);
        assert_eq!(json["sources"][0]["originLines"], 20);
        CoverageSession::uninstall_local();
    }
}
