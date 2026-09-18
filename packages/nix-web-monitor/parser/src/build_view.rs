//! The compact build-tree render model.
//!
//! [`MonitorSnapshot`](crate::MonitorSnapshot) is the full state the web UI
//! consumes over its delta protocol: every activity, a 500-line log tail, the
//! dependency DAG, the switch activation subtree. That is far
//! more than a small "what is this build doing right now" pane needs, and its
//! log tail changes every line -- pathological for a dashboard body that is
//! diffed wholesale on each tick.
//!
//! [`BuildView`] is the projection of that state a build-tree pane actually
//! renders: the derivations (with status and phase), a few in-flight non-build
//! activities (fetches, copies) with their progress, the aggregate counters,
//! and a capped error list. It is owned here, alongside the parser, so the
//! kernel's headless emitter and the dashboard's `nix-build` renderer share one
//! definition of the model instead of each re-deriving it. Bounded by
//! construction (`ERROR_LIMIT`, `ACTIVITY_LIMIT`), so the serialized body stays
//! small no matter how verbose the build.

use serde::{Deserialize, Serialize};

use crate::{ActivityStatus, BuildStatus, MonitorState};

/// Most recent errors carried in a view. A build usually fails on one root
/// error; the cap bounds a warning-heavy eval without dropping the failure.
const ERROR_LIMIT: usize = 20;

/// In-flight non-build activities carried in a view (fetches, copies,
/// substitutions). The build rows carry the bulk of the tree; this is the
/// "what else is happening" strip, so a handful is plenty and keeps the body
/// bounded when hundreds of paths are fetched at once.
const ACTIVITY_LIMIT: usize = 24;

/// One derivation row in a [`BuildView`]: the pane renders these as the tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildRow {
    /// The `.drv` store path, the row's stable identity.
    pub derivation: String,
    /// The human name pulled from the store path (`<hash>-<name>.drv` -> `name`),
    /// so the pane can show `ripgrep-14.1.0` rather than the full hashed path.
    pub name: String,
    pub status: BuildStatus,
    /// The build's current phase (`unpackPhase`, `buildPhase`, ...) when Nix has
    /// reported one, else `None`.
    pub phase: Option<String>,
    /// The remote builder host, when the build ran off-machine.
    pub host: Option<String>,
    /// Unix milliseconds when this derivation started building, so a renderer
    /// can show elapsed time. A terminal pane redraws many times a second and
    /// must not be handed a pre-formatted duration that is stale by the next
    /// frame, so the view carries the instant and the renderer does the clock.
    /// Stamped when the build activity starts (0 for a still-planned row).
    pub started_at_ms: u64,
    /// Unix milliseconds when it stopped, or `None` while it is still running.
    /// A settled row's duration is `stopped - started`; a live row's is
    /// `now - started`.
    pub stopped_at_ms: Option<u64>,
    /// How many build-log lines this derivation has emitted; a rough liveness
    /// signal for a build with no phase reporting.
    pub log_count: usize,
    /// Whether Nix resolved this derivation before building it (content
    /// addressed), so the pane can badge the folded row rather than let it look
    /// like a stray.
    pub content_addressed: bool,
}

/// One in-flight non-build activity (a fetch, a copy, a substitution). Kept
/// separate from [`BuildRow`] because these have progress bars, not phases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityRow {
    /// The activity kind name (`copy_path`, `substitute`, `file_transfer`, ...).
    pub kind: String,
    /// The activity's own text (`copying '/nix/store/...'`), trimmed of ANSI.
    pub text: String,
    /// Work done so far, when the activity reports byte/item progress.
    pub done: i64,
    /// Total work expected, when known (`0` when the activity is unsized).
    pub expected: i64,
    /// Bytes the server measured for a "copying … to the store" activity Nix
    /// reports without progress; `None` on every other activity.
    pub size_bytes: Option<i64>,
}

