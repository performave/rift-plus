//! Owns the drag drop-region overlay and drives its animation.
//!
//! The overlay is drawn by rendering a layer into a window-server window, which
//! is a snapshot rather than something Core Animation keeps moving, so the
//! motion has to be advanced by hand. This actor holds a frame timer that runs
//! only while the region is still travelling and stops as soon as it settles —
//! a drag that is not moving between drop zones costs nothing.

use dispatchr::queue;
use dispatchr::time::Time;
use objc2::MainThreadMarker;
use objc2_core_foundation::CGRect;
use tracing::{debug, instrument, warn};

use crate::actor;
use crate::common::config::Config;
use crate::sys::dispatch::DispatchExt;
use crate::ui::drop_overlay::{DropOverlayConfig, DropOverlayWindow};
use crate::ui::stack_line::Color;

/// Frame interval while the region is moving, in nanoseconds. Roughly 60Hz.
///
/// rift runs its actors on its own executor rather than a Tokio runtime, so
/// there is no timer driver to await; frames are scheduled onto the main
/// dispatch queue, the same way deferred work is done elsewhere.
const FRAME_INTERVAL_NS: i64 = 16 * 1_000_000;

#[derive(Debug)]
pub enum Event {
    /// Point the overlay at a region of `screen`, both in global coordinates.
    Aim {
        screen: CGRect,
        region: CGRect,
    },
    /// The drag ended or left every drop target.
    Hide,
    /// Advance the animation one frame. Scheduled by the actor itself.
    Tick,
    ConfigUpdated(Config),
}

pub type Sender = actor::Sender<Event>;
pub type Receiver = actor::Receiver<Event>;

pub struct DropOverlay {
    rx: Receiver,
    tx: Sender,
    mtm: MainThreadMarker,
    config: Config,
    window: Option<DropOverlayWindow>,
    /// Whether a frame is already scheduled, so a burst of drag updates does
    /// not pile timers on top of each other.
    tick_scheduled: bool,
}

impl DropOverlay {
    pub fn new(config: Config, tx: Sender, rx: Receiver, mtm: MainThreadMarker) -> Self {
        Self {
            rx,
            tx,
            mtm,
            config,
            window: None,
            tick_scheduled: false,
        }
    }

    pub async fn run(mut self) {
        while let Some((span, event)) = self.rx.recv().await {
            let _guard = span.enter();
            let animating = self.handle_event(event);
            if animating {
                self.schedule_tick();
            }
        }
    }

    fn schedule_tick(&mut self) {
        if self.tick_scheduled {
            return;
        }
        self.tick_scheduled = true;
        queue::main().after_f_s(
            Time::new_after(Time::NOW, FRAME_INTERVAL_NS),
            self.tx.clone(),
            |tx| tx.send(Event::Tick),
        );
    }

    fn settings(&self) -> DropOverlayConfig {
        let settings = &self.config.settings.ui.drop_overlay;
        let defaults = DropOverlayConfig::default();
        DropOverlayConfig {
            corner_radius: settings.corner_radius,
            clear_style: settings.clear_style,
            tint: settings
                .tint
                .map(|[r, g, b, a]| Color::new(r, g, b, a))
                .unwrap_or(defaults.tint),
            follow_rate: settings.follow_rate,
        }
    }

    /// Returns whether the overlay still has frames to draw.
    #[instrument(name = "drop_overlay::handle_event", skip(self))]
    fn handle_event(&mut self, event: Event) -> bool {
        match event {
            Event::ConfigUpdated(config) => {
                self.config = config;
                // Colours and blur are baked into the window, so drop it and
                // let the next drag build one with the new settings.
                self.window = None;
                false
            }
            Event::Hide => {
                if let Some(window) = &self.window {
                    window.hide();
                }
                false
            }
            Event::Tick => {
                self.tick_scheduled = false;
                self.window.as_ref().is_some_and(DropOverlayWindow::step)
            }
            Event::Aim { screen, region } => {
                if !self.config.settings.ui.drop_overlay.enabled {
                    return false;
                }
                // One window per display: a drag that crosses displays needs a
                // window on the one it is over.
                let matches_screen =
                    self.window.as_ref().is_some_and(|window| window.screen() == screen);
                if !matches_screen {
                    if let Some(existing) = self.window.take() {
                        existing.hide();
                    }
                    match DropOverlayWindow::new(screen, self.settings(), self.mtm) {
                        Some(window) => self.window = Some(window),
                        None => {
                            warn!("could not create the drop overlay window");
                            return false;
                        }
                    }
                    debug!(?screen, "drop overlay created");
                }
                let Some(window) = &self.window else {
                    return false;
                };
                window.aim_at(region);
                true
            }
        }
    }
}
