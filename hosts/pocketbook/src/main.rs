//! pocketbook-host — the PocketJS UI runtime on PocketBook e-readers.
//!
//! Reuses the backend-agnostic `ui` surface (`pocket_ui_surface::UiSurface`)
//! and the core's software rasterizer, then blits the frame as RGB24 (inkview
//! converts to gray on grayscale panels, writes RGB on color panels). See
//! `docs/IMPLEMENTATION.md` in this directory for the full design and the
//! ground-truth API notes.
//!
//! Event-loop model (mirrors `inkview-slint`): `iv_main` runs on the main
//! thread forwarding every `Event` into an mpsc channel; a second thread owns
//! the `Screen` and the PocketJS tick/render loop, pulling events with a
//! timeout so it ticks on a fixed cadence even when idle.

mod framebuffer;
mod input;
mod refresh;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use inkview::screen::ScreenOrientation;
use inkview::Event;
use pocket_mod::Guest;
use pocket_ui_surface::UiSurface;

use framebuffer::DirtyRect;

/// Host platform-contract identity, baked from the resolved build plan by
/// build.rs (POCKETJS_TARGET). Must equal the bundle's target id or plan-built
/// bundles refuse this host (framework/src/host.ts::assertNativeHostContract) —
/// so one source tree is rebuilt per pocketbook-* target, not one binary for
/// the whole family.
const HOST_ID: &str = env!("POCKETJS_TARGET");
const HOST_ABI_STR: &str = env!("POCKETJS_HOST_ABI");

/// Logical tick cadence. E-ink doesn't need 60 fps; ~30 fps keeps animations
/// smooth while sparing CPU and battery.
const TICK_MS: u64 = 33;

/// How many consecutive ticks the physical G-sensor must hold a new orientation
/// before we rotate (~200 ms at the 33 ms cadence). A physical turn passes
/// through transient intermediate readings (confirmed on the Era Color:
/// portrait→landscape→portrait flips through GSensor 2/1 en route); debouncing
/// avoids flapping the guest session through them.
const ORIENT_DEBOUNCE_TICKS: u32 = 6;

/// The authored logical viewport + raster density the target profile bakes
/// bundles for (contracts/spec/platforms.ts), baked by build.rs. Must match the
/// bundle: the framework lays the app out for this size, so the host presents
/// exactly it. Both pocketbook logicals stay ≤511 px/axis, keeping touch
/// coordinates inside the 9-bit wire format (framework/src/touch.ts).
const LOGICAL_W_STR: &str = env!("POCKETJS_LOGICAL_WIDTH");
const LOGICAL_H_STR: &str = env!("POCKETJS_LOGICAL_HEIGHT");
const DENSITY_STR: &str = env!("POCKETJS_RASTER_DENSITY");
/// Presentation the plan resolved: "fit" (default tier — scale to the panel up
/// or down) or "integer-fit" (pocketbook-compat — blit-or-shrink, verbatim).
const PRESENTATION: &str = env!("POCKETJS_PRESENTATION");

/// Baked host identity + viewport, parsed once at startup.
struct HostConfig {
    host_abi: u32,
    /// The logical viewport the app declared (its authored orientation).
    logical_w: u32,
    logical_h: u32,
    density: u32,
}

impl HostConfig {
    fn from_env() -> Self {
        Self {
            host_abi: parse_baked(HOST_ABI_STR, "POCKETJS_HOST_ABI"),
            logical_w: parse_baked(LOGICAL_W_STR, "POCKETJS_LOGICAL_WIDTH"),
            logical_h: parse_baked(LOGICAL_H_STR, "POCKETJS_LOGICAL_HEIGHT"),
            density: parse_baked(DENSITY_STR, "POCKETJS_RASTER_DENSITY"),
        }
    }
}

fn parse_baked(value: &str, name: &str) -> u32 {
    value
        .parse::<u32>()
        .unwrap_or_else(|_| panic!("baked {name} value {value:?} is not a u32"))
}

fn is_landscape(orientation: ScreenOrientation) -> bool {
    matches!(
        orientation,
        ScreenOrientation::Landscape90Deg | ScreenOrientation::Landscape270Deg
    )
}

