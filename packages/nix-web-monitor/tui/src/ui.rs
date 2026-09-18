//! Rendering. A pure function of [`App`] plus the current time, so the same
//! code drives a real terminal and the `--snapshot` capture used in reviews.

use crate::App;
use nix_web_monitor_parser::BuildStatus;
use nix_web_monitor_parser::global::{GlobalBuild, GlobalBuildKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};

const ACCENT: Color = Color::Cyan;

/// A store URI is far too long for a column and its interesting part is the
/// host: `ssh-ng://root@vc1-nix?max-connections=1` -> `vc1-nix`.
pub fn short_host(host: &str) -> String {
    let after_scheme = host.split_once("://").map_or(host, |(_, rest)| rest);
    let after_user = after_scheme.rsplit('@').next().unwrap_or(after_scheme);
    let before_path = after_user.split(['?', '/']).next().unwrap_or(after_user);
    let before_port = before_path.rsplit_once(':').map_or(before_path, |(h, _)| h);
    if before_port.is_empty() {
        host.to_owned()
    } else {
        before_port.to_owned()
    }
}

pub fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{}.{}s", s, (ms % 1000) / 100)
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The human name in `<hash>-<name>.drv`.
fn drv_name(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    let base = base.strip_suffix(".drv").unwrap_or(base);
    base.split_once('-')
        .map_or_else(|| base.to_owned(), |(_, name)| name.to_owned())
}

