//! What these defend: the ranking that separates a compiler from a defect, the
//! two shapes fan-out and blast radius disagree about, the parallelism ceiling,
//! and the diff's claim about which change actually caused a rebuild.

use crate::{Builder, Change, Dag, GraphFile, Node, diff};

/// A graph shaped like ENG-10647: one trivial input wired into every unit,
/// plus one genuinely expensive shared dependency with the same reach.
fn one_cheap_input_and_one_compiler(units: usize) -> Dag {
    let mut builder = Builder::new();
    let env = builder.node(Node::new("env-var").with_cost(0.01).with_kind("setup"));
    let compiler = builder.node(Node::new("rustc").with_cost(600.0).with_kind("toolchain"));
    for unit in 0..units {
        let id = builder.node(Node::new(format!("unit-{unit}")).with_cost(30.0));
        builder.depends_on(id, env);
        builder.depends_on(id, compiler);
    }
    builder.build().expect("acyclic")
}

#[test]
fn leverage_puts_the_trivial_node_above_the_compiler_they_both_feed() {
    let analysis = one_cheap_input_and_one_compiler(200).analyze();

    // Blast radius alone cannot tell them apart: both feed all 200 units.
    let env = analysis
        .ranked
        .iter()
        .find(|node| node.key == "env-var")
        .expect("present");
    let rustc = analysis
        .ranked
        .iter()
        .find(|node| node.key == "rustc")
        .expect("present");
    assert_eq!(env.blast_radius, rustc.blast_radius);

    // Leverage does: the compiler is expected, the trivial node is the defect.
    assert_eq!(analysis.leverage[0].key, "env-var");
    assert!(
        analysis.leverage[0].score > analysis.leverage[1].score,
        "the cheap node must outrank everything, got {:?}",
        analysis.leverage[0],
    );
    let scored = |key: &str| {
        analysis
            .leverage
            .iter()
            .find(|node| node.key == key)
            .expect("present")
            .score
    };
    // The two have identical reach, so the whole separation comes from cost.
    assert!(scored("env-var") > 0.9, "{}", scored("env-var"));
    assert!(
        scored("env-var") > scored("rustc") * 100.0,
        "the compiler must be orders below the trivial node: {} vs {}",
        scored("rustc"),
        scored("env-var"),
    );
    assert!(
        analysis.leverage[0]
            .explain()
            .contains("rebuild when it changes"),
        "the ranking has to say why: {}",
        analysis.leverage[0].explain(),
    );
}

#[test]
fn a_node_with_no_cost_is_left_out_rather_than_assumed_free() {
    let mut builder = Builder::new();
    let unmeasured = builder.node(Node::new("unmeasured"));
    let measured = builder.node(Node::new("measured").with_cost(1.0));
    builder.depends_on(measured, unmeasured);
    let analysis = builder.build().expect("acyclic").analyze();

    assert_eq!(analysis.leverage_costed, 1);
    assert!(
        analysis.leverage.iter().all(|node| node.key != "unmeasured"),
        "an uncosted node must not be ranked as if it were free",
    );
}

#[test]
fn fan_out_separates_a_star_from_a_chain() {
    let mut star = Builder::new();
    let hub = star.node(Node::new("hub"));
    for leaf in 0..5 {
        let id = star.node(Node::new(format!("leaf-{leaf}")));
        star.depends_on(id, hub);
    }
    let star = star.build().expect("acyclic").analyze();

    let mut chain = Builder::new();
    let mut previous = chain.node(Node::new("link-0"));
    for link in 1..6 {
        let id = chain.node(Node::new(format!("link-{link}")));
        chain.depends_on(id, previous);
        previous = id;
    }
    let chain = chain.build().expect("acyclic").analyze();

    assert_eq!(star.ranked[0].blast_radius, chain.ranked[0].blast_radius);
    assert_eq!(star.ranked[0].fan_out, 5);
    assert_eq!(chain.ranked[0].fan_out, 1);
    assert_eq!(star.width.max, 5);
    assert_eq!(chain.width.max, 1);
}

#[test]
fn the_critical_path_is_the_floor_and_names_its_own_chain() {
    let mut builder = Builder::new();
    let base = builder.node(Node::new("base").with_cost(60.0));
    let slow = builder.node(Node::new("slow").with_cost(180.0));
    builder.depends_on(slow, base);
    for wide in 0..10 {
        let id = builder.node(Node::new(format!("wide-{wide}")).with_cost(5.0));
        builder.depends_on(id, base);
    }
    let analysis = builder.build().expect("acyclic").analyze();

    let path = analysis.critical_path;
    assert_eq!(path.cost, Some(240.0));
    assert_eq!(path.path, vec!["base".to_owned(), "slow".to_owned()]);
    // 60 + 180 + 10*5 = 290 on one worker, against a 240 floor.
    assert_eq!(path.total_cost, Some(290.0));
    assert!(path.ideal_speedup.is_some_and(|speedup| speedup < 1.3));
}

#[test]
fn a_cycle_is_refused_and_named() {
    let mut builder = Builder::new();
    let left = builder.node(Node::new("left"));
    let right = builder.node(Node::new("right"));
    builder.depends_on(left, right);
    builder.depends_on(right, left);

    let error = builder.build().expect_err("cyclic");
    let rendered = error.to_string();
    assert!(rendered.contains("left"), "{rendered}");
    assert!(rendered.contains("right"), "{rendered}");
}

