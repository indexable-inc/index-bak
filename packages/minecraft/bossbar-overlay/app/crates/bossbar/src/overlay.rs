//! The live overlay: one transparent, always-on-top, borderless window **per
//! bar**, each sized to just that bar. winit owns the windows and event loop; the
//! SQLite watcher runs on its own thread and wakes the loop with a fresh
//! `Vec<BossBar>` whenever the database changes.
//!
//! Because each window covers only its bar, the desktop is click-through
//! everywhere else automatically: there is simply no window to intercept the
//! mouse off a bar. Hovering a bar fires native `CursorEntered`/`CursorLeft` (the
//! bar paints opaque, the cursor becomes a grab hand); pressing it hands off to
//! the native `Window::drag_window`, so the OS owns the drag and the new position
//! is read back from `Moved` and persisted. A two-finger trackpad scroll over a
//! bar nudges it the same way: a scroll has no button for `drag_window` to own, so
//! the overlay moves the window itself ([`overlay_core::scroll_drag_delta`]). The
//! press/drag/click disambiguation is [`overlay_core::DragClick`]; the GPU
//! bring-up, surface config, and non-activating raise are [`overlay_core::window`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use overlay_core::glam::DVec2;
use overlay_core::wgpu;
use overlay_core::winit::application::ApplicationHandler;
use overlay_core::winit::dpi::{LogicalPosition, PhysicalPosition, PhysicalSize};
use overlay_core::winit::event::{
    ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent,
};
use overlay_core::winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use overlay_core::winit::window::{CursorIcon, Window, WindowId};
use overlay_core::{anim, window as ocwin, DragClick, HoverAnim, TexHandle};

use crate::bars::BossBar;
use crate::db;
use crate::gpu_core::GpuCore;
use crate::scene;
use crate::theme;

/// Top margin and vertical gap (logical points) for bars that have no pinned
/// position and fall back to the auto-stacked top-center column. `AUTO_TOP`
/// clears the macOS menu bar so the system does not nudge the window down (a
/// nudge would otherwise read as a move).
const AUTO_TOP: f64 = 40.0;
const AUTO_GAP: f64 = 6.0;

/// Hover grow/shrink time: an ease-out tween at the responsive end of the
/// feedback range. See the `animation` skill.
const GROW: Duration = Duration::from_millis(160);
/// Full breathing cycle while hovered. ~2.6s reads calm and alive, near a resting
/// heart rate, rather than urgent.
const BREATHE_PERIOD: Duration = Duration::from_millis(2600);
/// Largest animation step a single frame may apply, so a stall (the loop slept
/// for a tick) does not jump the hover; frames are otherwise ~16ms.
const MAX_STEP: Duration = Duration::from_millis(50);
/// Animation frame budget while something is moving (~60 fps).
const FRAME: Duration = Duration::from_millis(16);
/// Redraw cadence for a bar showing a live elapsed counter: once a second is
/// enough to advance a clock that ticks in seconds, and lets the loop sleep the
/// rest of the time.
const TICK: Duration = Duration::from_secs(1);
/// Pointer travel (physical px) past which a press becomes a drag rather than a
/// click. Below it, the press-release opens the bar's URL; at or above it, the
/// native window drag takes over.
const DRAG_THRESHOLD: f64 = 5.0;
/// How long a window must sit still after its last move before the overlay will
/// apply an externally-read position again. Must exceed the DB watch poll so the
/// watcher's lagged read-back of our own drag never snaps the window back.
const SETTLE: Duration = Duration::from_millis(700);

/// How long the "poof" despawn animation runs before the window is actually
/// removed. A bar that leaves the DB shrinks toward its center and fades out over
/// this window (ease-out) instead of vanishing in a single frame.
const POOF_DUR: Duration = Duration::from_millis(280);
/// How far the bar shrinks at the end of the poof (1.0 - this), so it implodes to
/// ~55% as it fades. Kept under 1.0 and centered, so it never clips the window.
const POOF_SHRINK: f32 = 0.45;

