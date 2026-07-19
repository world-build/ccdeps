//! The scanning engine.
//!
//! State is split by how long it lives:
//!
//! - [`ScanService`] holds the path interner and the minimized-file cache. It
//!   lives for the whole process and is shared across every translation unit,
//!   so a shared header is read and minimized once no matter how many TUs pull
//!   it in.
//! - [`ScanScratch`] holds the macro table, conditional stack, and visited-set
//!   for one worker. It is cleared, not freed, between TUs, so the warm path
//!   allocates nothing.
//! - [`ScanOutput`] holds one TU's classified dependency lists, also cleared
//!   and reused.
//!
//! The `observe` callback of [`ScanService::scan_tu`] fires once per file a TU
//! depends on, whether that file was read fresh or served from the cache. An
//! embedder that virtualizes the filesystem records its per-TU dependency fact
//! there, so a cache hit still tracks the dependency instead of silently
//! dropping it and going stale when the header later changes.

use std::path::Path;
use std::sync::Arc;

use dashmap::DashMap;

use crate::depgraph;
use crate::intern::{content_hash, ContentHash, FileId, PathInterner};
use crate::macros::{MacroBase, MacroTable};
use crate::minimize::{minimize_with_options, MinimizeOptions};
use crate::resolve::SearchPaths;
use crate::vfs::{Vfs, VfsError};

/// Cached, minimized form of a file. Held by the [`ScanService`]'s content
/// cache and shared across every TU that includes the file. The expensive work
/// (read + minimize + guard detection + hash) happens once per unique file.
#[derive(Debug)]
pub struct FileInfo {
    /// Minimized source: only preprocessor-relevant lines, validated as text
    /// once here rather than on every visit.
    pub directives: Box<str>,
    /// `#pragma once` was seen.
    pub has_pragma_once: bool,
    /// A classic `#ifndef GUARD / #define GUARD` include guard was detected.
    pub has_include_guard: bool,
    /// The include-guard macro name, if any.
    pub guard_macro: Option<Box<str>>,
    /// Hash of the raw bytes, handed to the embedder via `observe`.
    pub content_hash: ContentHash,
    /// Original (pre-minimization) size, for stats.
    pub original_size: usize,
}

/// Process-wide options for a [`ScanService`]. These affect *content*
/// (minimization), so they belong to the service and its content cache rather
/// than to a per-TU [`ScanEnv`].
#[derive(Debug, Clone, Default)]
pub struct ServiceOptions {
    /// Process trigraphs during minimization (off by default, as modern
    /// gcc/clang do). Must be constant for a service: the content cache keys on
    /// file identity, not on this flag.
    pub trigraphs: bool,
}

/// Process-global scanning state: the path interner and the content cache.
/// Thread-safe, so several evaluator threads may call [`scan_tu`](Self::scan_tu)
/// concurrently and share both.
pub struct ScanService {
    pub(crate) interner: PathInterner,
    content: DashMap<FileId, Arc<FileInfo>>,
    options: ServiceOptions,
}

