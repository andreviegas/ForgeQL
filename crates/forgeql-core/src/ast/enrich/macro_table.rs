//! Macro definition table and types for the two-pass macro-expansion pipeline.
//!
//! [`MacroTable`] accumulates [`MacroDef`] records during the first indexing
//! pass and supplies them to enrichers during the second pass.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub use crate::ast::lang::MacroDef;

// -----------------------------------------------------------------------
// MacroTable
// -----------------------------------------------------------------------

/// Accumulates macro definitions and invocation-site information.
///
/// Built during the first pass of the two-pass indexing pipeline;
/// consumed (read-only) during the second enrichment pass.
#[derive(Debug, Default)]
pub struct MacroTable {
    /// All definitions indexed by macro name (one name → many defs for
    /// multiply-defined or conditionally-compiled macros).
    defs: HashMap<String, Vec<MacroDef>>,

    /// Files that define each macro name (for incremental invalidation).
    defs_by_file: HashMap<PathBuf, HashSet<String>>,

    /// Files that invoke each macro name (for blast-radius analysis).
    invokers: HashMap<String, HashSet<PathBuf>>,
}

/// Where a [`MacroTable`]'s heap bytes sit, as counted by
/// [`MacroTable::heap_breakdown`].
///
/// Every field is bytes. They are reported separately rather than summed
/// because the decision they inform is which one to stop storing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MacroHeapBreakdown {
    /// Fixed size of every [`MacroDef`] record, before its strings.
    pub records: usize,
    /// The `defs` map's keys — one copy of each distinct macro name.
    pub def_keys: usize,
    /// Each record's own `name`, a second copy of the same text.
    pub names: usize,
    /// Each record's expansion `body`.
    pub bodies: usize,
    /// Each record's defining file path, stored once per definition rather
    /// than once per file.
    pub paths: usize,
    /// Parameter names of function-like macros.
    pub params: usize,
    /// Guard-branch text at the definition site.
    pub guards: usize,
    /// The `defs_by_file` index: a path per file and a third copy of each name.
    pub by_file: usize,
    /// The `invokers` index.
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
    pub fn insert(&mut self, def: MacroDef) {
        let name = def.name.clone();
        let file = def.file.clone();
        self.defs.entry(name.clone()).or_default().push(def);
        let _ = self.defs_by_file.entry(file).or_default().insert(name);
    }

    /// Record a file that invokes a macro by name.
    pub fn record_invocation(&mut self, name: &str, file: PathBuf) {
        let _ = self
            .invokers
            .entry(name.to_owned())
            .or_default()
            .insert(file);
    }

    /// Look up all definitions for a macro name.
    ///
    /// Returns an empty slice when the macro is not found.
    #[must_use]
    pub fn get(&self, name: &str) -> &[MacroDef] {
        self.defs.get(name).map_or(&[], Vec::as_slice)
    }

    /// Whether any definition exists for `name`.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.defs.contains_key(name)
    }

    /// Total number of definition records across all names.
    #[must_use]
    pub fn def_count(&self) -> usize {
        self.defs.values().map(Vec::len).sum()
    }

    /// All macro names defined in a specific file.
    #[must_use]
    pub fn names_in_file(&self, file: &std::path::Path) -> Option<&HashSet<String>> {
        self.defs_by_file.get(file)
    }

    /// Files that invoke a macro by name.
    #[must_use]
    pub fn invokers_of(&self, name: &str) -> Option<&HashSet<PathBuf>> {
        self.invokers.get(name)
    }

    /// Merge all definitions, file mappings, and invocation records from
    /// `other` into `self`.
    pub fn merge_from(&mut self, other: Self) {
        for (name, mut defs) in other.defs {
            self.defs.entry(name.clone()).or_default().append(&mut defs);
        }
        for (file, names) in other.defs_by_file {
            self.defs_by_file.entry(file).or_default().extend(names);
        }
        for (name, files) in other.invokers {
            self.invokers.entry(name).or_default().extend(files);
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
        let mut b = MacroHeapBreakdown::default();
        for (key, defs) in &self.defs {
            b.def_keys += key.len();
            b.records += defs.len() * std::mem::size_of::<MacroDef>();
            for d in defs {
                b.names += d.name.len();
                b.bodies += d.body.len();
                b.paths += d.file.as_os_str().len();
                b.params += d
                    .params
                    .as_ref()
                    .map_or(0, |p| p.iter().map(String::len).sum::<usize>());
                b.guards += d.guard_branch.as_ref().map_or(0, String::len);
            }
        }
        for (path, names) in &self.defs_by_file {
            b.by_file += path.as_os_str().len() + names.iter().map(String::len).sum::<usize>();
        }
        for (name, files) in &self.invokers {
            b.invokers += name.len() + files.iter().map(|f| f.as_os_str().len()).sum::<usize>();
        }
        b
    }

    /// Consume the table and return all macro definitions as a flat vector.
    ///
    /// Used to serialise macro defs into the persistent cache.
    #[must_use]
    pub fn into_defs(self) -> Vec<MacroDef> {
        self.defs.into_values().flatten().collect()
    }

    /// Borrow all macro definitions as a flat vector (for cache serialization
    /// without consuming the table).
    #[must_use]
    pub fn to_defs(&self) -> Vec<MacroDef> {
        self.defs.values().flatten().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        table.record_invocation("A", path.clone());
        assert!(table.invokers_of("A").unwrap().contains(&path));
    }
}