/// Current wall-clock time as Unix seconds, for the live elapsed counter. Before
/// the epoch (clock unset) it clamps to 0, which reads as a 0 counter.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
/// One bar's window and its surface.
struct BarWin {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    bar: BossBar,
    /// Resolved texture for `bar.icon` (the square avatar), re-resolved whenever
    /// the bar's icon path changes. `None` when the bar has no icon or it failed
    /// to load.
    icon_tex: Option<TexHandle>,
    /// Resolved texture set for `bar.theme`, re-resolved when the theme name
    /// changes (or when a previously missing set may have been imported).
    /// `None` renders the vanilla color sprites.
    theme_tex: Option<theme::ThemeSprites>,
    /// Hover target: the pointer is currently over this bar.
    hovered: bool,
    /// Hover amount animated toward `hovered` each frame. Drives the grow +
    /// opacity (via its eased value); the breathing fades in with it.
    hover_anim: HoverAnim,
    /// Timestamp of the last animation step, for frame-rate-independent easing.
    last: Instant,
    /// Press/drag/click disambiguation for this window's left-button gesture.
    gesture: DragClick,
    /// Last position we know the window holds (logical points): what we set, or
    /// where the OS placed it. Lets `Moved` skip echoes of our own placement.
    self_set: Option<LogicalPosition<f64>>,
    /// This bar carries a description, so it has a hover pop-down panel.
    has_description: bool,
    /// The window size (physical px) last requested for this bar. The size-settle
    /// pass recomputes the target each tick and resizes only when it changes, so
    /// growing for the hover panel and widening as a title (or its live counter)
    /// grows both flow through one place without per-frame resize churn.
    last_size: (u32, u32),
    /// When the window last moved (a `Moved` during a drag). The reconcile guard
    /// ignores externally-read positions until the window has been still for
    /// SETTLE, so the watcher's lagged read-back never snaps a live drag back.
    last_move: Instant,
    /// Set when the bar has left the DB and the window is playing its poof
    /// despawn. While `Some`, the window shrinks+fades from this instant and is
    /// removed once `POOF_DUR` elapses; it is cleared back to `None` if the bar
    /// reappears before the poof finishes.
    poof: Option<Instant>,
}

impl BarWin {
    /// Still moving: easing toward/away from hover, breathing while hovered, or
    /// playing the poof despawn (which must keep redrawing until it finishes).
    fn animating(&self) -> bool {
        self.hovered || !self.hover_anim.is_resting() || self.poof.is_some()
    }

    /// The window should be grown for the panel while the bar is hovered or still
    /// easing back from a hover, but only when it has a description AND the bar
    /// opts into the pop-down (`expandable`). A non-expandable bar stays bar-sized
    /// on hover with no box, even if it carries description text.
    fn wants_expanded(&self) -> bool {
        self.has_description && self.bar.expandable && self.animating()
    }
}

pub struct App {
    db: PathBuf,
    /// Logical pixel scale; multiplied by the monitor scale factor so the bars
    /// look the same physical size on HiDPI and standard displays. Fractional
    /// values are honored (e.g. 1.25 for 25% larger bars).
    base_scale: f32,
    proxy: EventLoopProxy<Vec<BossBar>>,
    instance: wgpu::Instance,
    core: Option<GpuCore>,
    wins: HashMap<i64, BarWin>,

    /// Monitor logical size, for centering auto-stacked bars.
    mon_logical: (f64, f64),
    scale_factor: f64,
    /// Physical sprite scale, `base_scale * scale_factor`. Kept fractional so a
    /// non-integer `base_scale` produces exactly the requested size.
    scale: f32,
    /// App start, the zero point for the continuous breathing phase.
    start: Instant,
    ready: bool,
}

impl App {
    /// Build a surface for `window`, creating the shared GPU core on first use.
    fn make_surface(
        &mut self,
        window: &Arc<Window>,
    ) -> (wgpu::Surface<'static>, wgpu::SurfaceConfiguration) {
        let surface = self
            .instance
            .create_surface(window.clone())
            .expect("create surface");

        if self.core.is_none() {
            self.core = Some(GpuCore::new(&self.instance, &surface));
        }

        let core = self.core.as_ref().expect("core just initialized");
        let size = window.inner_size();
        let config = ocwin::surface_config(core.format, core.alpha_mode, size.width, size.height);
        surface.configure(core.gpu.device(), &config);
        (surface, config)
    }

