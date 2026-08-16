//! Macro definition table and types for the two-pass macro-expansion pipeline.
//!
//! [`MacroTable`] accumulates [`MacroDef`] records during the first indexing
//! pass and supplies them to enrichers during the second pass.
//!
//! # Why the table does not keep a [`MacroDef`] as it arrives
//!
//! Two of a definition's fields are almost entirely repetition. The defining
//! path repeats once per definition — the Linux kernel yields 6,119,906
//! definitions across at most 80,510 distinct paths — and the macro name was
//! held three times over: as the `defs` key, inside the record, and again in
//! the by-file index. Together they accounted for roughly 1.3 GiB of the
//! 6.8 GiB of anonymous memory this phase holds at its peak, none of it
//! reclaimable — which is what decides whether a corpus whose high-water
//! mark is this phase indexes at all on a memory-constrained host. On a
//! corpus whose peak belongs instead to the later overlay build, as
//! Zephyr's does, shrinking this phase leaves the whole-run peak unmoved.
//!
//! So the table stores [`StoredDef`], which carries a `u32` id where
//! [`MacroDef`] carries a `String` or a `PathBuf`, and interns both into the
//! shared [`InternPool`] at the one point a definition enters the table —
//! [`MacroTable::insert`]. [`MacroDef`] itself is untouched: it is the
//! vocabulary the language plugins speak, and how core chooses to store what
//! they hand it must not reach back into them. Anything needing a whole
//! definition again asks [`MacroTable::hydrate`] for one.
//!
//! [`InternPool`]: crate::ast::intern::InternPool

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::ast::intern::{PathPool, StringPool};

pub use crate::ast::lang::MacroDef;

// -----------------------------------------------------------------------
// StoredDef
// -----------------------------------------------------------------------

/// A macro definition in the form [`MacroTable`] keeps it.
///
/// Identical to [`MacroDef`] except that the name and the defining file are
/// ids into the owning table's pools rather than owned strings. The ids stay
/// private because they mean nothing away from the table that issued them;
/// [`MacroTable::hydrate`] is the way back to a [`MacroDef`].
#[derive(Debug, Clone)]
pub struct StoredDef {
    /// Macro name, as an id into the owning table's name pool.
    name: u32,
    /// Defining file, as an id into the owning table's path pool.
    file: u32,
    /// Parameter names for function-like macros.
    params: Option<Vec<String>>,
    /// Expansion body text (post-`\` line-continuation joining).
    body: String,
    /// 1-based source line of the definition.
    line: u32,
    /// Guard group id from the guard stack at the definition site, if any.
    guard_group_id: Option<u64>,
    /// Guard branch text at the definition site, if any.
    guard_branch: Option<String>,
}

impl StoredDef {
    /// Parameter names for a function-like macro.
    ///
    /// `None` marks an object-like macro — that is the distinction, so an
    /// empty slice is a function-like macro declared with no parameters and
    /// is not the same thing.
    #[must_use]
    pub fn params(&self) -> Option<&[String]> {
        self.params.as_deref()
    }

    /// Expansion body text.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    /// 1-based source line of the definition.
    #[must_use]
    pub const fn line(&self) -> u32 {
        self.line
    }
}

// -----------------------------------------------------------------------
// MacroTable
// -----------------------------------------------------------------------

/// Accumulates macro definitions and invocation-site information.
///
/// Built during the first pass of the two-pass indexing pipeline;
/// consumed (read-only) during the second enrichment pass.
#[derive(Debug, Default)]
pub struct MacroTable {
    /// Every distinct macro name the table has seen. Records and both
    /// indexes below hold ids into this pool instead of the text.
    names: StringPool,

    /// Every distinct file path the table has seen — once per file, not once
    /// per definition.
    paths: PathPool,

    /// All definitions indexed by macro name id (one name → many defs for
    /// multiply-defined or conditionally-compiled macros).
    defs: HashMap<u32, Vec<StoredDef>>,

    /// Files that define each macro name (for incremental invalidation).
    defs_by_file: HashMap<u32, HashSet<u32>>,

    /// Files that invoke each macro name (for blast-radius analysis).
    invokers: HashMap<u32, HashSet<u32>>,
}