/// The compact render model for a build-tree pane: everything the pane draws,
/// and nothing it does not. Produced by [`MonitorState::build_view`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildView {
    /// The Nix invocation label (`nix build .#ix`), the tree's root.
    pub command: String,
    /// Derivation rows: planned, running, and settled, in first-seen order.
    pub builds: Vec<BuildRow>,
    /// In-flight non-build activities (fetches, copies), newest first, capped at
    /// [`ACTIVITY_LIMIT`].
    pub activities: Vec<ActivityRow>,
    /// Count of build rows in each status, so the pane's summary bar needs no
    /// second pass over `builds`.
    pub counts: BuildCounts,
    /// The most recent error-level messages, capped at [`ERROR_LIMIT`].
    pub errors: Vec<String>,
    /// Whether the wrapped Nix process has exited.
    pub finished: bool,
    /// The wrapped process's exit status once it has exited (`None` while
    /// running, or when Nix reported no code).
    pub exit_code: Option<i32>,
}

/// Build-row counts by status, for the pane's summary bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildCounts {
    pub planned: usize,
    pub running: usize,
    pub stopped: usize,
    pub succeeded: usize,
    pub failed: usize,
}

impl MonitorState {
    /// Project the current state into the compact [`BuildView`] a build-tree
    /// pane renders. Bounded by construction, unlike [`snapshot`](Self::snapshot):
    /// the log tail and dependency DAG are dropped, and errors and
    /// non-build activities are capped, so the serialized body stays small on the
    /// dashboard's wholesale-diffed pane path regardless of build verbosity.
    #[must_use]
    pub fn build_view(&self) -> BuildView {
        let mut counts = BuildCounts::default();
        let builds: Vec<BuildRow> = self
            .builds
            .values()
            .map(|build| {
                match build.status {
                    BuildStatus::Planned => counts.planned += 1,
                    BuildStatus::Running => counts.running += 1,
                    BuildStatus::Stopped => counts.stopped += 1,
                    BuildStatus::Succeeded => counts.succeeded += 1,
                    BuildStatus::Failed => counts.failed += 1,
                }
                BuildRow {
                    name: derivation_name(&build.derivation),
                    derivation: build.derivation.clone(),
                    status: build.status,
                    phase: build.phase.clone(),
                    host: build.host.clone(),
                    started_at_ms: build.started_at_ms,
                    stopped_at_ms: build.stopped_at_ms,
                    log_count: build.log_count,
                    content_addressed: build.content_addressed,
                }
            })
            .collect();

        // Running non-build activities that carry their own progress: fetches,
        // copies, substitutions. A build's own activity is represented by its
        // build row, so skip anything already attached to a build. Newest first
        // so the strip shows what just started, capped so a fan-out of hundreds
        // of fetches cannot unbound the body.
        let mut activities: Vec<ActivityRow> = self
            .activities
            .values()
            .filter(|activity| {
                activity.status == ActivityStatus::Running
                    && activity.build.is_none()
                    && activity.activity_type.name != "build"
                    && !activity.text.is_empty()
            })
            .map(|activity| {
                let progress = activity.progress.unwrap_or_default();
                ActivityRow {
                    kind: activity.activity_type.name.clone(),
                    text: activity.text.clone(),
                    done: progress.done,
                    expected: progress.expected,
                    size_bytes: activity.size_bytes,
                }
            })
            .collect();
        // `activities` is a BTreeMap keyed by ascending id, so later ids are
        // more recent; reverse for newest-first, then cap.
        activities.reverse();
        activities.truncate(ACTIVITY_LIMIT);

        let errors = self
            .errors
            .iter()
            .rev()
            .take(ERROR_LIMIT)
            .rev()
            .cloned()
            .collect();

        BuildView {
            command: self.command_label().to_owned(),
            builds,
            activities,
            counts,
            errors,
            finished: self.finished,
            exit_code: self.exit_code,
        }
    }
}

