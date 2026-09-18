//! `dag-complexity`: what a graph costs, what it invalidates, and which node is
//! holding it hostage.
//!
//! One core, one subcommand per adapter. `rust` reads a Rust workspace through
//! rust-analyzer; `graph` reads the JSON interchange any other adapter can emit
//! (see `dag_complexity_core::GraphFile`). Both then run the same metrics, and both
//! take `--against` for the invalidation diff.

mod report;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser as _;
use dag_complexity_core::{Dag, GraphFile};
use snafu::{ResultExt as _, Snafu};

/// How many entries each ranked list prints. A wall of four thousand nodes is
/// not a result; the top of the list is where the decisions are.
const DEFAULT_TOP: usize = 20;

#[derive(clap::Parser, Debug)]
#[command(name = "dag-complexity", version, about)]
struct Args {
    #[command(subcommand)]
    source: Source,
    /// Emit the report as JSON instead of prose.
    #[arg(long, global = true)]
    json: bool,
    /// How many entries per ranked list.
    #[arg(long, global = true, default_value_t = DEFAULT_TOP)]
    top: usize,
    /// Write the graph itself to this path in the JSON interchange format, so
    /// a later run can diff against it.
    #[arg(long, global = true, value_name = "PATH")]
    export: Option<PathBuf>,
    /// An earlier graph, in the JSON interchange format, to diff against.
    /// Switches the report to "what changed and what did it invalidate".
    #[arg(long, global = true, value_name = "PATH")]
    against: Option<PathBuf>,
}

#[derive(clap::Subcommand, Debug)]
enum Source {
    /// A Rust workspace, resolved by rust-analyzer.
    Rust {
        /// Directory holding the workspace `Cargo.toml`.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// `module` (one node per source file) or `crate`.
        #[arg(long, default_value = "module")]
        granularity: dag_complexity_rust::Granularity,
        /// Add a node per external crate referenced from the workspace.
        #[arg(long)]
        include_external: bool,
        /// Reuse an existing SCIP index instead of running rust-analyzer, which
        /// is the slow step. Produce one with `rust-analyzer scip <path>`.
        #[arg(long, value_name = "PATH")]
        scip: Option<PathBuf>,
    },
    /// A graph in the JSON interchange format, from any other adapter.
    Graph {
        /// The JSON file, or `-` for stdin.
        path: PathBuf,
    },
}

#[derive(Debug, Snafu)]
enum RunError {
    #[snafu(display("cannot read the graph at {}", path.display()))]
    ReadGraph {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("{} is not a dag-complexity graph", path.display()))]
    ParseGraph {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[snafu(display("cannot write the graph to {}", path.display()))]
    WriteGraph {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("cannot create a scratch directory for the SCIP index"))]
    Scratch { source: std::io::Error },
    #[snafu(display(
        "rust-analyzer could not index {}; it must be on PATH and the workspace must load",
        path.display()
    ))]
    Index {
        path: PathBuf,
        source: dag_complexity_rust::IndexError,
    },
    #[snafu(display("the Rust graph could not be built"))]
    RustGraph { source: dag_complexity_rust::Error },
    #[snafu(display("the graph does not describe a DAG"))]
    Build { source: dag_complexity_core::BuildError },
    #[snafu(display("failed to render the report"))]
    Json { source: serde_json::Error },
    #[snafu(display("failed to write the report"))]
    Write { source: std::io::Error },
    #[snafu(display(
        "{nodes} nodes is past the {} this reports on: exact reachability needs nodes^2 bits, so a graph this size would cost more than a gigabyte to score",
        report::MAX_NODES
    ))]
    TooLarge { nodes: usize },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("dag-complexity: {error}");
            let mut cause: &dyn std::error::Error = &error;
            while let Some(next) = std::error::Error::source(cause) {
                eprintln!("  caused by: {next}");
                cause = next;
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), RunError> {
    let args = Args::parse();
    let (dag, subject) = build(&args.source)?;
    snafu::ensure!(dag.len() <= report::MAX_NODES, TooLargeSnafu { nodes: dag.len() });

    if let Some(path) = &args.export {
        let text =
            serde_json::to_string(&GraphFile::from(&dag)).context(JsonSnafu)?;
        std::fs::write(path, text).context(WriteGraphSnafu { path: path.clone() })?;
    }

    let mut out = std::io::stdout().lock();
    match &args.against {
        Some(path) => {
            let before = read_graph(path)?;
            let diff = dag_complexity_core::diff(&before, &dag, None);
            if args.json {
                writeln!(out, "{}", serde_json::to_string(&diff).context(JsonSnafu)?)
            } else {
                write!(out, "{}", report::diff(&subject, &diff, args.top))
            }
        }
        None => {
            let analysis = dag.analyze();
            if args.json {
                writeln!(
                    out,
                    "{}",
                    serde_json::to_string(&analysis).context(JsonSnafu)?
                )
            } else {
                write!(out, "{}", report::analysis(&subject, &analysis, args.top))
            }
        }
    }
    .context(WriteSnafu)
}

/// The graph, plus the one-line description of what it is that heads the report.
fn build(source: &Source) -> Result<(Dag, String), RunError> {
    match source {
        Source::Graph { path } => {
            let dag = read_graph(path)?;
            Ok((dag, format!("graph from {}", path.display())))
        }
        Source::Rust {
            path,
            granularity,
            include_external,
            scip,
        } => {
            let options = dag_complexity_rust::Options {
                granularity: *granularity,
                include_external: *include_external,
            };
            // The index is the slow step and the only one worth announcing.
            let scratch;
            let index_path = match scip {
                Some(existing) => existing.clone(),
                None => {
                    eprintln!(
                        "dag-complexity: indexing {} with rust-analyzer (minutes on a large workspace)",
                        path.display()
                    );
                    scratch = tempfile::tempdir().context(ScratchSnafu)?;
                    let output = scratch.path().join("index.scip");
                    dag_complexity_rust::index(path, &output)
                        .context(IndexSnafu { path: path.clone() })?;
                    output
                }
            };
            let index = dag_complexity_rust::load_index(&index_path)
                .context(IndexSnafu { path: path.clone() })?;
            let root = root_of(&index, path);
            let dag = dag_complexity_rust::graph(&index, &root, &options).context(RustGraphSnafu)?;
            Ok((
                dag,
                format!(
                    "rust {} graph over {}",
                    match granularity {
                        dag_complexity_rust::Granularity::Module => "module",
                        dag_complexity_rust::Granularity::Crate => "crate",
                    },
                    root.display()
                ),
            ))
        }
    }
}

/// A SCIP index records the root it was produced from; prefer it, because an
/// index handed over with `--scip` may well have been made elsewhere and its
/// relative document paths only resolve against that root.
fn root_of(index: &dag_complexity_rust::ScipIndex, fallback: &Path) -> PathBuf {
    let recorded = dag_complexity_rust::project_root(index);
    if recorded.is_dir() {
        recorded
    } else {
        fallback.to_path_buf()
    }
}

fn read_graph(path: &Path) -> Result<Dag, RunError> {
    let text = if path == Path::new("-") {
        std::io::read_to_string(std::io::stdin().lock())
    } else {
        std::fs::read_to_string(path)
    }
    .context(ReadGraphSnafu { path: path.to_path_buf() })?;
    serde_json::from_str::<GraphFile>(&text)
        .context(ParseGraphSnafu { path: path.to_path_buf() })?
        .into_condensed_dag()
        .context(BuildSnafu)
}