pub fn draw(f: &mut Frame, app: &App, now_ms: u64) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(5),
    ])
    .areas(f.area());

    draw_header(f, header, app);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(body);
    draw_builds(f, left, app, now_ms);
    draw_dag(f, right, app, now_ms);
    draw_footer(f, footer, app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = Vec::new();
    if let Some(view) = &app.view {
        let c = view.counts;
        spans.push(Span::styled(
            format!(" {} building ", c.running),
            Style::new().fg(Color::Black).bg(ACCENT).bold(),
        ));
        spans.push(Span::raw(format!("  {} done", c.succeeded)));
        if c.planned > 0 {
            spans.push(Span::raw(format!("  {} planned", c.planned)));
        }
        if c.failed > 0 {
            spans.push(Span::styled(
                format!("  {} failed", c.failed),
                Style::new().fg(Color::Red).bold(),
            ));
        }
    }
    if let Some(g) = &app.global {
        let n = g.builds.len();
        spans.push(Span::styled(
            format!("   {n} goals machine-wide"),
            Style::new().fg(Color::Magenta),
        ));
    }
    if let Some(e) = &app.global_error {
        spans.push(Span::styled(
            format!("   machine view: {}", truncate(e, 40)),
            Style::new().fg(Color::Red),
        ));
    }
    if spans.is_empty() {
        spans.push(Span::styled(
            " waiting for the first poll ",
            Style::new().fg(Color::DarkGray),
        ));
    }

    let block = Block::bordered().title(Line::from(vec![
        Span::styled(" nix-tui ", Style::new().fg(ACCENT).bold()),
        Span::raw(truncate(&app.title, area.width.saturating_sub(14) as usize)),
        Span::raw(" "),
    ]));
    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

/// One row per derivation. Two sources feed this: the wrapped command's
/// `BuildView` (which knows the phase and the remote builder) and the
/// machine-wide goal list (which knows the true start time and the requesting
/// user, including for builds nobody here launched).
fn draw_builds(f: &mut Frame, area: Rect, app: &App, now_ms: u64) {
    // Paired with elapsed so the table can be sorted longest-first: on a busy
    // machine the screen holds maybe twenty of thirty-odd rows, and the ones
    // worth keeping are the slow builds, not an arbitrary map order.
    let mut rows: Vec<(u64, Row)> = Vec::new();

    if let Some(view) = &app.view {
        for build in view
            .builds
            .iter()
            .filter(|b| b.status == BuildStatus::Running)
        {
            let elapsed = now_ms.saturating_sub(build.started_at_ms);
            rows.push((
                elapsed,
                Row::new(vec![
                    Cell::from(format!("{:>9}", fmt_ms(elapsed))).style(elapsed_style(elapsed)),
                    host_cell(build.host.as_deref()),
                    Cell::from(truncate(&build.name, 32)).style(Style::new().bold()),
                    Cell::from(build.phase.clone().unwrap_or_default())
                        .style(Style::new().fg(Color::Blue)),
                    Cell::from(format!("{} log lines", build.log_count))
                        .style(Style::new().fg(Color::DarkGray)),
                ]),
            ));
        }
    }

    if let Some(g) = &app.global {
        // Anything the wrapped command already showed is skipped, so a build
        // is one row whichever source saw it first.
        let known: Vec<&str> = app
            .view
            .iter()
            .flat_map(|v| v.builds.iter())
            .filter(|b| b.status == BuildStatus::Running)
            .map(|b| b.derivation.as_str())
            .collect();
        for build in &g.builds {
            let Some(path) = goal_path(build) else {
                continue;
            };
            if known.contains(&path) {
                continue;
            }
            let elapsed = build.start_time.map_or(0, |t| {
                now_ms.saturating_sub((t.max(0) as u64).saturating_mul(1000))
            });
            let kind = match build.kind {
                GlobalBuildKind::Substitution => "substituting",
                _ => "",
            };
            rows.push((
                elapsed,
                Row::new(vec![
                    Cell::from(format!("{:>9}", fmt_ms(elapsed))).style(elapsed_style(elapsed)),
                    host_cell(None),
                    Cell::from(truncate(&drv_name(path), 32)),
                    Cell::from(kind).style(Style::new().fg(Color::Blue)),
                    Cell::from(format!(
                        "pid {} · {}",
                        build.pid.unwrap_or(0),
                        build.user.clone().unwrap_or_else(|| "?".to_owned())
                    ))
                    .style(Style::new().fg(Color::DarkGray)),
                ]),
            ));
        }
    }

    let title = format!(" builds ({} running) ", rows.len());
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    let table = Table::new(
        rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
        [
            Constraint::Length(9),
            Constraint::Length(14),
            Constraint::Length(32),
            Constraint::Length(12),
            Constraint::Min(6),
        ],
    )
    .header(
        Row::new(vec!["  ELAPSED", "WHERE", "DERIVATION", "PHASE", "DETAIL"])
            .style(Style::new().fg(ACCENT).add_modifier(Modifier::UNDERLINED)),
    )
    .block(Block::bordered().title(Span::styled(title, Style::new().fg(ACCENT))));
    f.render_widget(table, area);
}

fn goal_path(build: &GlobalBuild) -> Option<&str> {
    build.drv_path.as_deref().or(build.store_path.as_deref())
}

/// A build that has been going for minutes is the one worth looking at, and
/// colour is the only affordance that survives a glance at a full screen.
fn elapsed_style(ms: u64) -> Style {
    match ms / 1000 {
        0..=59 => Style::new().fg(Color::Green),
        60..=299 => Style::new().fg(Color::Yellow),
        _ => Style::new().fg(Color::Red).bold(),
    }
}

fn host_cell(host: Option<&str>) -> Cell<'static> {
    match host {
        Some(h) => {
            Cell::from(truncate(&short_host(h), 14)).style(Style::new().fg(Color::Magenta).bold())
        }
        None => Cell::from("local").style(Style::new().fg(Color::DarkGray)),
    }
}

