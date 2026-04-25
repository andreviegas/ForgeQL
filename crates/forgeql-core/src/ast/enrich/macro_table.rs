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