    /// Auto-stacked default position (logical points) for the `slot`-th bar that
    /// has no pinned location.
    fn auto_pos(&self, slot: usize, w_px: u32, h_px: u32) -> LogicalPosition<f64> {
        let wl = w_px as f64 / self.scale_factor;
        let hl = h_px as f64 / self.scale_factor;
        let x = ((self.mon_logical.0 - wl) / 2.0).max(0.0);
        let y = AUTO_TOP + slot as f64 * (hl + AUTO_GAP);
        LogicalPosition::new(x, y)
    }

    fn create_win(
        &mut self,
        event_loop: &ActiveEventLoop,
        bar: BossBar,
        slot: usize,
    ) -> Option<BarWin> {
        // Size the window to the title, not just the 182px bar: a long downtime
        // title is wider than the bar and would otherwise be clipped at the window
        // edge. Measured on the CPU font, so this works before the GPU core exists
        // (the first window is created before its surface).
        let title_w = scene::title_extent_px(&bar, self.scale, now_unix());
        let (w_px, h_px) = scene::bar_window_px(self.scale, title_w, !bar.icon.is_empty());
        let pos = match bar.pos {
            Some(p) => LogicalPosition::new(p.x, p.y),
            None => self.auto_pos(slot, w_px, h_px),
        };
        let attrs = ocwin::float_attributes("Boss Bar", w_px, h_px, Some(pos));
        let window = Arc::new(event_loop.create_window(attrs).ok()?);
        // Each bar is a background window, so hover (grow + breathe + description
        // panel) only reaches it through an always-active tracking area.
        ocwin::enable_background_hover(&window);
        let (surface, config) = self.make_surface(&window);
        window.request_redraw();
        let has_description = !bar.description.trim().is_empty();
        // `make_surface` guarantees the GPU core exists, so the icon and theme
        // can upload now.
        let icon_tex = self.core.as_mut().and_then(|c| c.icon(&bar.icon));
        let theme_tex = self.core.as_mut().and_then(|c| c.theme(&bar.theme));
        let now = Instant::now();
        Some(BarWin {
            window,
            surface,
            config,
            bar,
            icon_tex,
            theme_tex,
            hovered: false,
            hover_anim: HoverAnim::default(),
            last: now,
            gesture: DragClick::new(DRAG_THRESHOLD),
            self_set: Some(pos),
            has_description,
            // The window was just created at this size; the settle pass grows it
            // for the panel on hover and tracks any later title-width changes.
            last_size: (w_px, h_px),
            // Far enough in the past that a fresh window accepts an external
            // position immediately (the SETTLE guard only fires after a real move).
            last_move: now - SETTLE,
            poof: None,
        })
    }

    /// Diff the DB's visible bars against the live windows: drop windows for gone
    /// bars, create them for new bars, update data and (externally changed)
    /// position for existing ones.
    fn reconcile(&mut self, event_loop: &ActiveEventLoop, bars: Vec<BossBar>) {
        // A bar that left the DB is not dropped instantly: mark its window
        // poofing so it shrinks+fades, and remove it once the poof finishes (in
        // `about_to_wait`). request_redraw kicks the animation off now.
        let now_i = Instant::now();
        for (id, win) in self.wins.iter_mut() {
            if win.poof.is_none() && !bars.iter().any(|b| b.id == *id) {
                win.poof = Some(now_i);
                win.window.request_redraw();
            }
        }

        let mut slot = 0usize;
        for b in &bars {
            let this_slot = if b.pos.is_none() {
                let s = slot;
                slot += 1;
                s
            } else {
                0
            };

            if let Some(win) = self.wins.get_mut(&b.id) {
                // The bar is back in the DB; if it was mid-poof (removed then
                // re-added before the despawn finished), cancel the poof.
                if win.poof.take().is_some() {
                    win.window.request_redraw();
                }
                win.has_description = !b.description.trim().is_empty();
                // Re-resolve the avatar only when the icon path actually changed,
                // so a steady bar does not re-touch the cache each reconcile.
                if win.bar.icon != b.icon {
                    win.icon_tex = self.core.as_mut().and_then(|c| c.icon(&b.icon));
                }
                // Same for the theme; also retry while unresolved, so a theme
                // directory imported after the bar appeared is picked up (a
                // cache hit makes the steady-state retry cheap).
                if win.bar.theme != b.theme || (win.theme_tex.is_none() && !b.theme.is_empty()) {
                    win.theme_tex = self.core.as_mut().and_then(|c| c.theme(&b.theme));
                }
                win.bar = b.clone();
                // Honor a position set in the DB by something other than our own
                // drag (e.g. the CLI), but do not fight a live drag: while the
                // user is moving the window the DB still holds an older value (the
                // watcher reads it ~200ms behind), and forcing it would snap the
                // window back. So only apply an external position once the window
                // has been still for SETTLE, and skip the echo of our own move.
                ocwin::apply_stored_position(&win.window, &mut win.self_set, win.last_move, SETTLE, b.pos);
                // A title or description change can alter the window size (wider for
                // a longer title, taller for a panel). The settle pass in
                // `about_to_wait` recomputes the target from the new bar and
                // resizes if needed; the redraw wakes the loop so it runs now.
                win.window.request_redraw();
            } else if let Some(win) = self.create_win(event_loop, b.clone(), this_slot) {
                self.wins.insert(b.id, win);
            }
        }
    }

