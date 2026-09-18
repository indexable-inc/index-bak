//! The Rust adapter: a semantic dependency graph out of a SCIP index, handed to
//! [`dag_complexity_core`].
//!
//! # Granularity, and what it does and does not capture
//!
//! A node is a **source file** (`--granularity module`) or a **crate**
//! (`--granularity crate`). Both come from the same table: rust-analyzer's
//! resolved occurrences, so an edge exists because name resolution said one
//! file uses a symbol another file defines, not because a `use` line was
//! pattern-matched. File granularity stands in for module granularity: an
//! inline `mod` inside a file is folded into it, which is the conservative
//! direction (it can merge two modules, never invent an edge).
//!
//! Item granularity was left out on purpose. rust-analyzer resolves each
//! occurrence to a symbol, but attributing the *referring* side to an
//! enclosing item means reconstructing item spans from occurrence ranges, and a
//! guessed enclosing item produces edges nobody can check. A correct
//! file-and-crate graph beats a guessed function graph.
//!
//! What an edge does capture: paths, method calls resolved through inherent and
//! trait impls, macro-expanded references rust-analyzer resolved, and re-exports
//! (which land on the defining file, not the re-exporting one).
//!
//! What it does not: `build.rs` outputs and `include!`d files, `cfg`-disabled
//! code the index was not built with, and anything behind a feature flag that
//! was off. A file whose only coupling is a generated constant looks
//! independent here and is not.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use dag_complexity_core::{Builder, Dag, Node};
use snafu::{ResultExt as _, Snafu};

pub use scip::types::Index as ScipIndex;
pub use scipql_core::Error as IndexError;
pub use scipql_core::{index, load_index, project_root};

/// What one node stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Granularity {
    /// One node per source file.
    #[default]
    Module,
    /// One node per crate.
    Crate,
}

impl std::str::FromStr for Granularity {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "module" => Ok(Self::Module),
            "crate" => Ok(Self::Crate),
            other => Err(format!("expected `module` or `crate`, got `{other}`")),
        }
    }
}

/// How to build the graph.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub granularity: Granularity,
    /// Add a node per external crate a workspace file references. Off by
    /// default: an external crate has no dependencies of its own in this index,
    /// so it lands as a leaf whose blast radius is real but whose cost is
    /// unknown, which is exactly the shape that pollutes a leverage ranking.
    pub include_external: bool,
}

/// What can go wrong building the graph.
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("failed to lower the SCIP index"))]
    Facts { source: scipql_core::Error },
    #[snafu(display("failed to read {}", path.display()))]
    ReadSource {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("two nodes claimed the same key"))]
    Build { source: dag_complexity_core::BuildError },
}

/// Build a dependency graph from a SCIP index rooted at `root`.
///
/// An edge `a -> b` means a symbol defined in `b` is referenced from `a`, so a
/// change to `b` invalidates `a`.
///
/// # Errors
///
/// Fails if the index cannot be lowered, a source file it names cannot be read,
/// or the resolved graph turns out cyclic.
pub fn graph(scip_index: &ScipIndex, root: &Path, options: &Options) -> Result<Dag, Error> {
    let facts = scipql_core::facts_from_index(scip_index, Some(root)).context(FactsSnafu)?;

    // A symbol's home file. First definition wins, and documents arrive in a
    // stable order, so two runs over the same index agree.
    let mut home: HashMap<&str, &str> = HashMap::new();
    for row in &facts.occurrences {
        if row.role == "definition" {
            home.entry(row.symbol.as_str()).or_insert(&row.path);
        }
    }
    let structural: std::collections::HashSet<&str> = facts
        .symbols
        .iter()
        .filter(|row| STRUCTURAL_KINDS.contains(&row.kind.as_str()))
        .map(|row| row.symbol.as_str())
        .collect();

    let measured = measure(&facts.documents, root)?;
    let crate_of = crates_by_path(&home);
    let group = |path: &str| -> Option<String> {
        match options.granularity {
            Granularity::Module => Some(path.to_owned()),
            Granularity::Crate => crate_of.get(path).cloned(),
        }
    };

    let mut builder = Builder::new();
    let mut ids = HashMap::new();
    for node in nodes(&facts.documents, &measured, &crate_of, options.granularity) {
        let key = node.key.clone();
        let id = builder.node(node);
        ids.insert(key, id);
    }

    let mut edges: BTreeSet<(String, String)> = BTreeSet::new();
    for row in &facts.occurrences {
        if row.role == "definition" || structural.contains(row.symbol.as_str()) {
            continue;
        }
        let Some(source) = group(&row.path) else {
            continue;
        };
        match home.get(row.symbol.as_str()) {
            Some(defined_in) => {
                if let Some(target) = group(defined_in)
                    && target != source
                {
                    edges.insert((source, target));
                }
            }
            None if options.include_external => {
                if let Some(package) = package_of(&row.symbol) {
                    edges.insert((source, format!("external:{package}")));
                }
            }
            None => {}
        }
    }

    for (dependent, dependency) in edges {
        let Some(dependent) = ids.get(&dependent).copied() else {
            continue;
        };
        let dependency = *ids.entry(dependency.clone()).or_insert_with(|| {
            builder.node(Node::new(dependency).with_kind("external"))
        });
        builder.depends_on(dependent, dependency);
    }

    // Condensed, not validated: Rust modules reference each other in cycles all
    // the time, and a cycle is one unit of invalidation rather than an error.
    builder.build_condensed().context(BuildSnafu)
}

