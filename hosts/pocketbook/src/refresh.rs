//! E-ink panel update policy.
//!
//! Ported from the battle-tested strategy in `inkview-slint/src/lib.rs`: gate
//! on `Screen::is_updating()`; issue a high-quality `partial_update` on the
//! damage box while the panel is idle; while an update is in flight, throttle
//! fast `dynamic_update`s (≥20 ms apart) on the accumulated damage; and after
//! ~200 ms of quiet, do a final `partial_update` over the dynamic-updated
//! region to clear ghosting.

use std::time::{Duration, Instant};

use inkview::screen::Screen;

use crate::framebuffer::DirtyRect;

const DYNAMIC_MIN_INTERVAL: Duration = Duration::from_millis(20);
const CLEANUP_QUIET: Duration = Duration::from_millis(200);

#[derive(Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

pub struct Refresh {
    last_draw: Instant,
    /// Damage accumulated while a panel update was in flight; needs a cleanup
    /// partial update once things go quiet.
    pending_cleanup: Option<Rect>,
    cleanup_after: Option<Instant>,
    /// Force the next incremental update to be a high-quality partial (not a
    /// dynamic). Set on resume: after a background/foreground cycle the panel
    /// ignores dynamic updates until a partial re-primes it (confirmed on the
    /// Era Color — the spinner's dynamic updates stayed invisible until input).
    force_partial: bool,
}

impl Refresh {
    pub fn new() -> Self {
        Self {
            last_draw: Instant::now(),
            pending_cleanup: None,
            cleanup_after: None,
            force_partial: false,
        }
    }

    /// Drive the panel for this frame. `dirty` is in SCREEN coordinates
    /// (render-buffer rects offset by the integer-fit origin).
    pub fn present(&mut self, screen: &mut Screen, dirty: &[DirtyRect]) {
        // Quiet-period cleanup: a final high-quality partial update on the
        // region we hammered with dynamic updates (clears ghosting).
        if let Some(at) = self.cleanup_after {
            if Instant::now() >= at {
                if let Some(r) = self.pending_cleanup.take() {
                    screen.partial_update(r.x, r.y, r.w, r.h);
                    self.last_draw = Instant::now();
                }
                self.cleanup_after = None;
            }
        }

        if dirty.is_empty() {
            return;
        }
        let d = merge(dirty);

        // Resume re-prime: the first incremental update after a
        // background/foreground cycle must be a high-quality partial — a
        // dynamic update here is silently dropped by the panel.
        if self.force_partial {
            self.force_partial = false;
            screen.partial_update(d.x, d.y, d.w, d.h);
            self.last_draw = Instant::now();
            return;
        }

        if screen.is_updating() {
            // A panel update is still in flight. Queue a fast dynamic update,
            // throttled to ≥20 ms, on the accumulated damage; schedule a
            // cleanup partial update 200 ms after the last draw.
            self.pending_cleanup = Some(union(self.pending_cleanup, d));
            if self.last_draw.elapsed() > DYNAMIC_MIN_INTERVAL {
                let r = self.pending_cleanup.unwrap();
                screen.dynamic_update(r.x, r.y, r.w, r.h);
                self.last_draw = Instant::now();
            }
            self.cleanup_after = Some(Instant::now() + CLEANUP_QUIET);
        } else {
            // Idle panel: high-quality non-flashing partial on the damage box.
            screen.partial_update(d.x, d.y, d.w, d.h);
            self.last_draw = Instant::now();
        }
    }

    /// Full flashing redraw — call on first paint / orientation change.
    pub fn full(&mut self, screen: &mut Screen) {
        screen.full_update();
        self.last_draw = Instant::now();
        self.pending_cleanup = None;
        self.cleanup_after = None;
    }

    /// Resume-from-background repaint: a high-quality partial over the whole
    /// displayed region (NOT a flashing full_update), mirroring inkview-slint,
    /// which never full-updates on foreground. The next incremental update is
    /// forced partial too. `x,y,w,h` are the displayed region in screen
    /// coordinates. (On the Era Color this does not yet fully re-prime the
    /// panel — incremental updates still stall until input; see main.rs.)
    pub fn resume(&mut self, screen: &mut Screen, x: i32, y: i32, w: u32, h: u32) {
        screen.partial_update(x, y, w, h);
        self.last_draw = Instant::now();
        self.pending_cleanup = None;
        self.cleanup_after = None;
        self.force_partial = true;
    }
}

impl Default for Refresh {
    fn default() -> Self {
        Self::new()
    }
}

fn merge(rects: &[DirtyRect]) -> Rect {
    let (mut x0, mut y0) = (i32::MAX, i32::MAX);
    let (mut x1, mut y1) = (0i32, 0i32);
    for r in rects {
        x0 = x0.min(r.x as i32);
        y0 = y0.min(r.y as i32);
        x1 = x1.max((r.x + r.w) as i32);
        y1 = y1.max((r.y + r.h) as i32);
    }
    Rect {
        x: x0,
        y: y0,
        w: (x1 - x0) as u32,
        h: (y1 - y0) as u32,
    }
}

fn union(a: Option<Rect>, b: Rect) -> Rect {
    match a {
        None => b,
        Some(a) => {
            let x0 = a.x.min(b.x);
            let y0 = a.y.min(b.y);
            let x1 = (a.x + a.w as i32).max(b.x + b.w as i32);
            let y1 = (a.y + a.h as i32).max(b.y + b.h as i32);
            Rect {
                x: x0,
                y: y0,
                w: (x1 - x0) as u32,
                h: (y1 - y0) as u32,
            }
        }
    }
}
