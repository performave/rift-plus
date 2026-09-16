//! Owns the tile-confirmation halo and drives its animation.
//!
//! The same shape as the drop overlay actor: a frame timer that runs only
//! while a flash is on screen and stops the moment it finishes, so the
//! overwhelming majority of the time this costs nothing at all. Unlike that
//! one, a flash has a fixed length nobody else can extend, so the actor keeps
//! ticking until the window says it is done rather than until a target stops
//! moving.

use dispatchr::queue;
use dispatchr::time::Time;
use objc2::MainThreadMarker;
use objc2_core_foundation::CGRect;
use tracing::{debug, instrument, warn};

use crate::actor;
use crate::common::config::Config;
use crate::sys::dispatch::DispatchExt;
use crate::ui::stack_line::Color;
use crate::ui::tile_halo::{HaloKind, TileHaloConfig, TileHaloWindow};

/// Frame interval while a flash is on screen, in nanoseconds. Roughly 60Hz.
///
/// rift runs its actors on its own executor rather than a Tokio runtime, so
/// there is no timer driver to await; frames are scheduled onto the main
/// dispatch queue, the same way deferred work is done elsewhere.
const FRAME_INTERVAL_NS: i64 = 16 * 1_000_000;

#[derive(Debug)]
pub enum Event {
    /// Flash `frame` on `screen`, both in global coordinates.
    Flash {
        screen: CGRect,
        frame: CGRect,
        kind: HaloKind,
    },
    /// Advance the animation one frame. Scheduled by the actor itself.
    Tick,
    ConfigUpdated(Config),
}

pub type Sender = actor::Sender<Event>;
pub type Receiver = actor::Receiver<Event>;

pub struct TileHalo {
    rx: Receiver,
    tx: Sender,
    mtm: MainThreadMarker,
    config: Config,
    window: Option<TileHaloWindow>,
    /// Whether a frame is already scheduled, so nothing piles timers on top
    /// of each other.
    tick_scheduled: bool,
}

impl TileHalo {
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

    fn settings(&self) -> TileHaloConfig {
        let settings = &self.config.settings.ui.tile_halo;
        let rgb = |c: [f64; 3]| Color::new(c[0], c[1], c[2], 1.0);
        TileHaloConfig {
            grow: settings.grow,
            thickness: settings.thickness,
            corner_radius: settings.corner_radius,
            duration_ms: settings.duration_ms,
            // Absent means the system accent colour, which the window resolves
            // for itself: it is a live setting, and the config cannot hold one.
            tile_color: settings.tile_color.map(rgb),
            float_color: settings.float_color.map(rgb),
            stack_depth: settings.stack_depth,
        }
    }

    /// Returns whether the halo still has frames to draw.
    #[instrument(name = "tile_halo::handle_event", skip(self))]
    fn handle_event(&mut self, event: Event) -> bool {
        match event {
            Event::ConfigUpdated(config) => {
                self.config = config;
                // Colours and widths are baked into the layers, so drop the
                // window and let the next flash build one with the new
                // settings.
                if let Some(existing) = self.window.take() {
                    existing.hide();
                }
                false
            }
            Event::Tick => {
                self.tick_scheduled = false;
                self.window.as_ref().is_some_and(TileHaloWindow::step)
            }
            Event::Flash { screen, frame, kind } => {
                if !self.config.settings.ui.tile_halo.enabled {
                    return false;
                }
                if matches!(kind, HaloKind::Floated) && !self.config.settings.ui.tile_halo.on_float
                {
                    return false;
                }
                // One window per display: the toggle can be pressed on either
                // one, and the panel has to be on the display the window is.
                let matches_screen =
                    self.window.as_ref().is_some_and(|window| window.screen() == screen);
                if !matches_screen {
                    if let Some(existing) = self.window.take() {
                        existing.hide();
                    }
                    match TileHaloWindow::new(screen, self.settings(), self.mtm) {
                        Some(window) => self.window = Some(window),
                        None => {
                            warn!("could not create the tile halo window");
                            return false;
                        }
                    }
                    debug!(?screen, "tile halo created");
                }
                let Some(window) = &self.window else {
                    return false;
                };
                window.flash(frame, kind, self.mtm);
                true
            }
        }
    }
}