/// The human name in a `<hash>-<name>.drv` store path (`ripgrep-14.1.0`), or the
/// whole path when it does not match that shape. Splits once on the first `-`
/// after the `/nix/store/<hash>` prefix and strips the `.drv` suffix, matching
/// how Nix itself names a derivation in its output.
fn derivation_name(derivation: &str) -> String {
    let base = derivation
        .rsplit('/')
        .next()
        .unwrap_or(derivation)
        .strip_suffix(".drv")
        .unwrap_or(derivation);
    // `<hash>-<name>`: the hash has no `-`, so the first `-` splits it off.
    base.split_once('-')
        .map_or_else(|| base.to_owned(), |(_hash, name)| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_start(id: u64, drv: &str) -> String {
        format!(
            r#"@nix {{"action":"start","fields":["{drv}","local",1,1],"id":{id},"level":3,"text":"building '{drv}'","type":105}}"#
        )
    }

    #[test]
    fn build_view_projects_rows_counts_and_name() {
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ripgrep-14.1.0.drv";
        let mut state = MonitorState::new("nix build .#ripgrep".to_owned());
        state.apply_line(&build_start(7, drv));
        state.apply_line(r#"@nix {"action":"result","fields":["buildPhase"],"id":7,"type":104}"#);

        let view = state.build_view();
        assert_eq!(view.command, "nix build .#ripgrep");
        assert_eq!(view.builds.len(), 1);
        assert_eq!(view.builds[0].name, "ripgrep-14.1.0");
        assert_eq!(view.builds[0].status, BuildStatus::Running);
        assert_eq!(view.builds[0].phase.as_deref(), Some("buildPhase"));
        assert_eq!(view.counts.running, 1);
        assert!(!view.finished);
    }

    #[test]
    fn build_view_reports_finish_and_exit_code() {
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo.drv";
        let mut state = MonitorState::default();
        state.apply_line(&build_start(7, drv));
        state.apply_line(r#"@nix {"action":"stop","id":7}"#);
        state.finish(Some(0));

        let view = state.build_view();
        assert!(view.finished);
        assert_eq!(view.exit_code, Some(0));
        assert_eq!(view.counts.succeeded, 1);
        assert_eq!(view.builds[0].status, BuildStatus::Succeeded);
    }

    #[test]
    fn build_view_caps_errors_to_the_most_recent() {
        let mut state = MonitorState::default();
        for i in 0..(ERROR_LIMIT + 5) {
            state.apply_line(&format!(
                r#"@nix {{"action":"msg","level":0,"msg":"error number {i}"}}"#
            ));
        }
        let view = state.build_view();
        assert_eq!(view.errors.len(), ERROR_LIMIT);
        // The tail is kept: the last error pushed is the last one shown.
        assert_eq!(
            view.errors.last().map(String::as_str),
            Some(&format!("error number {}", ERROR_LIMIT + 4)[..])
        );
    }

    #[test]
    fn build_view_excludes_build_activities_from_the_activity_strip() {
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo.drv";
        let mut state = MonitorState::default();
        state.apply_line(&build_start(7, drv));
        // A fetch activity: no build attached, carries text.
        state.apply_line(
            r#"@nix {"action":"start","fields":[],"id":8,"level":3,"text":"downloading 'https://example/x'","type":101}"#,
        );

        let view = state.build_view();
        assert_eq!(view.builds.len(), 1, "the build is a build row");
        assert_eq!(
            view.activities.len(),
            1,
            "only the fetch is an activity row"
        );
        assert_eq!(view.activities[0].kind, "file_transfer");
    }

    #[test]
    fn derivation_name_strips_hash_and_suffix() {
        assert_eq!(
            derivation_name("/nix/store/abcdef0123456789abcdef0123456789-ripgrep-14.1.0.drv"),
            "ripgrep-14.1.0"
        );
        // A path that does not match the shape is returned as-is.
        assert_eq!(derivation_name("weird"), "weird");
    }
}