    fn bar_id_for(&self, wid: WindowId) -> Option<i64> {
        self.wins
            .iter()
            .find(|(_, w)| w.window.id() == wid)
            .map(|(id, _)| *id)
    }

    fn render_id(&mut self, id: i64) {
        let now = Instant::now();
        // Continuous breathing phase over the whole run, never reset, so it never
        // snaps when a bar starts or stops being hovered.
        let breathe = anim::breathe(now.duration_since(self.start), BREATHE_PERIOD);

        // Advance this bar's hover amount toward its target, then read its eased
        // value for the visible grow/opacity.
        let hover = {
            let Some(win) = self.wins.get_mut(&id) else {
                return;
            };
            let dt = now.duration_since(win.last).min(MAX_STEP);
            win.last = now;
            win.hover_anim
                .approach(if win.hovered { 1.0 } else { 0.0 }, dt, GROW);
            win.hover_anim.eased()
        };

        let Some(core) = self.core.as_ref() else {
            return;
        };
        let Some(win) = self.wins.get_mut(&id) else {
            return;
        };
        let frame = match win.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                win.surface.configure(core.gpu.device(), &win.config);
                return;
            }
            Err(e) => {
                eprintln!("bossbar-overlay: surface error: {e:?}");
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut quads = scene::build_one(
            &core.gpu,
            &core.textures,
            win.icon_tex,
            win.theme_tex.as_ref(),
            self.scale,
            win.config.width,
            win.config.height,
            now_unix(),
            &win.bar,
            hover,
            breathe,
        );
        // Poof despawn: shrink the whole bar toward the window center and fade it
        // out over POOF_DUR (ease-out), so a removed bar implodes instead of
        // blinking off. Applied as a post-pass on the laid-out quads, so the
        // scene builder stays unaware of the despawn.
        if let Some(t0) = win.poof {
            let t = (now.duration_since(t0).as_secs_f32() / POOF_DUR.as_secs_f32()).clamp(0.0, 1.0);
            let e = anim::ease_out_cubic(t);
            let cx = win.config.width as f32 * 0.5;
            let cy = win.config.height as f32 * 0.5;
            anim::scale_quads_about(&mut quads, cx, cy, 1.0 - POOF_SHRINK * e);
            let fade = 1.0 - e;
            for q in &mut quads {
                q.color[3] *= fade;
            }
        }
        let _ = core.gpu.draw(&view, win.config.width, win.config.height, &quads);
        frame.present();
    }

