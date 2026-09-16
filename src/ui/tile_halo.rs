//! A focus ring flashed over a window the float toggle has just moved into or
//! out of the tiling tree.
//!
//! The toggle is the one command whose effect can be entirely invisible. A
//! window the user has already sized by hand often lands on a frame it was
//! practically sitting on, and one that joins a stack covers its neighbours
//! exactly — in both cases nothing on screen changes, and the key reads as
//! broken when it worked. The ring says it happened.
//!
//! # Why the ring is drawn inside the window
//!
//! The obvious reading of a halo is a ring *around* the window, and it is
//! wrong here. The inner gap between two tiled windows is a handful of points;
//! anything drawn outside a frame spends that immediately, bleeds onto the
//! neighbour, and collides with the neighbour's own ring when two windows are
//! tiled in a row. Every layer below is therefore inset into the window's own
//! edge. The entry travel still starts outside, because it starts at zero
//! opacity and nothing is legible out there yet — and the spring's overshoot
//! carries it briefly *past* the frame, which is to say inward, where there is
//! room.
//!
//! # Why a ring and not glass
//!
//! `NSGlassEffectView` is a filled shape: the only way to make an outline of
//! it is four bars merged by an `NSGlassEffectContainerView`, whose merging
//! rule is documented as "sufficiently similar" and nothing more. A stroked
//! `CALayer` is certain, needs no version gate, and draws the shape AppKit
//! itself puts around a focused control — which is exactly what has happened:
//! this window is now the selected object in a structure.

use std::cell::{Cell, RefCell};
use std::time::Instant;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSColorSpace, NSPanel, NSScreen, NSStatusWindowLevel, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_quartz_core::CALayer;
use tracing::warn;

use crate::sys::screen::CoordinateConverter;
use crate::ui::common::with_disabled_actions;
use crate::ui::stack_line::Color;

/// Share of the flash spent fading out. The rest is the ring arriving and
/// holding, which is the part anybody reads.
const FADE_SHARE: f64 = 0.4;
/// How long the ring takes to reach full strength, in milliseconds. Opacity
/// leads the travel so the ring is already legible when it lands.
const RISE_MS: f64 = 70.0;

/// A spring with a touch of overshoot: what makes the ring read as snapping
/// onto something rather than sliding to a stop. Raise the damping toward
/// `2 * sqrt(stiffness)` — about 44 here — to remove the overshoot.
const SPRING_STIFFNESS: f64 = 480.0;
const SPRING_DAMPING: f64 = 32.0;
const SPRING_MASS: f64 = 1.0;
/// Seconds the spring above takes to settle: four time constants, where the
/// constant is `1 / (zeta * omega)`. Used to fit its motion into whatever
/// `duration_ms` the config asks for.
const SPRING_SETTLE_SECS: f64 = 0.25;

/// Gap between one stack rim and the next, in points.
const GHOST_STEP: f64 = 5.0;
/// Rims drawn behind the ring for a stacked landing. Two is enough to say
/// "there are more under this" and few enough not to read as a target.
const MAX_GHOSTS: usize = 2;

#[derive(Debug, Clone, Copy)]
pub struct TileHaloConfig {
    pub grow: f64,
    pub thickness: f64,
    pub corner_radius: f64,
    pub duration_ms: f64,
    /// `None` takes the user's system accent colour, which is the point.
    pub tile_color: Option<Color>,
    pub float_color: Option<Color>,
    pub stack_depth: bool,
}

impl Default for TileHaloConfig {
    fn default() -> Self {
        Self {
            grow: 14.0,
            thickness: 3.5,
            corner_radius: 14.0,
            duration_ms: 420.0,
            tile_color: None,
            float_color: None,
            stack_depth: true,
        }
    }
}

/// Which way the window went, which is what the ring's colour and direction of
/// travel report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HaloKind {
    /// Joined the tree. `stack_members` counts the windows sharing its stack,
    /// itself included, and is 0 when it did not land in one.
    Tiled { stack_members: usize },
    /// Left the tree.
    Floated,
}

