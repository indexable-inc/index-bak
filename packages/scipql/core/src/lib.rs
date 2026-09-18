//! scipql-core: a SCIP semantic index lowered into Soufflé datalog facts, with
//! datalog queries and find/replace edits over the resolved symbols.
//!
//! Unlike `astlog` (datalog over tree-sitter syntax), the facts here are keyed
//! by SCIP *monikers*, so a query distinguishes two same-named symbols in
//! different modules and a rewrite touches only the real definition and its
//! references. The pipeline is: [`index`] (run `rust-analyzer scip`) →
//! [`load_index`] + [`facts_from_index`] (lower to relations) → [`query`] /
//! [`fix`] / [`rename`] (run Soufflé; a `fix` program emits an `edit` relation
//! applied via `edit-applier`).
//!
//! Every entry point takes an explicit `root` for the source tree (defaulting
//! to the index's `project_root`): byte offsets and on-disk writes resolve
//! relative document paths against it.

use std::path::Path;

use scip::types::Index;
use snafu::{ResultExt as _, ensure};

mod error;
mod facts;
mod index;
mod rename;
mod rewrite;
mod souffle;

pub use crate::error::Error;
pub use crate::facts::{
    Facts, OccurrenceRow, RelationshipRow, SCHEMA, SymbolRow, facts_from_index, load_index,
    project_root,
};
pub use crate::index::index;
pub use crate::rewrite::EDIT_SCHEMA;
pub use crate::souffle::{OutputRelation, QueryOutput};

fn resolve_root(index: &Index, root: Option<&Path>) -> std::path::PathBuf {
    root.map_or_else(|| facts::project_root(index), Path::to_path_buf)
}

/// Run a Soufflé `program` over the facts lowered from `index`.
///
/// Returns every `.output` relation. The fact relations ([`SCHEMA`]) are in
/// scope; the program only writes its own rules and `.output` directives.
///
/// # Errors
///
/// Fails if facts cannot be lowered or Soufflé errors.
pub fn query(index: &Index, root: Option<&Path>, program: &str) -> Result<QueryOutput, Error> {
    let facts = facts_from_index(index, root)?;
    let scratch = tempfile::tempdir().context(crate::error::ScratchSnafu)?;
    souffle::run(&facts, program, scratch.path())
}

/// Run a `fix` program (one that `.output`s `edit(path, start, end,
/// replacement)`) and apply its edits. Returns the unified diff; with `write`,
/// the files under `root` are rewritten on disk.
///
/// # Errors
///
/// Fails if facts cannot be lowered, Soufflé errors, an `edit` row is
/// malformed, edits overlap, or a source file is unreadable/unwritable.
pub fn fix(
    index: &Index,
    root: Option<&Path>,
    program: &str,
    write: bool,
) -> Result<String, Error> {
    let facts = facts_from_index(index, root)?;
    let root = resolve_root(index, root);
    let scratch = tempfile::tempdir().context(crate::error::ScratchSnafu)?;
    rewrite::fix(&facts, program, &root, write, scratch.path())
}

/// Rename every occurrence whose SCIP moniker contains `selector` to
/// `new_name`. A convenience over [`fix`] with a generated program; renaming
/// `net/Socket#` leaves a `mock/Socket#` untouched.
///
/// # Errors
///
/// Same as [`fix`].
pub fn rename(
    index: &Index,
    root: Option<&Path>,
    selector: &str,
    new_name: &str,
    write: bool,
) -> Result<String, Error> {
    ensure!(!selector.is_empty(), crate::error::EmptySelectorSnafu);
    fix(index, root, &rename::program(selector, new_name), write)
}