impl std::fmt::Debug for ScanService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanService")
            .field("interned", &self.interner.len())
            .field("cached", &self.content.len())
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl Default for ScanService {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanService {
    pub fn new() -> Self {
        Self::with_options(ServiceOptions::default())
    }

    pub fn with_options(options: ServiceOptions) -> Self {
        Self {
            interner: PathInterner::new(),
            content: DashMap::new(),
            options,
        }
    }

    /// The canonical path an id maps to.
    pub fn path(&self, id: FileId) -> Arc<Path> {
        self.interner.path(id)
    }

    /// Intern a canonical path.
    pub fn intern(&self, path: &Path) -> FileId {
        self.interner.intern(path)
    }

    /// Number of unique files minimized into the content cache.
    pub fn cached_files(&self) -> usize {
        self.content.len()
    }

    /// Get a file's cached minimized form, reading and minimizing it on the
    /// first request only. Shared across every TU.
    pub(crate) fn get_or_scan(
        &self,
        file: FileId,
        path: &Path,
        vfs: &dyn Vfs,
    ) -> Result<Arc<FileInfo>, VfsError> {
        if let Some(info) = self.content.get(&file) {
            return Ok(Arc::clone(&info));
        }
        let bytes = vfs.read(path)?;
        let hash = content_hash(&bytes);
        let minimized = minimize_with_options(
            &bytes,
            &MinimizeOptions {
                trigraphs: self.options.trigraphs,
            },
        );
        let info = Arc::new(FileInfo {
            directives: String::from_utf8_lossy(&minimized.content)
                .into_owned()
                .into_boxed_str(),
            has_pragma_once: minimized.has_pragma_once,
            has_include_guard: minimized.has_include_guard,
            guard_macro: minimized.guard_macro.map(String::into_boxed_str),
            content_hash: hash,
            original_size: bytes.len(),
        });
        // If another thread raced us, keep whichever landed first; both are
        // equal for an immutable-within-service tree.
        self.content
            .entry(file)
            .or_insert_with(|| Arc::clone(&info));
        Ok(info)
    }

    /// Scan one translation unit rooted at `source`.
    ///
    /// `scratch` and `out` are caller-owned and reused across calls; this
    /// method resets them. `observe` fires once for every file whose content
    /// the result depends on (the root and each transitively included file),
    /// **including cache hits**, so the embedder records a per-TU dependency
    /// fact for each one. Unresolved includes never fire `observe` (nothing was
    /// read); they land in [`ScanOutput::sys_includes`] / [`ScanOutput::missing`].
    pub fn scan_tu<F>(
        &self,
        source: &Path,
        env: &ScanEnv<'_>,
        vfs: &dyn Vfs,
        scratch: &mut ScanScratch,
        out: &mut ScanOutput,
        mut observe: F,
    ) -> Result<(), VfsError>
    where
        F: FnMut(FileId, ContentHash),
    {
        scratch.begin(Arc::clone(&env.macro_base));
        out.clear();

        let canonical = if env.trust_canonical {
            source.to_path_buf()
        } else {
            vfs.canonicalize(source)?
        };
        let root = self.interner.intern(&canonical);
        scratch.visited.mark(root);

        let info = self.get_or_scan(root, &canonical, vfs)?;
        observe(root, info.content_hash);

        let ctx = depgraph::Ctx {
            service: self,
            env,
            vfs,
        };
        depgraph::walk_file(
            &ctx,
            &mut scratch.macros,
            &mut scratch.visited,
            &mut scratch.cond_stack,
            out,
            &mut observe,
            &info,
            &canonical,
            false, // root is not a system header
            None,  // no display override for the root
        );
        Ok(())
    }
}

/// Per-worker scratch reused across TUs. Cleared, not freed: after warmup its
/// buffers keep their capacity, so a steady-state incremental scan allocates
/// nothing here.
pub struct ScanScratch {
    pub(crate) macros: MacroTable,
    pub(crate) visited: Visited,
    pub(crate) cond_stack: Vec<depgraph::ConditionalState>,
}

impl Default for ScanScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanScratch {
    pub fn new() -> Self {
        Self {
            macros: MacroTable::new(),
            visited: Visited::new(),
            cond_stack: Vec::new(),
        }
    }

    /// Reset for a new TU: swap in the env's macro base and clear the delta,
    /// advance the visited epoch (O(1)), and clear the conditional stack.
    fn begin(&mut self, base: Arc<MacroBase>) {
        self.macros.reset(base);
        self.cond_stack.clear();
        self.visited.next_epoch();
    }
}

/// A visited-set keyed by [`FileId`], using an epoch stamp so resetting between
/// TUs is O(1): a slot counts as visited only when it holds the current epoch.
pub(crate) struct Visited {
    seen: Vec<u32>,
    epoch: u32,
}

impl Visited {
    fn new() -> Self {
        Self {
            seen: Vec::new(),
            epoch: 0,
        }
    }

    fn next_epoch(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            // Wrapped after ~4e9 TUs: retire all stamps and restart.
            self.seen.iter_mut().for_each(|s| *s = 0);
            self.epoch = 1;
        }
    }

    #[inline]
    pub(crate) fn is_visited(&self, file: FileId) -> bool {
        self.seen.get(file.index()).copied() == Some(self.epoch)
    }

    #[inline]
    pub(crate) fn mark(&mut self, file: FileId) {
        let i = file.index();
        if i >= self.seen.len() {
            self.seen.resize(i + 1, 0);
        }
        self.seen[i] = self.epoch;
    }
}