    /// A window moved. Persist it only when the move came from a user drag;
    /// otherwise (initial placement, OS menu-bar clamp) just record where the
    /// window actually sits so a later drag is measured from the right spot.
    /// `Moved` arrives at roughly the refresh rate during a drag and the write is
    /// cheap (see `db::set_position`), so every drag move is saved; the last one is
    /// the exact drop position.
    fn on_moved(&mut self, id: i64, pos: PhysicalPosition<i32>) {
        let Some(win) = self.wins.get_mut(&id) else {
            return;
        };
        let logical: LogicalPosition<f64> = pos.to_logical(win.window.scale_factor());
        let echo = win
            .self_set
            .is_some_and(|ss| (ss.x - logical.x).abs() < 0.5 && (ss.y - logical.y).abs() < 0.5);
        if echo {
            return;
        }
        win.self_set = Some(logical);
        if !win.gesture.dragging() {
            return; // OS-initiated placement, not a user drag
        }
        // Mark the window as moving now, so the reconcile guard keeps the watcher's
        // lagged read-back from snapping it back mid-drag.
        win.last_move = Instant::now();
        let dv = DVec2::new(logical.x, logical.y);
        win.bar.pos = Some(dv);
        if let Err(e) = db::set_position(&self.db, id, dv) {
            eprintln!("bossbar-overlay: save position failed: {e}");
        }
    }

    /// Move a bar to follow a two-finger trackpad scroll, persisting the new
    /// position like a drag. There is no button for `Window::drag_window` to own,
    /// so we move the window ourselves: update `self_set` before the move so the
    /// resulting `Moved` reads as our own echo (no double write), refresh
    /// `last_move` so the reconcile guard does not snap it back from the watcher's
    /// lagged read, and write the position straight to the DB. Scrolling a bar
    /// pins it, exactly as dragging one does.
    // clone:ignore intentionally parallel to the book overlay, with bar-specific state updates.
    fn scroll_move(&mut self, id: i64, delta: MouseScrollDelta, phase: TouchPhase) {
        let Some(win) = self.wins.get_mut(&id) else {
            return;
        };
        // Move the window live on each event. `ocwin::suppress_scroll_momentum`
        // drops the macOS momentum coast upstream, so this only sees the physical
        // finger drag (and the bar stops on lift). `self_set` tracks where the
        // window sits and is set after create; measure the scroll from there, and
        // pin the bar (`bar.pos`) so the size-settle pass stops re-centering it,
        // exactly as a drag does.
        let result = overlay_core::handle_scroll_drag(
            &win.window,
            win.gesture.cursor(),
            &mut win.self_set,
            &mut win.last_move,
            delta,
            phase,
            |pos| win.bar.pos = Some(DVec2::new(pos.x, pos.y)),
            |pos| db::set_position(&self.db, id, DVec2::new(pos.x, pos.y)),
        );
        // Persist only when the gesture settles, not per frame: a scroll emits a
        // burst of `MouseWheel` events, and writing on each would open a SQLite
        // connection per frame on the UI thread. The finger lift carries
        // `TouchPhase::Ended`; a discrete wheel notch (`LineDelta`) has no Ended
        // phase but is low-frequency, so save it directly.
        if let Err(e) = result {
            eprintln!("bossbar-overlay: save position failed: {e}");
        }
    }

    /// Settle a bar's window to the size its current state asks for, resizing only
    /// when that size changes. The target is the expanded (panel-open) size while
    /// the bar is hovered or easing back, otherwise the collapsed bar size; either
    /// way it is widened to hold the title (which a live elapsed counter can lengthen
    /// mid-session). Doing it here, off the per-frame easing, keeps the window from
    /// resizing every frame: it only resizes when the bar opens, closes, or the
    /// title's width actually changes.
    ///
    /// An auto-stacked bar (no pinned position) is re-centered as its width changes
    /// so the column stays centered; a pinned bar keeps its dragged top-left and
    /// grows rightward.
    fn settle_window_size(&mut self, id: i64) {
        let Some(core) = self.core.as_ref() else {
            return;
        };
        let now = now_unix();
        let Some(win) = self.wins.get_mut(&id) else {
            return;
        };
        // A poofing window is on its way out; leave its size fixed so it shrinks
        // cleanly via the render transform instead of also resizing the window.
        if win.poof.is_some() {
            return;
        }
        let expand = win.wants_expanded();
        let (w_px, h_px) = if expand {
            scene::expanded_window_px(&core.gpu, &win.bar, self.scale, now)
        } else {
            let title_w = scene::title_extent_px(&win.bar, self.scale, now);
            scene::bar_window_px(self.scale, title_w, !win.bar.icon.is_empty())
        };
        let (prev_w, _) = win.last_size;
        if win.last_size == (w_px, h_px) {
            return;
        }
        win.last_size = (w_px, h_px);
        // Apply the resize. winit returns `Some(new)` when it resized the window
        // synchronously, in which case no `Resized` event follows (notably on a 1x
        // display in winit 0.30), so reconfigure the surface here; otherwise the
        // `Resized` handler does it. Without this an unmatched synchronous resize
        // leaves `config` stale and the title clips against the old surface extent.
        if let Some(new) = win.window.request_inner_size(PhysicalSize::new(w_px, h_px)) {
            win.config.width = new.width.max(1);
            win.config.height = new.height.max(1);
            win.surface.configure(core.gpu.device(), &win.config);
        }

        // Keep an auto-stacked bar centered as it widens: shift its window left by
        // half the width change. winit grows the window from its top-left, so
        // without this the centered bar would drift right each time the counter
        // gains a digit. Record the new position in `self_set` first so the
        // resulting `Moved` reads as our own echo (no DB write, no snap-back). Skip
        // while a drag is in flight so the re-center never moves the window out from
        // under the OS-owned drag.
        if win.bar.pos.is_none()
            && !win.gesture.dragging()
            && w_px != prev_w
            && let Some(ss) = win.self_set
        {
            let dx = (w_px as f64 - prev_w as f64) / 2.0 / self.scale_factor;
            let np = LogicalPosition::new((ss.x - dx).max(0.0), ss.y);
            win.self_set = Some(np);
            win.window.set_outer_position(np);
        }
    }
}