#[test]
fn a_reference_cycle_condenses_into_one_node_that_keeps_its_members() {
    let mut builder = Builder::new();
    let left = builder.node(Node::new("left.rs").with_cost(10.0).with_version("l1"));
    let right = builder.node(Node::new("right.rs").with_cost(4.0).with_version("r1"));
    let user = builder.node(Node::new("user.rs").with_cost(1.0).with_version("u1"));
    builder.depends_on(left, right);
    builder.depends_on(right, left);
    builder.depends_on(user, left);

    let dag = builder.build_condensed().expect("condensing cannot fail on a cycle");
    assert_eq!(dag.len(), 2);
    let cycle = dag.node(dag.id("left.rs").expect("named for its first member"));
    assert_eq!(cycle.members, vec!["left.rs", "right.rs"]);
    // Touch either file and both recompile, so the pair costs and invalidates
    // as one thing.
    assert_eq!(cycle.cost, Some(14.0));
    assert_eq!(dag.analyze().ranked[0].merged, 2);
    assert_eq!(dag.analyze().ranked[0].blast_radius, 1);
}

#[test]
fn condensing_leaves_an_already_acyclic_graph_alone() {
    let mut builder = Builder::new();
    let base = builder.node(Node::new("base"));
    let top = builder.node(Node::new("top"));
    builder.depends_on(top, base);

    let dag = builder.build_condensed().expect("acyclic");
    assert_eq!(dag.len(), 2);
    assert!(dag.nodes().all(|node| node.members.is_empty()));
}

#[test]
fn a_duplicate_key_is_refused_rather_than_merged() {
    let mut builder = Builder::new();
    builder.node(Node::new("same"));
    builder.node(Node::new("same"));
    assert_eq!(
        builder.build().expect_err("duplicate").to_string(),
        "two nodes share the key same",
    );
}

/// `edited -> middle -> top`, where `middle` and `top` only moved because
/// `edited` did.
fn chain_with(edited_version: &str) -> Dag {
    let mut builder = Builder::new();
    let edited = builder.node(Node::new("edited").with_version(edited_version));
    let untouched = builder.node(Node::new("untouched").with_version("v1"));
    let middle = builder.node(Node::new("middle").with_version(edited_version));
    let top = builder.node(Node::new("top").with_version(edited_version));
    builder.depends_on(middle, edited);
    builder.depends_on(middle, untouched);
    builder.depends_on(top, middle);
    builder.build().expect("acyclic")
}

#[test]
fn the_diff_blames_the_edit_not_everything_downstream_of_it() {
    let report = diff(&chain_with("v1"), &chain_with("v2"), None);

    assert_eq!(report.modified, vec!["edited", "middle", "top"]);
    assert_eq!(report.invalidated, 3);
    assert_eq!(
        report.frontier.len(),
        1,
        "only the node whose own inputs held still is a cause: {:?}",
        report.frontier,
    );
    assert_eq!(report.frontier[0].key, "edited");
    assert_eq!(report.frontier[0].change, Change::Modified);
    assert_eq!(report.frontier[0].dependents, vec!["middle", "top"]);
}

#[test]
fn targets_narrow_a_cause_to_what_the_caller_asked_to_build() {
    let report = diff(
        &chain_with("v1"),
        &chain_with("v2"),
        Some(&["top".to_owned()]),
    );

    assert_eq!(report.frontier.len(), 1);
    assert_eq!(report.frontier[0].dependents, vec!["top"]);
}

#[test]
fn a_target_that_moved_on_its_own_is_its_own_cause() {
    let before = {
        let mut builder = Builder::new();
        builder.node(Node::new("check").with_version("v1"));
        builder.build().expect("acyclic")
    };
    let after = {
        let mut builder = Builder::new();
        builder.node(Node::new("check").with_version("v2"));
        builder.build().expect("acyclic")
    };

    let report = diff(&before, &after, Some(&["check".to_owned()]));
    assert_eq!(report.frontier.len(), 1);
    assert_eq!(report.frontier[0].dependents, vec!["check"]);
}

#[test]
fn the_json_interchange_survives_a_round_trip() {
    let dag = one_cheap_input_and_one_compiler(3);
    let text = serde_json::to_string(&GraphFile::from(&dag)).expect("serializable");
    let restored = serde_json::from_str::<GraphFile>(&text)
        .expect("parses")
        .into_dag()
        .expect("acyclic");

    assert_eq!(
        serde_json::to_value(dag.analyze().ranked).expect("serializable"),
        serde_json::to_value(restored.analyze().ranked).expect("serializable"),
    );
}

#[test]
fn an_edge_may_name_a_node_the_file_never_declared() {
    let file: GraphFile = serde_json::from_str(
        r#"{"nodes": [{"key": "a"}], "edges": [{"dependent": "a", "dependency": "b"}]}"#,
    )
    .expect("parses");
    let dag = file.into_dag().expect("acyclic");

    assert_eq!(dag.len(), 2);
    assert_eq!(dag.analyze().ranked[0].key, "b");
}