/// The include-search configuration and macro seed for a set of TUs. Cheap to
/// build; an embedder constructs one per target (all TUs of a target share
/// search paths and predefs) and reuses it across that target's TUs.
pub struct ScanEnv<'a> {
    /// `-I` / `-iquote` / `-isystem` search paths.
    pub search_paths: &'a SearchPaths,
    /// Shared, immutable predef + `-D` seed (see [`MacroBase`]).
    pub macro_base: Arc<MacroBase>,
    /// Unknown-macro conditionals take both branches (a sound superset). This
    /// is the mode that needs no predef table.
    pub conservative: bool,
    /// Record unresolved includes as data (`sys_includes` / `missing`) instead
    /// of dropping them (gcc's `-MG`).
    pub missing_as_generated: bool,
    /// Treat resolved paths as already canonical (a Nix store has no symlinks
    /// inside the tree), skipping the per-include `canonicalize` round-trip.
    pub trust_canonical: bool,
    /// Record gcc-style display spellings for resolved deps (the CLI wants
    /// them; a store-path embedder does not, so it pays nothing).
    pub record_display: bool,
}

impl<'a> ScanEnv<'a> {
    /// An env over `search_paths` with an empty macro base and default flags.
    pub fn new(search_paths: &'a SearchPaths) -> Self {
        Self {
            search_paths,
            macro_base: Arc::new(MacroBase::new()),
            conservative: false,
            missing_as_generated: false,
            trust_canonical: false,
            record_display: false,
        }
    }
}

/// A single resolved dependency: the interned file plus how it was reached.
/// `Copy`-free only because of the optional display spelling, which is `None`
/// (no allocation) unless [`ScanEnv::record_display`] is set.
#[derive(Debug, Clone)]
pub struct ResolvedDep {
    pub file: FileId,
    pub is_system: bool,
    /// gcc-style spelling for output, when it differs from the canonical path.
    pub display: Option<Box<str>>,
}

/// Classified scan result, reused across TUs.
///
/// `resolved` is in first-encounter order (so the CLI reproduces gcc's dep
/// order); the embedder buckets it by [`ResolvedDep::is_system`] with the
/// [`includes`](Self::includes) / [`sys_paths`](Self::sys_paths) helpers, all
/// in terms of `FileId` with no string work. `sys_includes` and `missing` hold
/// the raw spellings of unresolved `<...>` and `"..."` includes.
#[derive(Default)]
pub struct ScanOutput {
    pub resolved: Vec<ResolvedDep>,
    pub sys_includes: Vec<Box<str>>,
    pub missing: Vec<Box<str>>,
    /// Dedup set for unresolved names, reused across TUs (retains capacity).
    seen_unresolved: std::collections::HashSet<Box<str>>,
}

impl ScanOutput {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear for reuse, keeping capacity.
    pub fn clear(&mut self) {
        self.resolved.clear();
        self.sys_includes.clear();
        self.missing.clear();
        self.seen_unresolved.clear();
    }

    /// Resolved non-system dependencies (`-I` / `-iquote` / relative), in order.
    pub fn includes(&self) -> impl Iterator<Item = FileId> + '_ {
        self.resolved
            .iter()
            .filter(|d| !d.is_system)
            .map(|d| d.file)
    }

    /// Resolved system dependencies (found via `-isystem`), in order.
    pub fn sys_paths(&self) -> impl Iterator<Item = FileId> + '_ {
        self.resolved.iter().filter(|d| d.is_system).map(|d| d.file)
    }

    pub(crate) fn push_resolved(
        &mut self,
        file: FileId,
        is_system: bool,
        display: Option<Box<str>>,
    ) {
        self.resolved.push(ResolvedDep {
            file,
            is_system,
            display,
        });
    }

    /// Record an unresolved include name (deduped). `angle` routes it to
    /// `sys_includes` (toolchain boundary) vs `missing` (generated / absent).
    pub(crate) fn push_unresolved(&mut self, name: &str, angle: bool) {
        if !self.seen_unresolved.contains(name) {
            let boxed: Box<str> = name.into();
            self.seen_unresolved.insert(boxed.clone());
            if angle {
                self.sys_includes.push(boxed);
            } else {
                self.missing.push(boxed);
            }
        }
    }
}