/// The in-flight dependency DAG, drawn from the why-chains the status
/// directory reports: root goal at depth 0, each hop indented under it, the
/// derivation actually building marked and timed. These are real goal edges,
/// so no evaluation or store query is needed to draw them.
fn draw_dag(f: &mut Frame, area: Rect, app: &App, now_ms: u64) {
    let mut children: Vec<(&str, &str)> = app
        .dag_parent
        .iter()
        .map(|(child, parent)| (parent.as_str(), child.as_str()))
        .collect();
    children.sort_unstable();

    let running_since: std::collections::BTreeMap<&str, u64> = app
        .global
        .iter()
        .flat_map(|g| g.builds.iter())
        .filter_map(|b| {
            Some((
                goal_path(b)?,
                b.start_time.map_or(0, |t| t.max(0) as u64 * 1000),
            ))
        })
        .collect();

    let mut lines: Vec<Line> = Vec::new();
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let budget = area.height.saturating_sub(2) as usize;
    let width = area.width.saturating_sub(3) as usize;

    let roots: Vec<&str> = app
        .dag_roots
        .keys()
        .map(String::as_str)
        .filter(|r| !app.dag_parent.contains_key(*r))
        .collect();

    for root in roots {
        // An explicit stack, not recursion: a why-chain is short in practice
        // but `seen` is also what stops a cycle from looping forever.
        let mut stack = vec![(0usize, root)];
        while let Some((depth, node)) = stack.pop() {
            if lines.len() >= budget {
                break;
            }
            if !seen.insert(node) {
                continue;
            }
            let indent = "  ".repeat(depth);
            let branch = if depth == 0 { "" } else { "└ " };
            match running_since.get(node) {
                Some(started) => {
                    let tail = format!("  {}", fmt_ms(now_ms.saturating_sub(*started)));
                    let room = width.saturating_sub(indent.len() + branch.len() + 2 + tail.len());
                    lines.push(Line::from(vec![
                        Span::raw(format!("{indent}{branch}")),
                        Span::styled("● ", Style::new().fg(Color::Green)),
                        Span::styled(truncate(&drv_name(node), room.max(8)), Style::new().bold()),
                        Span::styled(tail, Style::new().fg(Color::Yellow)),
                    ]));
                }
                None => {
                    let room = width.saturating_sub(indent.len() + branch.len() + 2);
                    lines.push(Line::from(vec![
                        Span::raw(format!("{indent}{branch}")),
                        Span::styled("· ", Style::new().fg(Color::DarkGray)),
                        Span::styled(
                            truncate(&drv_name(node), room.max(8)),
                            Style::new().fg(Color::DarkGray),
                        ),
                    ]));
                }
            }
            for (parent, child) in children.iter().rev() {
                if *parent == node {
                    stack.push((depth + 1, child));
                }
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::styled(
            "  (no goal ancestry reported yet)",
            Style::new().fg(Color::DarkGray),
        ));
    }

    let title = format!(
        " dependency DAG ({} nodes) ",
        app.dag_parent.len() + app.dag_roots.len()
    );
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(Span::styled(title, Style::new().fg(ACCENT)))),
        area,
    );
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(view) = &app.view {
        for activity in view.activities.iter().take(2) {
            let pct = if activity.expected > 0 {
                activity.done * 100 / activity.expected
            } else {
                0
            };
            lines.push(Line::from(vec![
                Span::styled("  ↓ ", Style::new().fg(Color::Blue)),
                Span::raw(truncate(&activity.text, 80)),
                Span::styled(format!("  {pct}%"), Style::new().fg(Color::Blue)),
            ]));
        }
        for error in view.errors.iter().rev().take(2) {
            lines.push(Line::styled(
                format!("  ! {}", truncate(error, 110)),
                Style::new().fg(Color::Red),
            ));
        }
    }
    if let Some(msg) = &app.finished {
        lines.push(Line::styled(
            format!("  {msg}"),
            Style::new().fg(Color::Yellow).bold(),
        ));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "  no transfers or errors",
            Style::new().fg(Color::DarkGray),
        ));
    }

    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(Span::styled(
                    " transfers / errors ",
                    Style::new().fg(ACCENT),
                ))
                .title_bottom(
                    Line::from(Span::styled(" q quit ", Style::new().fg(Color::DarkGray)))
                        .right_aligned(),
                ),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_uris_shorten_to_hosts() {
        assert_eq!(short_host("ssh-ng://root@vc1-nix"), "vc1-nix");
        assert_eq!(
            short_host("ssh-ng://root@vm-builder?max-connections=1"),
            "vm-builder"
        );
        assert_eq!(
            short_host("ssh://nix@hil-compute-2.ix.internal:2222"),
            "hil-compute-2.ix.internal"
        );
    }

    #[test]
    fn durations_read_at_a_glance() {
        assert_eq!(fmt_ms(4_200), "4.2s");
        assert_eq!(fmt_ms(125_000), "2m05s");
        assert_eq!(fmt_ms(7_400_000), "2h03m");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        // Six characters out, ellipsis included -- the budget is characters,
        // not bytes, so the accented ones must not each cost two.
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
        assert_eq!(truncate("héllo wörld", 6).chars().count(), 6);
        assert_eq!(truncate("abc", 10), "abc");
    }
}