/// SCIP symbol kinds that name a place rather than a thing.
///
/// `mod foo;` and the `foo::` in `use crate::foo::Bar` both resolve to the
/// module symbol, so counting them as dependencies makes every parent module
/// depend on its children while the children keep depending on the parent.
/// That turns each crate's whole module tree into one reference cycle: ix's
/// `codec` collapsed to a single 15-file node before this filter, and the
/// resulting node ranked near the top of a list it had no business being on.
/// The item at the end of the path carries the real edge.
const STRUCTURAL_KINDS: [&str; 2] = ["Module", "Namespace"];

/// One file's size, the cost proxy. Lines rather than bytes because a
/// twelve-line `lib.rs` that re-exports everything is the shape the leverage
/// ranking exists to catch, and lines are what a reader will check it against.
struct Measured {
    lines: f64,
    digest: String,
}

fn measure(documents: &[String], root: &Path) -> Result<BTreeMap<String, Measured>, Error> {
    documents
        .iter()
        .map(|path| {
            let full = root.join(path);
            let text = std::fs::read_to_string(&full).context(ReadSourceSnafu { path: full })?;
            #[expect(
                clippy::cast_precision_loss,
                reason = "a line count is exact in f64 far past any file"
            )]
            let lines = text.lines().count() as f64;
            Ok((
                path.clone(),
                Measured {
                    lines,
                    digest: format!("{:016x}", xxhash_rust::xxh64::xxh64(text.as_bytes(), 0)),
                },
            ))
        })
        .collect()
}

fn nodes(
    documents: &[String],
    measured: &BTreeMap<String, Measured>,
    crate_of: &HashMap<String, String>,
    granularity: Granularity,
) -> Vec<Node> {
    match granularity {
        Granularity::Module => documents
            .iter()
            .filter_map(|path| {
                let file = measured.get(path)?;
                Some(
                    Node::new(path.clone())
                        .with_kind("module")
                        .with_cost(file.lines)
                        .with_version(file.digest.clone()),
                )
            })
            .collect(),
        Granularity::Crate => {
            let mut rolled: BTreeMap<&str, (f64, Vec<&str>)> = BTreeMap::new();
            for path in documents {
                let (Some(name), Some(file)) = (crate_of.get(path), measured.get(path)) else {
                    continue;
                };
                let entry = rolled.entry(name.as_str()).or_insert((0.0, Vec::new()));
                entry.0 += file.lines;
                entry.1.push(file.digest.as_str());
            }
            rolled
                .into_iter()
                .map(|(name, (lines, mut digests))| {
                    digests.sort_unstable();
                    Node::new(name)
                        .with_kind("crate")
                        .with_cost(lines)
                        .with_version(format!(
                            "{:016x}",
                            xxhash_rust::xxh64::xxh64(digests.join("").as_bytes(), 0)
                        ))
                })
                .collect()
        }
    }
}

/// The crate each document belongs to, read off the SCIP monikers of the
/// symbols it defines. Cheaper and more exact than re-deriving it from
/// `cargo metadata`: the moniker is what rust-analyzer itself resolved.
fn crates_by_path(home: &HashMap<&str, &str>) -> HashMap<String, String> {
    let mut votes: HashMap<&str, BTreeMap<&str, usize>> = HashMap::new();
    for (symbol, path) in home {
        if let Some(package) = package_of(symbol) {
            *votes.entry(path).or_default().entry(package).or_default() += 1;
        }
    }
    votes
        .into_iter()
        .filter_map(|(path, tally)| {
            // A file defines symbols in exactly one crate, except that
            // `#[path]`-included files and inline test modules can smuggle in a
            // second. Take the most-voted so a stray symbol cannot move a file.
            let winner = tally
                .into_iter()
                .max_by(|left, right| left.1.cmp(&right.1).then(right.0.cmp(left.0)))?;
            Some((path.to_owned(), winner.0.to_owned()))
        })
        .collect()
}

/// The package name out of a SCIP moniker, which rust-analyzer writes as
/// `rust-analyzer cargo <package> <version> <descriptors>`. A local symbol
/// (`local 3`) has no package.
fn package_of(symbol: &str) -> Option<&str> {
    let mut fields = symbol.split(' ');
    let scheme = fields.next()?;
    if scheme != "rust-analyzer" {
        return None;
    }
    let _manager = fields.next()?;
    fields.next().filter(|package| !package.is_empty())
}

#[cfg(test)]
mod tests;
