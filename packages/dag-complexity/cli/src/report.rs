//! Prose rendering. The JSON carries everything; this carries the part a
//! person will act on, which is the top of two lists and the four numbers that
//! frame them.

use dag_complexity_core::{Analysis, Diff};

/// Reachability costs `nodes^2 / 8` bytes. Past this the report would spend
/// more than a gigabyte to say something a sample would have said, so the
/// caller is told rather than left watching a machine swap.
pub const MAX_NODES: usize = 92_000;

pub fn analysis(subject: &str, analysis: &Analysis, top: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{subject}: {} nodes, {} edges, {} asked for, {} starting points\n\n",
        analysis.nodes, analysis.edges, analysis.roots, analysis.leaves,
    ));

    let path = &analysis.critical_path;
    out.push_str(&match path.cost {
        // Two different floors, and they disagree: the heaviest chain by cost
        // is not always the longest one by steps.
        Some(cost) => format!(
            "  critical path   {} nodes and {cost:.0} cost, the heaviest chain; {} nodes on the longest\n",
            path.nodes, path.longest_chain,
        ),
        None => format!("  critical path   {} nodes, the longest chain\n", path.nodes),
    });
    if let (Some(total), Some(speedup)) = (path.total_cost, path.ideal_speedup) {
        out.push_str(&format!(
            "                  {total:.0} cost in total, so {speedup:.1}x is the ceiling on any amount of parallelism\n",
        ));
    }
    out.push_str(&format!(
        "  parallelism     {} levels, widest {}, mean {:.1} nodes per level\n",
        analysis.width.levels, analysis.width.max, analysis.width.mean,
    ));
    for share in &analysis.concentration.top {
        out.push_str(&format!(
            "  concentration   {} of {} nodes hold {:.0}% of all blast radius\n",
            share.nodes, analysis.nodes, share.share * 100.0,
        ));
    }

    if analysis.leverage.is_empty() {
        out.push_str(
            "\nNo node carries a cost, so cheap-but-central nodes cannot be told from expensive ones.\n",
        );
    } else {
        out.push_str(&format!(
            "\nHighest leverage: cheap nodes that invalidate a lot ({} of {} nodes carry a cost)\n",
            analysis.leverage_costed, analysis.nodes,
        ));
        for (rank, node) in analysis.leverage.iter().take(top).enumerate() {
            out.push_str(&format!(
                "{:>4}. {} [{:.2}]\n        {}\n",
                rank + 1,
                node.label,
                node.score,
                node.explain(),
            ));
        }
    }

    out.push_str("\nLargest blast radius\n");
    for (rank, node) in analysis.ranked.iter().take(top).enumerate() {
        out.push_str(&format!(
            "{:>4}. {} - {} dependents, {} of them direct, at depth {}{}\n",
            rank + 1,
            node.label,
            node.blast_radius,
            node.fan_out,
            node.depth,
            if node.merged > 1 {
                format!(" ({} files in one reference cycle)", node.merged)
            } else {
                String::new()
            },
        ));
    }
    out
}

pub fn diff(subject: &str, diff: &Diff, top: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{subject}: {} added, {} removed, {} modified\n",
        diff.added.len(),
        diff.removed.len(),
        diff.modified.len(),
    ));
    out.push_str(&format!(
        "  invalidated   {} of {} nodes ({:.0}%)\n",
        diff.invalidated,
        diff.after_nodes,
        diff.invalidated_share * 100.0,
    ));

    if diff.frontier.is_empty() {
        out.push_str("\nNothing changed.\n");
        return out;
    }
    out.push_str(
        "\nCauses: nodes that changed while everything below them held still\n",
    );
    for (rank, cause) in diff.frontier.iter().take(top).enumerate() {
        out.push_str(&format!(
            "{:>4}. {} - invalidated {}{}\n",
            rank + 1,
            cause.label,
            cause.dependents.len(),
            cause
                .cost
                .map(|cost| format!(", own cost {cost:.0}"))
                .unwrap_or_default(),
        ));
    }
    if diff.frontier.len() > top {
        out.push_str(&format!(
            "      ... and {} more\n",
            diff.frontier.len() - top,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use dag_complexity_core::{Builder, Node};

    /// The shape the human report promises: a bounded list where every entry
    /// carries the reason it is there, so a reader can disagree with it.
    #[test]
    fn every_ranked_entry_states_why_it_is_ranked() {
        let mut builder = Builder::new();
        let hub = builder.node(Node::new("hub").with_cost(1.0));
        for leaf in 0..50 {
            let id = builder.node(Node::new(format!("leaf-{leaf}")).with_cost(500.0));
            builder.depends_on(id, hub);
        }
        let analysis = builder.build().expect("acyclic").analyze();

        let rendered = super::analysis("test", &analysis, 3);
        assert!(rendered.contains("hub"), "{rendered}");
        assert!(rendered.contains("rebuild when it changes"), "{rendered}");
        // `--top 3` bounds both lists; 51 nodes must not all be printed.
        assert!(
            !rendered.contains("leaf-49"),
            "the list has to stay bounded:\n{rendered}",
        );
    }
}