/// Where a [`MacroTable`]'s heap bytes sit, as counted by
/// [`MacroTable::heap_breakdown`].
///
/// Every field is bytes. They are reported separately rather than summed
/// because the decision they inform is which one to stop storing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MacroHeapBreakdown {
    /// Fixed size of every [`StoredDef`] record, before its strings.
    pub records: usize,
    /// The name pool: each distinct macro name, counted twice because the
    /// pool indexes it both by id and by text.
    pub def_keys: usize,
    /// Each record's own `name`. Always zero — a record holds a `u32` id,
    /// counted under `records`, and the text is counted once under
    /// `def_keys`. The field stays so the log line keeps showing that the
    /// second copy is gone.
    pub names: usize,
    /// Each record's expansion `body`.
    pub bodies: usize,
    /// The path pool: each distinct defining path, counted twice for the same
    /// reason as `def_keys`, and once per file rather than per definition.
    pub paths: usize,
    /// Parameter names of function-like macros.
    pub params: usize,
    /// Guard-branch text at the definition site.
    pub guards: usize,
    /// The `defs_by_file` index, now ids on both sides.
    pub by_file: usize,
    /// The `invokers` index, now ids on both sides.
    pub invokers: usize,
}

impl std::fmt::Display for MacroHeapBreakdown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mib = |n: usize| n / (1024 * 1024);
        write!(
            f,
            "records={}MiB def_keys={}MiB names={}MiB bodies={}MiB paths={}MiB \
             params={}MiB guards={}MiB by_file={}MiB invokers={}MiB",
            mib(self.records),
            mib(self.def_keys),
            mib(self.names),
            mib(self.bodies),
            mib(self.paths),
            mib(self.params),
            mib(self.guards),
            mib(self.by_file),
            mib(self.invokers),
        )
    }
}

impl MacroTable {
    /// Create a new, empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a macro definition.
    ///
    /// This is the boundary between the plugins' [`MacroDef`] and the table's
    /// storage: the name and path are interned here, and the `String` and
    /// `PathBuf` the plugin allocated are dropped rather than kept.
    pub fn insert(&mut self, def: MacroDef) {
        let name = self.names.intern(def.name.as_str());
        let file = self.paths.intern(def.file.as_path());
        let _ = self.defs_by_file.entry(file).or_default().insert(name);
        self.defs.entry(name).or_default().push(StoredDef {
            name,
            file,
            params: def.params,
            body: def.body,
            line: def.line,
            guard_group_id: def.guard_group_id,
            guard_branch: def.guard_branch,
        });
    }

    /// Record a file that invokes a macro by name.
    pub fn record_invocation(&mut self, name: &str, file: &Path) {
        let name = self.names.intern(name);
        let file = self.paths.intern(file);
        let _ = self.invokers.entry(name).or_default().insert(file);
    }

    /// Look up all definitions for a macro name.
    ///
    /// Returns an empty slice when the macro is not found. The records are
    /// [`StoredDef`]s; [`Self::hydrate`] turns one back into a [`MacroDef`].
    #[must_use]
    pub fn get(&self, name: &str) -> &[StoredDef] {
        self.names
            .get_id(name)
            .and_then(|id| self.defs.get(&id))
            .map_or(&[], Vec::as_slice)
    }

