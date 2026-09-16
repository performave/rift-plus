//! The region a dragged window would land in, drawn over the screen.
//!
//! yabai draws this feedback by creating a window-server window per tree node
//! and stroking a flat rectangle into it with CGContext, then destroying the
//! window when the target changes, so it blinks from place to place with hard
//! square edges (`insert_feedback_show` in its view.c).
//!
//! This uses an `NSGlassEffectView` in a borderless panel instead, which is the
//! system's Liquid Glass material. It samples and refracts what is behind it
//! live, which the window-server path cannot do at all: that renders a layer
//! tree with `renderInContext`, and a snapshot of a material defined by its
//! backdrop is just a still picture of one moment.
//!
//! Moving it is a plain `setFrame` on a real view, which the compositor
//! handles, rather than re-rendering a screen-sized layer tree every frame —
//! that snapshot, not the timer, is what made the earlier version stutter. The
//! caller advances `step` on a timer and the region eases toward its target, so
//! it glides between drop zones instead of jumping.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSGlassEffectView, NSGlassEffectViewStyle, NSPanel, NSScreen,
    NSStatusWindowLevel, NSView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use tracing::warn;

use crate::sys::screen::CoordinateConverter;
use crate::ui::stack_line::Color;

/// How close the drawn region has to get to its target before the animation is
/// considered finished, in points. Below a pixel there is nothing left to see.
const SETTLE_EPSILON: f64 = 0.5;

#[derive(Debug, Clone, Copy)]
pub struct DropOverlayConfig {
    pub tint: Color,
    pub corner_radius: f64,
    /// Whether to use the clearer of the two glass styles.
    pub clear_style: bool,
    /// Fraction of the remaining distance covered per frame, before easing.
    /// Higher is snappier; 1.0 removes the animation entirely.
    pub follow_rate: f64,
}

impl Default for DropOverlayConfig {
    fn default() -> Self {
        Self {
            tint: Color::new(0.0, 0.48, 1.0, 0.28),
            corner_radius: 12.0,
            clear_style: false,
            follow_rate: 0.35,
        }
    }
}

pub struct DropOverlayWindow {
    /// The display this covers, in the y-down space window frames use.
    screen: CGRect,
    config: DropOverlayConfig,
    panel: Retained<NSPanel>,
    glass: Retained<NSGlassEffectView>,
    /// Where the region is drawn now and where it is heading, both in
    /// panel-local coordinates.
    drawn: RefCell<Option<CGRect>>,
    target: RefCell<Option<CGRect>>,
    visible: Cell<bool>,
}