impl HaloKind {
    fn releasing(self) -> bool { matches!(self, HaloKind::Floated) }
}

/// Where the ring is and how strongly it is drawn, at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Phase {
    pub opacity: f64,
    /// Points the ring stands outside the window's frame on every side.
    /// Negative means inside it, which is where the spring's overshoot goes.
    pub inflate: f64,
}

/// The unit step response of the spring above, at `t` seconds.
fn spring(t: f64) -> f64 {
    if t <= 0.0 {
        return 0.0;
    }
    let w0 = (SPRING_STIFFNESS / SPRING_MASS).sqrt();
    let zeta = SPRING_DAMPING / (2.0 * (SPRING_STIFFNESS * SPRING_MASS).sqrt());
    if zeta < 1.0 {
        let wd = w0 * (1.0 - zeta * zeta).sqrt();
        1.0 - (-zeta * w0 * t).exp() * ((wd * t).cos() + (zeta * w0 / wd) * (wd * t).sin())
    } else {
        1.0 - (-w0 * t).exp() * (1.0 + w0 * t)
    }
}

fn ease_in_out(t: f64) -> f64 {
    if t < 0.5 {
        2.0 * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
    }
}

/// The whole animation, as a function of how long ago it started. `None` once
/// it is over.
///
/// Opacity and travel are computed separately rather than carved into phases
/// that hand off to each other: a spring does not finish on a schedule, and
/// cutting one off at a phase boundary put a visible step in the arrival.
///
/// Tiling springs inward: the ring starts `grow` points out and settles onto
/// the frame. Floating does the opposite, holding on the frame and drifting
/// off it as it goes, so the two directions are told apart by motion as well
/// as by colour.
pub fn phase_at(elapsed_ms: f64, total_ms: f64, grow: f64, releasing: bool) -> Option<Phase> {
    if elapsed_ms < 0.0 || total_ms <= 0.0 || elapsed_ms >= total_ms {
        return None;
    }
    let fade_ms = total_ms * FADE_SHARE;
    let steady_ms = total_ms - fade_ms;
    let rise_ms = RISE_MS.min(steady_ms * 0.5);

    let opacity = if elapsed_ms < rise_ms {
        elapsed_ms / rise_ms
    } else if elapsed_ms < steady_ms {
        1.0
    } else {
        1.0 - ease_in_out((elapsed_ms - steady_ms) / fade_ms)
    };

    let inflate = if releasing {
        // Sits on the frame until it starts to go, then outward with the fade.
        if elapsed_ms < steady_ms {
            0.0
        } else {
            grow * ease_in_out((elapsed_ms - steady_ms) / fade_ms)
        }
    } else {
        // The spring is scaled to settle as the hold begins, so `duration_ms`
        // stays a real knob instead of only trimming the tail.
        grow * (1.0 - spring(SPRING_SETTLE_SECS * elapsed_ms / steady_ms))
    };

    Some(Phase { opacity, inflate })
}

/// One inset ring: the layer, how far inside the window's edge it sits, and
/// how heavy its stroke is.
struct Rim {
    layer: Retained<CALayer>,
    inset: f64,
}

pub struct TileHaloWindow {
    /// The display this covers, in the y-down space window frames use.
    screen: CGRect,
    config: TileHaloConfig,
    panel: Retained<NSPanel>,
    /// One opacity for the whole flash, so fading is a single property.
    group: Retained<CALayer>,
    /// Outermost first: the ring itself, the glow inside it, then any stack
    /// rims. All inset into the window, never outside it.
    rims: RefCell<Vec<Rim>>,
    /// The window's frame, in panel-local y-up coordinates.
    frame: RefCell<Option<CGRect>>,
    kind: Cell<HaloKind>,
    started: Cell<Option<Instant>>,
    /// `grow` with Reduce Motion applied: zero when the user has asked for
    /// less movement, which leaves the ring to fade in place.
    travel: Cell<f64>,
    visible: Cell<bool>,
}