    /// Whether any definition exists for `name`.
    ///
    /// Interned is not the same as defined — [`Self::record_invocation`]
    /// interns names it has only ever seen invoked — so this asks `defs`
    /// rather than the pool.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.names
            .get_id(name)
            .is_some_and(|id| self.defs.contains_key(&id))
    }

    /// Rebuild the full [`MacroDef`] that a stored record came from.
    ///
    /// `def` must have come from this table: ids are only meaningful to the
    /// pools that issued them.
    #[must_use]
    pub fn hydrate(&self, def: &StoredDef) -> MacroDef {
        debug_assert!(
            (def.name as usize) < self.names.len() && (def.file as usize) < self.paths.len(),
            "hydrate was given a StoredDef from a different table"
        );
        MacroDef {
            name: self.names.get(def.name).to_owned(),
            params: def.params.clone(),
            body: def.body.clone(),
            file: self.paths.get(def.file).to_path_buf(),
            line: def.line,
            guard_group_id: def.guard_group_id,
            guard_branch: def.guard_branch.clone(),
        }
    }

    /// Total number of definition records across all names.
    #[must_use]
    pub fn def_count(&self) -> usize {
        self.defs.values().map(Vec::len).sum()
    }

    /// All macro names defined in a specific file, in no particular order.
    #[must_use]
    pub fn names_in_file(&self, file: &Path) -> Vec<&str> {
        let Some(id) = self.paths.get_id(file) else {
            return Vec::new();
        };
        self.defs_by_file.get(&id).map_or_else(Vec::new, |names| {
            names.iter().map(|&n| self.names.get(n)).collect()
        })
    }

    /// Files that invoke a macro by name, in no particular order.
    #[must_use]
    pub fn invokers_of(&self, name: &str) -> Vec<&Path> {
        let Some(id) = self.names.get_id(name) else {
            return Vec::new();
        };
        self.invokers.get(&id).map_or_else(Vec::new, |files| {
            files.iter().map(|&f| self.paths.get(f)).collect()
        })
    }

    /// Merge all definitions, file mappings, and invocation records from
    /// `other` into `self`.
    ///
    /// `other` numbered its strings independently, so every id crossing over
    /// is remapped through `self`'s pools first. That costs one lookup per
    /// distinct string, not per record.
    ///
    /// Each name's definitions keep their insertion order and `other`'s run
    /// is appended whole after `self`'s, because that order decides which
    /// definition [`resolve_macro`] picks for an invocation two definitions
    /// could both accept.
    ///
    /// [`resolve_macro`]: crate::ast::enrich::macro_resolve::resolve_macro
    pub fn merge_from(&mut self, other: Self) {
        let names: Vec<u32> = other
            .names
            .iter()
            .map(|n| self.names.intern(n.as_str()))
            .collect();
        let paths: Vec<u32> = other
            .paths
            .iter()
            .map(|p| self.paths.intern(p.as_path()))
            .collect();
        // `ids` was built by walking `other`'s pool end to end, so it has an
        // entry for every id `other` ever issued and the lookup cannot miss.
        let remap = |ids: &[u32], id: u32| {
            debug_assert!(
                (id as usize) < ids.len(),
                "merge_from was handed an id its own pool never issued"
            );
            ids.get(id as usize).copied().unwrap_or_default()
        };

        for (name, mut defs) in other.defs {
            let name = remap(&names, name);
            for def in &mut defs {
                def.name = name;
                def.file = remap(&paths, def.file);
            }
            self.defs.entry(name).or_default().append(&mut defs);
        }
        for (file, defined) in other.defs_by_file {
            self.defs_by_file
                .entry(remap(&paths, file))
                .or_default()
                .extend(defined.into_iter().map(|n| remap(&names, n)));
        }
        for (name, files) in other.invokers {
            self.invokers
                .entry(remap(&names, name))
                .or_default()
                .extend(files.into_iter().map(|f| remap(&paths, f)));
        }
    }

    /// Roughly how many heap bytes this table holds, split by what holds them.
    ///
    /// Walks the whole table, so it is for a diagnostic log line at a phase
    /// boundary and not for a hot path. The figures are the bytes of the
    /// strings and paths themselves plus the fixed size of each record; they
    /// ignore allocator rounding and hash-table slack, so the true footprint
    /// is somewhat larger than the total. What they are good for is the
    /// *ratio* — which field of a macro definition is worth attacking.
    #[must_use]
    pub fn heap_breakdown(&self) -> MacroHeapBreakdown {
        // A pool holds each value twice, once in its id-indexed `Vec` and
        // once as its lookup key, so both copies are counted here.
        let mut b = MacroHeapBreakdown {
            def_keys: 2 * self.names.iter().map(String::len).sum::<usize>(),
            paths: 2 * self
                .paths
                .iter()
                .map(|p| p.as_os_str().len())
                .sum::<usize>(),
            ..MacroHeapBreakdown::default()
        };
        for defs in self.defs.values() {
            b.records += defs.len() * std::mem::size_of::<StoredDef>();
            for d in defs {
                b.bodies += d.body.len();
                b.params += d
                    .params
                    .as_ref()
                    .map_or(0, |p| p.iter().map(String::len).sum::<usize>());
                b.guards += d.guard_branch.as_ref().map_or(0, String::len);
            }
        }
        let id = std::mem::size_of::<u32>();
        for names in self.defs_by_file.values() {
            b.by_file += id + names.len() * id;
        }
        for files in self.invokers.values() {
            b.invokers += id + files.len() * id;
        }
        b
    }

    /// Consume the table and return all macro definitions as a flat vector.
    ///
    /// Used to serialise macro defs into the persistent cache. Each name's
    /// definitions stay contiguous and in order, so feeding the vector back
    /// through [`Self::insert`] rebuilds a table that resolves identically.
    #[must_use]
    pub fn into_defs(self) -> Vec<MacroDef> {
        let Self {
            names, paths, defs, ..
        } = self;
        defs.into_values()
            .flatten()
            .map(|d| MacroDef {
                name: names.get(d.name).to_owned(),
                params: d.params,
                body: d.body,
                file: paths.get(d.file).to_path_buf(),
                line: d.line,
                guard_group_id: d.guard_group_id,
                guard_branch: d.guard_branch,
            })
            .collect()
    }

    /// Borrow all macro definitions as a flat vector (for cache serialization
    /// without consuming the table).
    #[must_use]
    pub fn to_defs(&self) -> Vec<MacroDef> {
        self.defs
            .values()
            .flatten()
            .map(|d| self.hydrate(d))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_def(name: &str) -> MacroDef {
        MacroDef {
            name: name.to_owned(),
            params: None,
            body: String::new(),
            file: PathBuf::from("test.cpp"),
            line: 1,
            guard_group_id: None,
            guard_branch: None,
        }
    }

    fn make_def_in(name: &str, file: &str, line: u32) -> MacroDef {
        MacroDef {
            file: PathBuf::from(file),
            line,
            ..make_def(name)
        }
    }

    #[test]
    fn insert_and_get() {
        let mut table = MacroTable::new();
        table.insert(make_def("MY_MACRO"));
        assert_eq!(table.get("MY_MACRO").len(), 1);
        assert!(table.contains("MY_MACRO"));
        assert!(!table.contains("OTHER"));
    }

    #[test]
    fn multiply_defined() {
        let mut table = MacroTable::new();
        table.insert(make_def("A"));
        table.insert(make_def("A"));
        assert_eq!(table.get("A").len(), 2);
        assert_eq!(table.def_count(), 2);
    }

    #[test]
    fn invocation_tracking() {
        let mut table = MacroTable::new();
        let path = PathBuf::from("user.cpp");
        table.record_invocation("A", &path);
        assert_eq!(table.invokers_of("A"), vec![path.as_path()]);
    }

    /// Interning a name is not the same as defining it. `record_invocation`
    /// puts names into the very pool `defs` is keyed by, so a `contains` that
    /// asked the pool would report every invoked-but-undefined name as
    /// defined — and `macro_expand_enrich` calls `contains` to decide whether
    /// a C call expression is a macro call at all, so that would turn every
    /// ordinary function call into an attempted macro expansion.
    #[test]
    fn a_name_only_invoked_is_not_defined() {
        let mut table = MacroTable::new();
        table.record_invocation("NEVER_DEFINED", Path::new("user.c"));
        assert!(!table.contains("NEVER_DEFINED"));
        assert!(table.get("NEVER_DEFINED").is_empty());
    }

    #[test]
    fn hydrate_round_trips_every_field() {
        let def = MacroDef {
            name: "MAX".to_owned(),
            params: Some(vec!["a".to_owned(), "b".to_owned()]),
            body: "((a) > (b) ? (a) : (b))".to_owned(),
            file: PathBuf::from("include/util.h"),
            line: 42,
            guard_group_id: Some(7),
            guard_branch: Some("#ifdef FAST".to_owned()),
        };
        let mut table = MacroTable::new();
        table.insert(def.clone());

        let back = table.hydrate(&table.get("MAX")[0]);
        assert_eq!(back.name, def.name);
        assert_eq!(back.params, def.params);
        assert_eq!(back.body, def.body);
        assert_eq!(back.file, def.file);
        assert_eq!(back.line, def.line);
        assert_eq!(back.guard_group_id, def.guard_group_id);
        assert_eq!(back.guard_branch, def.guard_branch);
    }

    /// The point of the whole representation: a path costs the table the same
    /// whether one definition names it or a thousand do. Before interning
    /// this grew linearly with the definition count, which is what made the
    /// Linux kernel spend 783 MiB on 80,510 distinct paths.
    #[test]
    fn a_path_costs_the_same_however_many_definitions_name_it() {
        let mut one = MacroTable::new();
        one.insert(make_def_in("A", "drivers/net/some/deep/path.c", 1));

        let mut many = MacroTable::new();
        for i in 0..1000 {
            many.insert(make_def_in(
                &format!("M{i}"),
                "drivers/net/some/deep/path.c",
                i,
            ));
        }

        assert_eq!(many.def_count(), 1000);
        assert_eq!(
            many.heap_breakdown().paths,
            one.heap_breakdown().paths,
            "the path pool must hold one copy per distinct path, not one per definition"
        );
    }

    /// Each record's own name is an id now, so the second copy of the text is
    /// gone entirely rather than merely smaller.
    #[test]
    fn a_record_holds_no_copy_of_its_own_name() {
        let mut table = MacroTable::new();
        table.insert(make_def("A_VERY_LONG_MACRO_NAME_INDEED"));
        assert_eq!(table.heap_breakdown().names, 0);
    }

    /// Two tables number their strings independently, so a merge that took
    /// `other`'s ids at face value would silently attribute definitions to
    /// whatever name and file happened to hold that id in `self`. The pools
    /// here are deliberately built in opposite orders so any such mix-up
    /// shows up as a wrong name or a wrong file.
    #[test]
    fn merge_remaps_ids_from_the_other_table() {
        let mut left = MacroTable::new();
        left.insert(make_def_in("FIRST", "a.c", 10));
        left.insert(make_def_in("SECOND", "b.c", 20));

        let mut right = MacroTable::new();
        right.insert(make_def_in("SECOND", "b.c", 21));
        right.insert(make_def_in("THIRD", "c.c", 30));

        left.merge_from(right);

        let third = left.hydrate(&left.get("THIRD")[0]);
        assert_eq!(third.name, "THIRD");
        assert_eq!(third.file, PathBuf::from("c.c"));
        assert_eq!(third.line, 30);

        let seconds: Vec<MacroDef> = left.get("SECOND").iter().map(|d| left.hydrate(d)).collect();
        assert_eq!(seconds.len(), 2);
        for def in &seconds {
            assert_eq!(def.name, "SECOND");
            assert_eq!(def.file, PathBuf::from("b.c"));
        }
        assert_eq!(left.names_in_file(Path::new("c.c")), vec!["THIRD"]);
    }

    /// `resolve_macro` picks the first definition matching an invocation's
    /// arity, so the order definitions sit in decides which one an ambiguous
    /// call expands to — and therefore what gets indexed. A merge that
    /// reordered them would change indexed output without failing anything
    /// that only counts definitions.
    #[test]
    fn merge_keeps_definition_order() {
        let mut left = MacroTable::new();
        left.insert(make_def_in("A", "a.c", 1));
        left.insert(make_def_in("A", "a.c", 2));

        let mut right = MacroTable::new();
        right.insert(make_def_in("A", "b.c", 3));
        right.insert(make_def_in("A", "b.c", 4));

        left.merge_from(right);

        let lines: Vec<u32> = left.get("A").iter().map(StoredDef::line).collect();
        assert_eq!(lines, vec![1, 2, 3, 4]);
    }

    /// The cache round-trip: `into_defs` flattens, `insert` rebuilds. Each
    /// name's definitions must come back in the same order for the rebuilt
    /// table to resolve identically to the one that was serialised.
    #[test]
    fn into_defs_round_trips_through_insert() {
        let mut table = MacroTable::new();
        table.insert(make_def_in("A", "a.c", 1));
        table.insert(make_def_in("B", "b.c", 2));
        table.insert(make_def_in("A", "c.c", 3));

        let flat = table.into_defs();
        assert_eq!(flat.len(), 3);

        let mut rebuilt = MacroTable::new();
        for def in flat {
            rebuilt.insert(def);
        }

        let a: Vec<(u32, PathBuf)> = rebuilt
            .get("A")
            .iter()
            .map(|d| (d.line(), rebuilt.hydrate(d).file))
            .collect();
        assert_eq!(
            a,
            vec![(1, PathBuf::from("a.c")), (3, PathBuf::from("c.c"))]
        );
        assert_eq!(rebuilt.get("B").len(), 1);
    }
}