impl ApplicationHandler<Vec<BossBar>> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.ready {
            return;
        }
        // A two-finger scroll-drag tracks the fingers and stops on lift; drop the
        // macOS momentum coast so a flick does not fling the bar past the gesture.
        ocwin::suppress_scroll_momentum();
        let monitor = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next());
        let (mon_w, mon_h, scale_factor) = match &monitor {
            Some(m) => (m.size().width, m.size().height, m.scale_factor()),
            None => (1920, 1080, 1.0),
        };
        self.scale_factor = scale_factor;
        // No integer rounding: a fractional `base_scale` (e.g. 1.25) must reach
        // the renderer intact so the bars grow by exactly the requested fraction
        // rather than snapping to the nearest whole sprite scale.
        self.scale = ((self.base_scale as f64) * scale_factor).max(1.0) as f32;
        self.mon_logical = (mon_w as f64 / scale_factor, mon_h as f64 / scale_factor);
        self.ready = true;

        let bars = db::read_once(&self.db).unwrap_or_default();
        self.reconcile(event_loop, bars);

        // Wake the loop with fresh bars on every DB change; a send error means the
        // loop is gone, which stops the watcher thread.
        let proxy = self.proxy.clone();
        db::spawn_watcher(self.db.clone(), move |bars| proxy.send_event(bars).is_ok());
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, bars: Vec<BossBar>) {
        self.reconcile(event_loop, bars);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, wid: WindowId, event: WindowEvent) {
        let Some(id) = self.bar_id_for(wid) else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                self.wins.remove(&id);
                if self.wins.is_empty() {
                    event_loop.exit();
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(win) = self.wins.get_mut(&id) {
                    win.config.width = size.width.max(1);
                    win.config.height = size.height.max(1);
                    if let Some(core) = self.core.as_ref() {
                        win.surface.configure(core.gpu.device(), &win.config);
                    }
                }
                self.render_id(id);
            }
            WindowEvent::RedrawRequested => self.render_id(id),
            WindowEvent::CursorEntered { .. } => {
                // winit's own tracking rect and the always-active NSTrackingArea
                // (enable_background_hover) both deliver mouseEntered:, so this can
                // arrive twice per crossing; act only on the first.
                let entered = self.wins.get_mut(&id).is_some_and(|win| {
                    let first = !win.hovered;
                    if first {
                        win.hovered = true;
                        win.window.set_cursor(CursorIcon::Grab);
                        // Bring this bar above its siblings so its pop-down panel
                        // covers them instead of unfolding underneath.
                        ocwin::raise_to_front(&win.window);
                    }
                    first
                });
                if entered {
                    self.render_id(id);
                }
            }
            WindowEvent::CursorLeft { .. } => {
                if let Some(win) = self.wins.get_mut(&id) {
                    win.hovered = false;
                    win.window.set_cursor(CursorIcon::Default);
                }
                self.render_id(id);
            }
            WindowEvent::CursorMoved { position, .. } => {
                let start_drag = self
                    .wins
                    .get_mut(&id)
                    .is_some_and(|win| win.gesture.cursor_moved(position));
                if start_drag {
                    if let Some(win) = self.wins.get(&id) {
                        win.window.set_cursor(CursorIcon::Grabbing);
                        if let Err(e) = win.window.drag_window() {
                            eprintln!("bossbar-overlay: drag failed: {e}");
                        }
                    }
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if let Some(win) = self.wins.get_mut(&id) {
                    win.gesture.pressed();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                // A release where the button-down gesture never became a drag is a
                // click: open the bar's URL if it has one. (A drag swallows its own
                // mouse-up on macOS, but the gesture already knows it was a drag.)
                let url = self.wins.get_mut(&id).and_then(|win| {
                    let clicked = win.gesture.released();
                    win.window.set_cursor(if win.hovered {
                        CursorIcon::Grab
                    } else {
                        CursorIcon::Default
                    });
                    (clicked && !win.bar.url.trim().is_empty()).then(|| win.bar.url.clone())
                });
                if let Some(url) = url {
                    overlay_core::open_url("bossbar-overlay", &url);
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } => {
                // Right-click opens a native menu to dismiss the HUD. Each bar is
                // its own window, but the bars are one downtime display, so the
                // action quits all of them.
                if overlay_core::menu::popup(&["Quit Boss Bars"]) == Some(0) {
                    event_loop.exit();
                }
            }
            WindowEvent::Moved(pos) => self.on_moved(id, pos),
            WindowEvent::MouseWheel { delta, phase, .. } => self.scroll_move(id, delta, phase),
            _ => {}
        }
    }

    /// Keep redrawing while any bar is animating (growing, shrinking, or
    /// breathing), capped to ~60 fps; tick once a second while any bar shows a live
    /// elapsed counter so its clock advances; otherwise let the loop sleep until
    /// the next event so an idle overlay costs nothing.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Settle each window's size first: grow for an open panel, shrink back once
        // the hover has fully eased out, and widen as the title (or its live
        // counter) grows. Done here, off the per-frame easing, so the window
        // resizes only when its target size actually changes.
        let ids: Vec<i64> = self.wins.keys().copied().collect();
        for id in ids {
            self.settle_window_size(id);
        }

        // Remove any window whose poof despawn has finished. Dropping the BarWin
        // closes its window. Parity with the old instant-removal: the overlay
        // stays alive with zero bars (the watcher re-adds them), so no exit here.
        let now_i = Instant::now();
        self.wins
            .retain(|_, w| w.poof.is_none_or(|t0| now_i.duration_since(t0) < POOF_DUR));

        let mut animating = false;
        let mut ticking = false;
        for win in self.wins.values() {
            // A live counter only needs the once-a-second TICK, but anything
            // mid-animation also wants its redraw now (the faster cadence wins).
            if win.animating() {
                animating = true;
                win.window.request_redraw();
            } else if win.bar.since.is_some() {
                ticking = true;
                win.window.request_redraw();
            }
        }
        event_loop.set_control_flow(if animating {
            ControlFlow::WaitUntil(Instant::now() + FRAME)
        } else if ticking {
            ControlFlow::WaitUntil(Instant::now() + TICK)
        } else {
            ControlFlow::Wait
        });
    }
}

/// Run the overlay event loop. Blocks until the last window closes.
pub fn run(db: PathBuf, base_scale: f32) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && std::env::var_os("BOSSBAR_WINIT").is_none()
    {
        match crate::layer_shell::run(db.clone(), base_scale) {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!("bossbar-overlay: layer-shell unavailable, falling back to winit: {e}");
            }
        }
    }

    let event_loop: EventLoop<Vec<BossBar>> = ocwin::build_event_loop()?;
    let proxy = event_loop.create_proxy();
    let mut app = App {
        db,
        base_scale: base_scale.max(1.0),
        proxy,
        instance: wgpu::Instance::default(),
        core: None,
        wins: HashMap::new(),
        mon_logical: (1920.0, 1080.0),
        scale_factor: 1.0,
        scale: base_scale.max(1.0),
        start: Instant::now(),
        ready: false,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