impl TileHaloWindow {
    pub fn new(screen: CGRect, config: TileHaloConfig, mtm: MainThreadMarker) -> Option<Self> {
        // Panels live in Cocoa coordinates, which run y-up from the bottom of
        // the main display; window frames run y-down from its top.
        let converter = main_screen_converter(mtm)?;
        let panel_frame = converter.convert_rect(screen)?;

        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            panel_frame,
            // Borderless so there is no chrome, non-activating so a flash
            // never takes focus from the window it is drawn over.
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setLevel(NSStatusWindowLevel as isize);
        panel.setIgnoresMouseEvents(true);
        // Feedback about a command, not a window: it follows the user
        // everywhere and takes no part in cycling.
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );

        let content = NSView::initWithFrame(
            NSView::alloc(mtm),
            CGRect::new(CGPoint::new(0.0, 0.0), panel_frame.size),
        );
        content.setWantsLayer(true);
        let root = content.layer()?;

        let group = CALayer::layer();
        group.setOpacity(0.0);
        root.addSublayer(&group);
        panel.setContentView(Some(&content));

        Some(Self {
            screen,
            config,
            panel,
            group,
            rims: RefCell::new(Vec::new()),
            frame: RefCell::new(None),
            kind: Cell::new(HaloKind::Tiled { stack_members: 0 }),
            started: Cell::new(None),
            travel: Cell::new(config.grow),
            visible: Cell::new(false),
        })
    }

    pub fn screen(&self) -> CGRect { self.screen }

    /// Starts a flash over `frame`, given in the same y-down space as window
    /// frames. A flash already running is replaced rather than queued: the
    /// user has pressed the key again, and the answer they want is about the
    /// window they pressed it on.
    pub fn flash(&self, frame: CGRect, kind: HaloKind, mtm: MainThreadMarker) {
        // Panel-local coordinates run y-up from the panel's bottom-left.
        let local = CGRect::new(
            CGPoint::new(
                frame.origin.x - self.screen.origin.x,
                self.screen.size.height
                    - (frame.origin.y - self.screen.origin.y + frame.size.height),
            ),
            frame.size,
        );
        *self.frame.borrow_mut() = Some(local);
        self.kind.set(kind);
        self.started.set(Some(Instant::now()));
        self.travel.set(if reduce_motion() {
            0.0
        } else {
            self.config.grow
        });
        self.build_rims(kind, mtm);
        self.present(Phase {
            opacity: 0.0,
            inflate: self.travel.get(),
        });
    }

    /// Draws the flash as it stands now. Returns whether it has frames left,
    /// so the caller can stop its timer the moment it finishes.
    pub fn step(&self) -> bool {
        let Some(started) = self.started.get() else {
            return false;
        };
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        let Some(phase) = phase_at(
            elapsed,
            self.config.duration_ms,
            self.travel.get(),
            self.kind.get().releasing(),
        ) else {
            self.hide();
            return false;
        };
        self.present(phase);
        true
    }

    pub fn hide(&self) {
        self.started.set(None);
        *self.frame.borrow_mut() = None;
        if self.visible.replace(false) {
            self.panel.orderOut(None);
        }
    }

    /// Rebuilds the stack of rims for one flash. Colours and stroke weights
    /// change only when a flash begins, so this runs once rather than per
    /// frame.
    fn build_rims(&self, kind: HaloKind, mtm: MainThreadMarker) {
        let base = match kind {
            HaloKind::Tiled { .. } => self.config.tile_color.unwrap_or_else(|| accent_color(mtm)),
            HaloKind::Floated => self.config.float_color.unwrap_or(UNEMPHASIZED),
        };
        let w = self.config.thickness;

        // The ring, then a wider, fainter stroke just inside it. A CALayer
        // border cannot be blurred, so the glow is a second stroke rather than
        // a real falloff — at these widths the difference does not show.
        let mut spec = vec![(0.0, w, base.a), (w, w * 1.8, base.a * 0.22)];
        // A stacked landing adds a rim per member behind the ring. Inward,
        // because outward is the neighbour's gap; the read is the same.
        if let HaloKind::Tiled { stack_members } = kind
            && self.config.stack_depth
            && stack_members > 1
        {
            let ghosts = (stack_members - 1).min(MAX_GHOSTS);
            for index in 0..ghosts {
                let inset = w * 3.4 + index as f64 * GHOST_STEP;
                spec.push((inset, w * 0.75, base.a * (0.34 - index as f64 * 0.14)));
            }
        }

        let mut rims = self.rims.borrow_mut();
        with_disabled_actions(|| {
            while rims.len() > spec.len() {
                if let Some(rim) = rims.pop() {
                    rim.layer.removeFromSuperlayer();
                }
            }
            while rims.len() < spec.len() {
                let layer = CALayer::layer();
                self.group.addSublayer(&layer);
                rims.push(Rim { layer, inset: 0.0 });
            }
            for (rim, &(inset, width, alpha)) in rims.iter_mut().zip(spec.iter()) {
                rim.inset = inset;
                rim.layer.setBorderWidth(width);
                let color = Color::new(base.r, base.g, base.b, alpha);
                rim.layer.setBorderColor(Some(&color.to_nscolor().CGColor()));
                // Concentric: a rounded shape nested inside another shares its
                // centre, so the inner radius is the outer one less the inset.
                // Apple ships this rule as `containerConcentric` in macOS 27;
                // here it is one subtraction.
                rim.layer.setCornerRadius((self.config.corner_radius - inset).max(0.0));
            }
        });
    }

    fn present(&self, phase: Phase) {
        let Some(frame) = *self.frame.borrow() else {
            return;
        };
        let rims = self.rims.borrow();
        with_disabled_actions(|| {
            for rim in rims.iter() {
                // The flash's travel moves every rim together; each rim's own
                // inset is what holds it inside the window.
                rim.layer.setFrame(inset_by(frame, rim.inset - phase.inflate));
            }
            self.group.setOpacity(phase.opacity as f32);
        });

        if !self.visible.replace(true) {
            // orderFrontRegardless rather than orderFront: the flash has to
            // appear without this process becoming active, or confirming a
            // command would steal focus from the window it confirms.
            self.panel.orderFrontRegardless();
        }
    }
}