/// inkview orientation raw int → `ScreenOrientation`. Mirrors inkview's private
/// `ScreenOrientation::from_iv`: 0=Portrait0, 1=Landscape270, 2=Landscape90,
/// 3=Portrait180. The physical G-sensor reading (`GetGSensorOrientation`), the
/// app orientation (`GetOrientation`), and the `SetOrientation` argument all use
/// this same encoding, so a G-sensor value can be fed straight to SetOrientation.
fn orient_from_raw(raw: i32) -> ScreenOrientation {
    match raw {
        0 => ScreenOrientation::Portrait0Deg,
        1 => ScreenOrientation::Landscape270Deg,
        2 => ScreenOrientation::Landscape90Deg,
        3 => ScreenOrientation::Portrait180Deg,
        _ => ScreenOrientation::Portrait0Deg,
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // &'static Inkview so both the render thread (Screen::new) and the main
    // thread (iv_main) can reference it — same pattern as the slint demo.
    let iv: &'static inkview::bindings::Inkview = Box::leak(Box::new(inkview::load()));

    let (tx, rx) = mpsc::channel::<Event>();

    let render = std::thread::spawn(move || run(iv, rx));

    // Main thread: the inkview event loop. Forward every event to the render
    // thread; if it has gone away (send fails), ask inkview to close the app.
    inkview::iv_main(iv, move |ev| {
        if tx.send(ev).is_err() {
            // SAFETY: the render thread owns the only other use of `iv` and has
            // dropped its receiver (send failed), so it is quiescent; CloseApp
            // only asks inkview to tear down its main loop. Mirrors the
            // inkview-slint demo's shutdown path.
            unsafe {
                iv.CloseApp();
            }
        }
        Some(())
    });

    render
        .join()
        .map_err(|_| anyhow::anyhow!("render thread panicked"))?
}

/// The render thread: boot the guest, then tick/render until Exit.
fn run(iv: &'static inkview::bindings::Inkview, rx: mpsc::Receiver<Event>) -> Result<()> {
    // inkview delivers Init first; wait for it before touching the framebuffer.
    if rx.recv().context("event channel closed before Init")? != Event::Init {
        anyhow::bail!("expected EVT_INIT first");
    }

    let mut screen = inkview::screen::Screen::new(iv);
    let cfg = HostConfig::from_env();

    // Read the bundle once; it is re-evaluated on every orientation restart.
    let pak = std::fs::read(pak_path()).with_context(|| format!("reading {}", pak_path()))?;
    let bundle =
        std::fs::read_to_string(js_path()).with_context(|| format!("reading {}", js_path()))?;

    // One session per device orientation. Rotation is driven by the physical
    // G-sensor (polled each visible tick, debounced), not by events — see the
    // poll below. `orientation` lives outside the session loop so a rotation
    // can update it and fall through into a fresh boot.
    let mut orientation = screen.orientation();
    'session: loop {
        let mut session = boot(&cfg, &screen, orientation, &pak, &bundle)?;

        // First paint: one frame + full-update so the screen starts clean.
        tick(
            &session.guest,
            &session.surface,
            &mut session.fb,
            &mut session.refresh,
            &mut session.input,
            &mut screen,
            &session.geo,
            true,
        )?;

        let mut last_tick = Instant::now();
        // Visibility: the app starts in the foreground. Hide/Background set
        // this; Show/Foreground/Repaint clear it (input.rs::Outcome).
        let mut hidden = false;
        // Orientation debounce: how many consecutive ticks the current
        // G-sensor reading has held, so we only rotate once it is stable.
        let mut pending_orient: Option<i32> = None;
        let mut pending_count: u32 = 0;
        loop {
            // Pull events until the tick deadline, then drain any burst.
            let deadline = last_tick + Duration::from_millis(TICK_MS);
            let mut quit = false;
            let mut full = false;
            let was_hidden = hidden;
            loop {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                match rx.recv_timeout(deadline - now) {
                    Ok(ev) => {
                        if apply_outcome(session.input.on_event(ev), &mut hidden, &mut full) {
                            quit = true;
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        quit = true;
                        break;
                    }
                }
            }
            while let Ok(ev) = rx.try_recv() {
                if apply_outcome(session.input.on_event(ev), &mut hidden, &mut full) {
                    quit = true;
                }
            }
            if quit {
                return Ok(());
            }

            // Resume from background: reboot the guest session, exactly like
            // the orientation path does. On the Era Color a task-list resume
            // leaves the panel dropping the app's incremental updates, and the
            // state that causes it lives in the running guest/surface — not the
            // render layer (re-acquiring the framebuffer, SetOrientation,
            // full/partial updates, a settle delay, and even a fresh
            // framebuffer pipeline + refresh policy with the guest kept alive
            // all fail). A fresh session — re-evaluating the bundle into a new
            // guest and doing a clean first-paint full_update — renders
            // normally. This resets in-app state (the same trade-off rotation
            // already makes); a state-preserving fix would require tracking
            // down the framework state that goes stale across a background gap.
            if was_hidden && !hidden {
                log::info!("pocketbook: resumed from background → rebooting session");
                screen = inkview::screen::Screen::new(iv);
                continue 'session;
            }

            // While hidden the launcher owns the panel: don't advance the
            // guest or drive the e-ink (it would fight the launcher's own
            // updates and burn battery). Keep waking on the tick cadence so a
            // resume event — or a rotation — is still noticed promptly.
            if hidden {
                last_tick = Instant::now();
                continue;
            }

            // Rotation: drive from the PHYSICAL G-sensor, not GetOrientation.
            // The firmware only auto-applies portrait↔portrait-180 flips to
            // GetOrientation; landscape (GSensor 1/2) is reported by the sensor
            // but never applied unless the app calls SetOrientation itself
            // (confirmed on the Era Color: GSensor reaches 1/2 while
            // GetOrientation stays 0/3). Debounce the sensor so a turn doesn't
            // flap the session through transient intermediate readings.
            let gsensor = unsafe { iv.GetGSensorOrientation() };
            if Some(gsensor) == pending_orient {
                pending_count += 1;
            } else {
                pending_orient = Some(gsensor);
                pending_count = 1;
            }
            if pending_count >= ORIENT_DEBOUNCE_TICKS && orient_from_raw(gsensor) != orientation {
                log::info!(
                    "pocketbook: rotating {orientation} → {} (gsensor {gsensor} stable)",
                    orient_from_raw(gsensor)
                );
                // Apply the rotation to the panel, then re-acquire the
                // framebuffer (its layout may swap for landscape) and boot a
                // fresh session at the orientation-matching logical viewport.
                unsafe { iv.SetOrientation(gsensor) };
                screen = inkview::screen::Screen::new(iv);
                orientation = orient_from_raw(gsensor);
                continue 'session;
            }

            last_tick = Instant::now();
            tick(
                &session.guest,
                &session.surface,
                &mut session.fb,
                &mut session.refresh,
                &mut session.input,
                &mut screen,
                &session.geo,
                full,
            )?;
        }
    }
}

/// Apply one event's outcome to the loop's visibility/repaint flags. Returns
/// `true` when the app should quit. Kept as a free function (not a closure)
/// so the mutable borrows of `hidden`/`full` don't overlap the loop's direct
/// `quit = true` on channel disconnect.
fn apply_outcome(outcome: input::Outcome, hidden: &mut bool, full: &mut bool) -> bool {
    match outcome {
        input::Outcome::Quit => true,
        input::Outcome::Show => {
            *hidden = false;
            *full = true;
            false
        }
        input::Outcome::Hide => {
            *hidden = true;
            false
        }
        input::Outcome::Continue => false,
    }
}

/// A running guest bound to one device orientation: the geometry chosen for the
/// panel + orientation, and the surface/guest/render state laid out for it.
struct Session {
    geo: Geometry,
    surface: UiSurface,
    guest: Guest,
    fb: framebuffer::FramebufferPipeline,
    refresh: refresh::Refresh,
    input: input::Input,
}

/// Boot the guest for one orientation: pick the orientation-matching logical
/// viewport, build the geometry, and run the bundle exactly like uihost
/// (feed pak, mount ui, eval bundle).
fn boot(
    cfg: &HostConfig,
    screen: &inkview::screen::Screen,
    orientation: ScreenOrientation,
    pak: &[u8],
    bundle: &str,
) -> Result<Session> {
    let geo = session_geometry(cfg, screen, orientation);
    log::info!(
        "pocketbook: orientation {orientation}, logical {}x{} @{}x ({}), render {}x{} → disp {}x{} +({},{})",
        geo.logical_w,
        geo.logical_h,
        geo.density,
        PRESENTATION,
        geo.render_w,
        geo.render_h,
        geo.disp_w,
        geo.disp_h,
        geo.ox,
        geo.oy
    );

    let surface =
        UiSurface::new_with_density((geo.logical_w as f32, geo.logical_h as f32), geo.density);
    surface.set_identity(HOST_ID, cfg.host_abi);
    surface.feed_pak(pak);

    let guest = Guest::new()?;
    surface.mount(&guest)?;
    guest.eval("app", bundle)?;
    anyhow::ensure!(
        guest.has_frame(),
        "bundle installed no frame() — is this a PocketJS app?"
    );

    let fb = framebuffer::FramebufferPipeline::new(geo.render_w, geo.render_h, geo.density);
    let refresh = refresh::Refresh::new();
    let input = input::Input::new(
        geo.ox as i32,
        geo.oy as i32,
        geo.logical_w as u32,
        geo.logical_h as u32,
        geo.disp_w as u32,
        geo.disp_h as u32,
    );
    Ok(Session {
        geo,
        surface,
        guest,
        fb,
        refresh,
        input,
    })
}

/// Pick the logical viewport matching the device orientation and compute the
/// presentation geometry for the effective panel. The panel dims are derived
/// from the framebuffer's short/long edges (via min/max, so it doesn't matter
/// whether `Screen::width()/height()` already reflect the current orientation
/// or stay orientation-independent native dims): portrait presents short×long,
/// landscape long×short. The app authors one logical (its declared
/// orientation); when the device is in the other orientation we present the
/// swapped logical so the app still fills the panel (every pocketbook target
/// offers both [w,h] and [h,w]).
fn session_geometry(
    cfg: &HostConfig,
    screen: &inkview::screen::Screen,
    orientation: ScreenOrientation,
) -> Geometry {
    let landscape = is_landscape(orientation);
    let (a, b) = (screen.width(), screen.height());
    let (short, long) = (a.min(b), a.max(b));
    let (panel_w, panel_h) = if landscape {
        (long, short)
    } else {
        (short, long)
    };
    let authored_landscape = cfg.logical_w > cfg.logical_h;
    let (logical_w, logical_h) = if landscape != authored_landscape {
        (cfg.logical_h, cfg.logical_w)
    } else {
        (cfg.logical_w, cfg.logical_h)
    };
    Geometry::for_panel(
        panel_w,
        panel_h,
        logical_w as usize,
        logical_h as usize,
        cfg.density,
        PRESENTATION,
    )
}

/// One fixed-step frame: guest turn → core tick → draw → raster → gray → blit
/// → panel update. Order matches uihost.
#[allow(clippy::too_many_arguments)]
fn tick(
    guest: &Guest,
    surface: &UiSurface,
    fb: &mut framebuffer::FramebufferPipeline,
    refresh: &mut refresh::Refresh,
    input: &mut input::Input,
    screen: &mut inkview::screen::Screen,
    geo: &Geometry,
    full: bool,
) -> Result<()> {
    let (buttons, analog, touches) = input.snapshot();
    guest.frame_with_touches(buttons, analog, &touches)?;

    surface.tick();

    // Incremental raster: repaint only DrawList damage into the retained
    // RGBA8 target, then pixel-diff WITHIN the damage plan to find the tiles
    // the panel must refresh. An idle frame plans nothing, rasterizes
    // nothing, and scans nothing.
    let dirty = surface.with_ui(|ui| {
        let words = ui.draw().words.clone();
        let plan = fb.rasterize(ui, &words);
        fb.diff(&plan)
    });

    if full {
        // Full panel redraw: the retained buffer is always the complete current
        // frame, so re-blit it and flash the panel (first paint / orientation
        // change / resume-from-background reboot).
        fb.blit_all(screen, geo);
        refresh.full(screen);
        fb.advance_full();
    } else if !dirty.is_empty() {
        fb.blit_dirty(screen, &dirty, geo);
        let screen_dirty = offset_rects(&dirty, geo);
        refresh.present(screen, &screen_dirty);
        // Latch only the blitted tiles; everything else already matches.
        fb.advance(&dirty);
    } else {
        // No pixel change this frame; still let the refresh policy run its
        // quiet cleanup timer (it no-ops when there's nothing pending).
        refresh.present(screen, &[]);
    }
    Ok(())
}

/// Render-buffer rects → screen rects via the geometry mapping.
fn offset_rects(rects: &[DirtyRect], geo: &Geometry) -> Vec<DirtyRect> {
    rects
        .iter()
        .map(|r| {
            let (sx, sy, sw, sh) = geo.render_rect_to_screen(r.x, r.y, r.w, r.h);
            DirtyRect {
                x: sx,
                y: sy,
                w: sw,
                h: sh,
            }
        })
        .collect()
}

/// Logical-viewport / raster-density geometry for the current panel.
struct Geometry {
    logical_w: usize,
    logical_h: usize,
    density: u32,
    render_w: usize,
    render_h: usize,
    /// Displayed width on the panel after presentation scaling.
    disp_w: usize,
    /// Displayed height on the panel after presentation scaling.
    disp_h: usize,
    /// Horizontal centering offset on the panel.
    ox: usize,
    /// Vertical centering offset on the panel.
    oy: usize,
}

impl Geometry {
    /// Present the bundle's fixed logical surface (logical × density render)
    /// on the actual panel, whose effective dimensions already reflect the
    /// device orientation. Two presentations:
    ///
    /// - `fit` (default `pocketbook` tier): nearest-neighbor scale to fill as
    ///   much of the panel as possible while preserving aspect — UP or DOWN.
    ///   This recovers ~95–99% of every ~3:4 panel; the cost is a soft
    ///   non-integer scale on panels whose size isn't close to the render.
    /// - anything else (`integer-fit`, the `pocketbook-compat` tier): the
    ///   legacy behavior verbatim — when the render fits it is blit 1:1 and
    ///   centered; when it doesn't (e.g. a 960-wide render on a 758-wide
    ///   portrait panel like the Verse) it is scaled DOWN to fit and centered.
    ///
    /// The logical viewport and density come from the plan and stay
    /// ≤511/axis, so touch coordinates fit the 9-bit wire format.
    fn for_panel(
        panel_w: usize,
        panel_h: usize,
        logical_w: usize,
        logical_h: usize,
        density: u32,
        presentation: &str,
    ) -> Self {
        let render_w = logical_w * density as usize;
        let render_h = logical_h * density as usize;

        // Scale to fit, picking the binding axis without floats:
        //   panel_w * render_h < panel_h * render_w  →  width binds
        let fit = |panel_w: usize, panel_h: usize| -> (usize, usize) {
            if panel_w * render_h < panel_h * render_w {
                (panel_w, (render_h * panel_w / render_w).max(1))
            } else {
                ((render_w * panel_h / render_h).max(1), panel_h)
            }
        };

        let (disp_w, disp_h) = if presentation == "fit" {
            fit(panel_w, panel_h)
        } else if render_w <= panel_w && render_h <= panel_h {
            (render_w, render_h)
        } else {
            fit(panel_w, panel_h)
        };

        let ox = panel_w.saturating_sub(disp_w) / 2;
        let oy = panel_h.saturating_sub(disp_h) / 2;
        Self {
            logical_w,
            logical_h,
            density,
            render_w,
            render_h,
            disp_w,
            disp_h,
            ox,
            oy,
        }
    }

    /// Map a panel x coordinate back to a render-buffer x coordinate.
    #[inline]
    fn screen_to_render_x(&self, sx: usize) -> usize {
        (sx.saturating_sub(self.ox)) * self.render_w / self.disp_w
    }

    /// Map a panel y coordinate back to a render-buffer y coordinate.
    #[inline]
    fn screen_to_render_y(&self, sy: usize) -> usize {
        (sy.saturating_sub(self.oy)) * self.render_h / self.disp_h
    }

    /// Conservatively map a render-buffer rect to the screen rect that covers
    /// every panel pixel sampling from within it.
    fn render_rect_to_screen(
        &self,
        rx: usize,
        ry: usize,
        rw: usize,
        rh: usize,
    ) -> (usize, usize, usize, usize) {
        let sx_min = (rx * self.disp_w).div_ceil(self.render_w) + self.ox;
        let sy_min = (ry * self.disp_h).div_ceil(self.render_h) + self.oy;
        let sx_max = ((rx + rw) * self.disp_w - 1) / self.render_w + self.ox;
        let sy_max = ((ry + rh) * self.disp_h - 1) / self.render_h + self.oy;
        (sx_min, sy_min, sx_max - sx_min + 1, sy_max - sy_min + 1)
    }
}

fn pak_path() -> String {
    std::env::var("POCKET_PAK").unwrap_or_else(|_| "app.pak".into())
}

fn js_path() -> String {
    std::env::var("POCKET_JS").unwrap_or_else(|_| "app.js".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_fills_the_plurality_panel_exactly() {
        // Default tier: 375×500 @4x → a 1500×2000 render. On the plurality
        // 1404×1872 InkPad panel the aspect matches exactly (both 3:4), so fit
        // scales to fill the whole panel with no letterbox.
        let geo = Geometry::for_panel(1404, 1872, 375, 500, 4, "fit");
        assert_eq!((geo.render_w, geo.render_h), (1500, 2000));
        assert_eq!((geo.disp_w, geo.disp_h), (1404, 1872));
        assert_eq!((geo.ox, geo.oy), (0, 0));
    }

    #[test]
    fn fit_scales_up_a_small_panel() {
        // The discontinued Basic 3 (600×800, 3:4) is smaller than the render;
        // fit scales UP to fill it (the legacy host only blit-or-shrank).
        let geo = Geometry::for_panel(600, 800, 375, 500, 4, "fit");
        assert_eq!((geo.disp_w, geo.disp_h), (600, 800));
        assert_eq!((geo.ox, geo.oy), (0, 0));
    }

    #[test]
    fn fit_letterboxes_a_non_matching_aspect() {
        // The Verse (758×1024, ~0.740) is not exactly 3:4, so fit fills the
        // width and leaves a thin vertical letterbox, centered.
        let geo = Geometry::for_panel(758, 1024, 375, 500, 4, "fit");
        assert_eq!(geo.disp_w, 758);
        assert!(geo.disp_h <= 1024);
        assert_eq!(geo.ox, 0);
        assert_eq!(geo.oy, (1024 - geo.disp_h) / 2);
    }

    #[test]
    fn compat_blits_when_the_render_fits() {
        // pocketbook-compat: 480×272 @2x → 960×544 render, integer-fit. On a
        // panel that fits it, the render is blit 1:1 and centered (verbatim
        // legacy behavior — no scale-up).
        let geo = Geometry::for_panel(1404, 1872, 480, 272, 2, "integer-fit");
        assert_eq!((geo.disp_w, geo.disp_h), (960, 544));
        assert_eq!((geo.ox, geo.oy), ((1404 - 960) / 2, (1872 - 544) / 2));
    }

    #[test]
    fn compat_scales_down_an_oversized_render() {
        // The legacy path on a portrait Verse panel: the 960-wide render is
        // wider than 758, so it scales DOWN to fit (matches the mapping the
        // input tests assume: disp 758×429, oy 297).
        let geo = Geometry::for_panel(758, 1024, 480, 272, 2, "integer-fit");
        assert_eq!((geo.disp_w, geo.disp_h), (758, 429));
        assert_eq!(geo.oy, 297);
    }

    #[test]
    fn landscape_is_detected_from_inkview_orientations() {
        assert!(!is_landscape(ScreenOrientation::Portrait0Deg));
        assert!(!is_landscape(ScreenOrientation::Portrait180Deg));
        assert!(is_landscape(ScreenOrientation::Landscape90Deg));
        assert!(is_landscape(ScreenOrientation::Landscape270Deg));
    }
}