impl DropOverlayWindow {
    pub fn new(screen: CGRect, config: DropOverlayConfig, mtm: MainThreadMarker) -> Option<Self> {
        // NSGlassEffectView is macOS 26+, and it is the only thing in rift that
        // needs Tahoe. objc2 resolves classes by name at run time, so an older
        // system gets here fine and would then panic on `alloc` -- which, since
        // actors are supervised and a panicking actor takes the process with
        // it, would turn an opt-in cosmetic setting into a crash loop. Returning
        // None is a case every caller already handles: no overlay, drags still
        // work, everything else is unaffected.
        if AnyClass::get(c"NSGlassEffectView").is_none() {
            warn!("drop overlay needs macOS 26 (NSGlassEffectView); not showing one");
            return None;
        }

        // Panels live in Cocoa coordinates, which run y-up from the bottom of
        // the main display; window frames run y-down from its top.
        let converter = main_screen_converter(mtm)?;
        let panel_frame = converter.convert_rect(screen)?;

        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            panel_frame,
            // Borderless so there is no chrome, non-activating so showing it
            // never takes focus from the window being dragged.
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setLevel(NSStatusWindowLevel as isize);
        panel.setIgnoresMouseEvents(true);
        // Follow the user everywhere and never take part in window cycling:
        // this is feedback about a drag in progress, not a window.
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
        let glass = NSGlassEffectView::initWithFrame(
            NSGlassEffectView::alloc(mtm),
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0)),
        );
        glass.setCornerRadius(config.corner_radius);
        glass.setTintColor(Some(&config.tint.to_nscolor()));
        glass.setStyle(if config.clear_style {
            NSGlassEffectViewStyle::Clear
        } else {
            NSGlassEffectViewStyle::Regular
        });
        content.addSubview(&glass);
        panel.setContentView(Some(&content));

        Some(Self {
            screen,
            config,
            panel,
            glass,
            drawn: RefCell::new(None),
            target: RefCell::new(None),
            visible: Cell::new(false),
        })
    }

    pub fn screen(&self) -> CGRect { self.screen }

    /// Points the overlay at a region, in the same y-down space as window
    /// frames.
    ///
    /// The first region appears where it is asked for; later ones are eased
    /// toward, so moving between drop zones reads as one region moving rather
    /// than two regions blinking.
    pub fn aim_at(&self, region: CGRect) {
        // Panel-local coordinates run y-up from the panel's bottom-left.
        let local = CGRect::new(
            CGPoint::new(
                region.origin.x - self.screen.origin.x,
                self.screen.size.height
                    - (region.origin.y - self.screen.origin.y + region.size.height),
            ),
            region.size,
        );
        *self.target.borrow_mut() = Some(local);
        if self.drawn.borrow().is_none() {
            *self.drawn.borrow_mut() = Some(local);
        }
        self.present();
    }

    /// Advances the animation one frame. Returns whether more frames are
    /// needed, so the caller can stop its timer once the region has settled.
    pub fn step(&self) -> bool {
        let (Some(target), Some(drawn)) = (*self.target.borrow(), *self.drawn.borrow()) else {
            return false;
        };
        if rects_close(drawn, target) {
            *self.drawn.borrow_mut() = Some(target);
            self.present();
            return false;
        }
        *self.drawn.borrow_mut() = Some(approach(drawn, target, self.config.follow_rate));
        self.present();
        true
    }

    pub fn hide(&self) {
        *self.target.borrow_mut() = None;
        *self.drawn.borrow_mut() = None;
        if self.visible.replace(false) {
            self.panel.orderOut(None);
        }
    }

    fn present(&self) {
        let Some(frame) = *self.drawn.borrow() else {
            return;
        };
        self.glass.setFrame(frame);
        if !self.visible.replace(true) {
            // orderFrontRegardless rather than orderFront: the panel has to
            // appear without this process becoming active, since the user is
            // in the middle of dragging another application's window.
            self.panel.orderFrontRegardless();
        }
    }
}

impl Drop for DropOverlayWindow {
    fn drop(&mut self) { self.panel.orderOut(None); }
}

fn main_screen_converter(mtm: MainThreadMarker) -> Option<CoordinateConverter> {
    let screens = NSScreen::screens(mtm);
    let main = screens.iter().next()?;
    let converter = CoordinateConverter::from_screen(&main);
    if converter.is_none() {
        warn!("drop overlay could not resolve the main screen height");
    }
    converter
}

/// Moves `from` a fraction of the way toward `to`, eased so the region slows as
/// it arrives rather than stopping dead.
fn approach(from: CGRect, to: CGRect, rate: f64) -> CGRect {
    let s = ease(rate.clamp(0.0, 1.0));
    let blend = |a: f64, b: f64| a + (b - a) * s;
    CGRect::new(
        CGPoint::new(
            blend(from.origin.x, to.origin.x),
            blend(from.origin.y, to.origin.y),
        ),
        CGSize::new(
            blend(from.size.width, to.size.width),
            blend(from.size.height, to.size.height),
        ),
    )
}

fn rects_close(a: CGRect, b: CGRect) -> bool {
    (a.origin.x - b.origin.x).abs() < SETTLE_EPSILON
        && (a.origin.y - b.origin.y).abs() < SETTLE_EPSILON
        && (a.size.width - b.size.width).abs() < SETTLE_EPSILON
        && (a.size.height - b.size.height).abs() < SETTLE_EPSILON
}

/// Ease-out: most of the distance early, settling gently.
fn ease(t: f64) -> f64 { 1.0 - (1.0 - t).powi(3) }

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    #[test]
    fn approach_moves_toward_the_target_and_arrives() {
        let from = rect(0.0, 0.0, 100.0, 100.0);
        let to = rect(200.0, 100.0, 400.0, 300.0);

        let mut current = from;
        for _ in 0..200 {
            current = approach(current, to, 0.35);
        }
        assert!(rects_close(current, to), "never arrived: {current:?}");

        let one = approach(from, to, 0.35);
        assert!(one.origin.x > from.origin.x && one.origin.x < to.origin.x);
        assert!(one.size.width > from.size.width && one.size.width < to.size.width);
    }

    #[test]
    fn a_full_rate_lands_immediately() {
        let from = rect(0.0, 0.0, 10.0, 10.0);
        let to = rect(50.0, 50.0, 20.0, 20.0);
        assert!(rects_close(approach(from, to, 1.0), to));
    }
}