impl Drop for TileHaloWindow {
    fn drop(&mut self) { self.panel.orderOut(None); }
}

/// Shrinks `rect` by `amount` on every side. A negative amount grows it.
fn inset_by(rect: CGRect, amount: f64) -> CGRect {
    CGRect::new(
        CGPoint::new(rect.origin.x + amount, rect.origin.y + amount),
        CGSize::new(
            (rect.size.width - amount * 2.0).max(0.0),
            (rect.size.height - amount * 2.0).max(0.0),
        ),
    )
}

/// Grey, for a window leaving the tree: macOS says "selected, but not the
/// focus" in exactly this register, and it cannot be mistaken for the accent
/// whatever the user has set that to.
const UNEMPHASIZED: Color = Color {
    r: 0.62,
    g: 0.65,
    b: 0.70,
    a: 1.0,
};

/// The colour the user picked in System Settings.
///
/// `controlAccentColor` rather than `keyboardFocusIndicatorColor`: the focus
/// colour carries an alpha of its own, and the rims here derive their alphas
/// from the base, so starting from an opaque colour keeps that arithmetic
/// honest.
fn accent_color(mtm: MainThreadMarker) -> Color {
    let _ = mtm;
    let accent = NSColor::controlAccentColor();
    // A catalog colour has no components until it is resolved into a real
    // colour space; asking one for `redComponent` raises.
    let Some(srgb) = accent.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace()) else {
        warn!("could not resolve the system accent colour; falling back to blue");
        return Color::new(0.0, 0.48, 1.0, 1.0);
    };
    Color::new(
        srgb.redComponent(),
        srgb.greenComponent(),
        srgb.blueComponent(),
        srgb.alphaComponent(),
    )
}

