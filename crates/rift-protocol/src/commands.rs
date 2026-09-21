use std::path::PathBuf;

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{
    Direction, DisplaySelector, LayoutMode, MirrorAxis, ResizeOrientation, RestoreScope,
    RestoreSource, RotateDegrees, WindowId, WorkspaceSelector,
};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FloatingWindowSizePreset {
    Smart,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FloatingWindowSize {
    Dimensions { w: f64, h: f64 },
    Preset(FloatingWindowSizePreset),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToggleWindowFloatingOptions {
    #[serde(default)]
    pub center: bool,
    #[serde(default)]
    pub size: Option<FloatingWindowSize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutCommand {
    NextWindow,
    PrevWindow,
    MoveFocus(#[serde(rename = "direction")] Direction),
    Ascend,
    Descend,
    MoveNode(Direction),
    JoinWindow(Direction),
    ConsumeOrExpelWindow(Direction),
    ToggleStack,
    ToggleOrientation,
    /// Reset every split in the active workspace to an even share.
    Balance,
    Rotate(RotateDegrees),
    Mirror(MirrorAxis),
    UnjoinWindows,
    ToggleFocusFloating,
    ToggleWindowFloating,
    ToggleWindowFloatingWithOptions(ToggleWindowFloatingOptions),
    ToggleFullscreen,
    ToggleFullscreenWithinGaps,
    ResizeWindowGrow(ResizeOrientation),
    ResizeWindowShrink(ResizeOrientation),
    ResizeWindowBy {
        amount: f64,
    },
    ScrollStrip {
        delta: f64,
    },
    SnapStrip,
    CenterSelection,
    NextWorkspace(Option<bool>),
    PrevWorkspace(Option<bool>),
    SwitchToWorkspace(usize),
    MoveWindowToWorkspace {
        workspace: WorkspaceSelector,
        follow: bool,
        window_id: Option<u32>,
    },
    SetWorkspaceLayout {
        workspace: Option<usize>,
        mode: LayoutMode,
    },
    /// Cycle the active workspace through the given layout modes.
    ///
    /// yabai had no such command either; its users wrote the toggle out as a
    /// shell pipeline that queried the space's current type and picked the
    /// other one. Two modes make that a toggle; more make it a cycle.
    ToggleWorkspaceLayout(Vec<LayoutMode>),
    CreateWorkspace,
    DestroyWorkspace,
    SwitchToLastWorkspace,
    SwapWindows(WindowId, WindowId),
    AdjustMasterRatio(f64),
    AdjustMasterCount {
        delta: i32,
    },
    PromoteToMaster,
    SwapMasterStack,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactorCommand {
    Debug,
    /// Start (`Some(path)`) or stop (`None`) recording a trace of everything
    /// the reactor sees — events and the system's answers — for offline
    /// replay. See `sys::trace`.
    RecordTrace {
        path: Option<PathBuf>,
    },
    /// Dump the always-on flight recorder — the last minutes of everything
    /// the reactor saw and every thread did — to a file, retroactively. No
    /// `RecordTrace` needs to have been started: reproduce first, dump after.
    DumpTrace {
        path: PathBuf,
    },
    Serialize,
    SaveLayout {
        path: PathBuf,
    },
    /// The layout heartbeat. Writes the same file `SaveLayout` does, without
    /// the log line and the reply: it runs every minute for the life of the
    /// process, and a save nobody asked for should not be heard from.
    AutosaveLayout {
        path: PathBuf,
    },
    SaveAndExit,
    RestoreLayout {
        path: PathBuf,
        scope: RestoreScope,
        #[serde(default)]
        source: RestoreSource,
    },
    SwitchSpace(Direction),
    /// Switch to a macOS space by its position on the active display, 1-based.
    SwitchToSpace(usize),
    /// Move a window to a macOS space by its position on the active display,
    /// 1-based. Defaults to the focused window. Needs yabai's scripting
    /// addition: macOS 26 has no unprivileged API left that can do this.
    MoveWindowToSpace {
        index: usize,
        #[serde(default)]
        follow: bool,
    },
    /// Create a macOS space on the active display. Scripting addition only.
    CreateSpace,
    /// Destroy the active macOS space. Scripting addition only.
    DestroySpace,
    /// Put the active macOS space's layout back the way it was when a display
    /// last departed. In spaces mode a desktop rearranged while a display was
    /// away keeps the rearrangement on replug; this undoes that on demand.
    RestoreDepartureLayout,
    ToggleSpaceActivated,
    FocusWindow {
        window_id: WindowId,
        window_server_id: Option<u32>,
    },
    ShowMissionControlAll,
    ShowMissionControlCurrent,
    DismissMissionControl,
    MoveMouseToDisplay(DisplaySelector),
    FocusDisplay(DisplaySelector),
    CloseWindow {
        window_server_id: Option<u32>,
    },
    MoveWindowToDisplay {
        selector: DisplaySelector,
        window_id: Option<u32>,
    },
    /// Move the active workspace to another display.
    ///
    /// Rift workspaces are display-local, so windows move into the destination
    /// display's workspace at the same ordinal and that workspace becomes active.
    MoveWorkspaceToDisplay {
        selector: DisplaySelector,
        /// Continue from the opposite edge when a directional selector has no
        /// display further in that direction.
        #[serde(default)]
        wrap_around: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricsCommand {
    ShowTiming,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigCommand {
    SetAnimate(bool),
    SetAnimationDuration(f64),
    SetAnimationFps(f64),
    SetAnimationEasing(AnimationEasing),
    SetMouseFollowsFocus(bool),
    SetMouseHidesOnFocus(bool),
    SetFocusFollowsMouse(bool),
    SetStackOffset(f64),
    SetOuterGaps {
        top: f64,
        left: f64,
        bottom: f64,
        right: f64,
    },
    SetInnerGaps {
        horizontal: f64,
        vertical: f64,
    },
    SetWorkspaceNames(Vec<String>),
    Set {
        key: String,
        value: Value,
    },
    GetConfig,
    SaveConfig,
    ReloadConfig,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnimationEasing {
    #[default]
    EaseInOut,
    Linear,
    EaseInSine,
    EaseOutSine,
    EaseInOutSine,
    EaseInQuad,
    EaseOutQuad,
    EaseInOutQuad,
    EaseInCubic,
    EaseOutCubic,
    EaseInOutCubic,
    EaseInQuart,
    EaseOutQuart,
    EaseInOutQuart,
    EaseInQuint,
    EaseOutQuint,
    EaseInOutQuint,
    EaseInExpo,
    EaseOutExpo,
    EaseInOutExpo,
    EaseInCirc,
    EaseOutCirc,
    EaseInOutCirc,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiftCommand {
    Layout(LayoutCommand),
    Metrics(MetricsCommand),
    Reactor(ReactorCommand),
    Config(ConfigCommand),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum TypedRiftCommand {
    Layout(LayoutCommand),
    Metrics(MetricsCommand),
    Reactor(ReactorCommand),
    Config(ConfigCommand),
}

#[derive(Deserialize)]
enum LegacyCommand {
    #[serde(alias = "reactor")]
    Reactor(LegacyReactorCommand),
    #[serde(alias = "config")]
    Config(ConfigCommand),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LegacyReactorCommand {
    Layout(LayoutCommand),
    Metrics(MetricsCommand),
    Reactor(ReactorCommand),
}

impl From<TypedRiftCommand> for RiftCommand {
    fn from(command: TypedRiftCommand) -> Self {
        match command {
            TypedRiftCommand::Layout(command) => Self::Layout(command),
            TypedRiftCommand::Metrics(command) => Self::Metrics(command),
            TypedRiftCommand::Reactor(command) => Self::Reactor(command),
            TypedRiftCommand::Config(command) => Self::Config(command),
        }
    }
}

impl<'de> Deserialize<'de> for RiftCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum CommandInput {
            Typed(TypedRiftCommand),
            LegacyJson(String),
        }

        match CommandInput::deserialize(deserializer)? {
            CommandInput::Typed(command) => Ok(command.into()),
            CommandInput::LegacyJson(command) => decode_legacy_command(&command),
        }
    }
}

fn decode_legacy_command<E>(command: &str) -> Result<RiftCommand, E>
where E: DeError {
    match serde_json::from_str::<LegacyCommand>(command)
        .map_err(|error| E::custom(format!("invalid legacy command JSON: {error}")))?
    {
        LegacyCommand::Config(command) => Ok(RiftCommand::Config(command)),
        LegacyCommand::Reactor(LegacyReactorCommand::Layout(command)) => {
            Ok(RiftCommand::Layout(command))
        }
        LegacyCommand::Reactor(LegacyReactorCommand::Metrics(command)) => {
            Ok(RiftCommand::Metrics(command))
        }
        LegacyCommand::Reactor(LegacyReactorCommand::Reactor(command)) => {
            Ok(RiftCommand::Reactor(command))
        }
    }
}