/// Whether the user has asked for less movement. Checked per flash rather than
/// cached: it is one message, and a setting that can change at any time.
fn reduce_motion() -> bool {
    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

fn main_screen_converter(mtm: MainThreadMarker) -> Option<CoordinateConverter> {
    let screens = NSScreen::screens(mtm);
    let main = screens.iter().next()?;
    let converter = CoordinateConverter::from_screen(&main);
    if converter.is_none() {
        warn!("tile halo could not resolve the main screen height");
    }
    converter
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOTAL: f64 = 420.0;
    const GROW: f64 = 14.0;

    fn tile(t: f64) -> Phase { phase_at(t, TOTAL, GROW, false).expect("still running") }

    #[test]
    fn the_flash_ends_and_stays_ended() {
        assert!(phase_at(TOTAL, TOTAL, GROW, false).is_none());
        assert!(phase_at(TOTAL + 1.0, TOTAL, GROW, false).is_none());
        assert!(phase_at(-1.0, TOTAL, GROW, false).is_none());
        // A zero duration is a configuration asking for no flash at all, not
        // one that runs forever.
        assert!(phase_at(0.0, 0.0, GROW, false).is_none());
    }

    #[test]
    fn tiling_springs_inward_and_settles_on_the_frame() {
        let start = tile(1.0);
        assert!(start.opacity < 0.1, "starts invisible: {start:?}");
        assert!(start.inflate > GROW * 0.9, "starts out wide: {start:?}");

        // Settled by the time the hold ends, which is what lets the ring be
        // read as being about this window rather than about a moment.
        let steady = TOTAL * (1.0 - FADE_SHARE);
        let held = tile(steady - 1.0);
        assert_eq!(held.opacity, 1.0);
        assert!(held.inflate.abs() < 0.5, "not settled: {held:?}");
    }

    /// The spring's overshoot is what makes the arrival read as a snap, and it
    /// has to go inward: outward is the neighbouring window's gap.
    #[test]
    fn the_overshoot_goes_inside_the_frame() {
        let steady = TOTAL * (1.0 - FADE_SHARE);
        let mut min_inflate = f64::INFINITY;
        for step in 1..=200 {
            let t = steady * step as f64 / 200.0;
            min_inflate = min_inflate.min(tile(t).inflate);
        }
        assert!(min_inflate < 0.0, "the spring never overshot: {min_inflate}");
        assert!(
            min_inflate > -GROW * 0.25,
            "overshoot is meant to be a hint, not a bounce: {min_inflate}"
        );
    }

    #[test]
    fn floating_holds_still_and_then_drifts_out() {
        let steady = TOTAL * (1.0 - FADE_SHARE);
        let held = phase_at(steady * 0.5, TOTAL, GROW, true).unwrap();
        assert_eq!(held.inflate, 0.0, "sits on the frame while it holds: {held:?}");

        let leaving = phase_at(TOTAL * 0.98, TOTAL, GROW, true).unwrap();
        assert!(leaving.opacity < 0.15, "nearly gone: {leaving:?}");
        assert!(leaving.inflate > GROW * 0.8, "well outside: {leaving:?}");
    }

    /// What Reduce Motion asks for, expressed as a zero travel: the ring still
    /// says what happened, it just does not move to say it.
    #[test]
    fn a_zero_travel_removes_the_motion_but_not_the_fade() {
        let mut faded = false;
        for step in 0..200 {
            let t = TOTAL * step as f64 / 200.0;
            let Some(phase) = phase_at(t, TOTAL, 0.0, false) else {
                continue;
            };
            assert_eq!(phase.inflate, 0.0, "moved at {t}ms");
            if phase.opacity > 0.0 && phase.opacity < 1.0 {
                faded = true;
            }
        }
        assert!(faded, "the ring never faded");
    }

    /// Whatever the configured length, the ring arrives, holds and leaves — a
    /// short duration must not cut the spring off mid-flight.
    #[test]
    fn every_duration_gets_a_whole_animation() {
        for &total in &[150.0, 420.0, 1200.0] {
            let steady = total * (1.0 - FADE_SHARE);
            let held = phase_at(steady - 1.0, total, GROW, false).unwrap();
            assert_eq!(held.opacity, 1.0, "no hold at {total}ms");
            assert!(
                held.inflate.abs() < 0.5,
                "the spring was still travelling at {total}ms: {held:?}"
            );
        }
    }
}
