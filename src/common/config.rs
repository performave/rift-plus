use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::bail;
use regex::RegexBuilder;
pub use rift_protocol::{AnimationEasing, ConfigCommand, LayoutMode, WorkspaceSelector};
use serde::{Deserialize, Serialize};

use super::collections::HashMap;
use crate::actor::wm_controller::WmCommand;
use crate::sys::hotkey::{Hotkey, HotkeySpec, Modifiers};

pub const MAX_WORKSPACES: usize = 128;

// TODO: when to remove these?
const DEPRECATED_MAP: &[(&str, &str)] = &[
    ("stack_windows", "toggle_stack"),
    ("unstack_windows", "toggle_stack"),
    ("toggle_tile_orientation", "toggle_orientation"),
];

pub fn data_dir() -> PathBuf { dirs::home_dir().unwrap().join(".rift") }
pub fn restore_file() -> PathBuf { data_dir().join("layout.ron") }
pub fn config_file() -> PathBuf {
    dirs::home_dir().unwrap().join(".config").join("rift").join("config.toml")
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct VirtualWorkspaceSettings {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_workspace_count")]
    pub default_workspace_count: usize,
    #[serde(default = "yes")]
    pub auto_assign_windows: bool,
    #[serde(default = "yes")]
    pub preserve_focus_per_workspace: bool,
    #[serde(default = "no")]
    pub workspace_auto_back_and_forth: bool,
    #[serde(default, alias = "prevent_wrapping_around")]
    pub prevent_wrapping: bool,
    #[serde(default = "default_workspace_names")]
    pub workspace_names: Vec<String>,
    #[serde(default)]
    pub default_workspace: usize,
    #[serde(default)]
    pub reapply_app_rules_on_title_change: bool,
    #[serde(default)]
    pub app_rules: Vec<AppWorkspaceRule>,
    #[serde(default)]
    pub workspace_rules: Vec<WorkspaceLayoutRule>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLayoutRule {
    /// Target workspace by index or name
    pub workspace: WorkspaceSelector,
    /// Layout mode to use for this workspace
    pub layout: LayoutMode,
}

// Allow specifying a workspace by numeric index or by name in the config.
// This supports both `workspace = 2` and `workspace = "coding"` in app rules.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct AppWorkspaceRule {
    /// Application bundle identifier (e.g., "com.apple.Terminal")
    pub app_id: Option<String>,
    /// Target workspace index (0 based) OR workspace name. If None, window goes to active workspace.
    pub workspace: Option<WorkspaceSelector>,
    /// Whether windows should be floating in this workspace
    #[serde(default)]
    pub floating: bool,
    /// Initial normalized position for a floating window. `(0, 0)` is the top-left
    /// and `(1, 1)` is the bottom-right of the available screen area.
    pub position: Option<AppRulePosition>,
    /// Preferred window size in logical pixels.
    pub size: Option<AppRuleSize>,
    /// Focus the window after applying this rule, switching virtual workspaces if needed.
    #[serde(default)]
    pub focus: bool,
    /// An explicit management override. `false` makes the window invisible to Rift;
    /// `true` overrides normal manageability heuristics for a visible window. When
    /// omitted, the matching rule leaves Rift's normal manageability decision intact.
    #[serde(default)]
    pub manage: Option<bool>,
    /// Optional: Application name pattern (alternative to app_id)
    pub app_name: Option<String>,
    /// Optional: Regular expression to match window title (applies to window.title)
    ///
    /// If present, this regex will be used when attempting to match a window by
    /// title.
    pub title_regex: Option<String>,
    /// Optional: Substring to search for in window title (applies to window.title)
    ///
    /// If present, rift will internally treat this as a substring match and will
    /// construct a regex to match titles containing this substring. This allows
    /// people who don't want to write full regexes to match by a simple substring.
    pub title_substring: Option<String>,

    /// Optional: Accessibility role to match (AXRole). If present, it must be a
    /// non-empty string and will be compared against the accessibility role
    /// reported by the AX APIs for a window (exact string match).
    pub ax_role: Option<String>,

    /// Optional: Accessibility subrole to match (AXSubrole). If present, it must be a
    /// non-empty string and will be compared against the accessibility subrole
    /// reported by the AX APIs for a window (exact string match).
    pub ax_subrole: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct AppRulePosition {
    pub x: f64,
    pub y: f64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct AppRuleSize {
    pub w: Option<f64>,
    pub h: Option<f64>,
}

impl Default for VirtualWorkspaceSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            default_workspace_count: default_workspace_count(),
            auto_assign_windows: true,
            preserve_focus_per_workspace: true,
            workspace_auto_back_and_forth: false,
            prevent_wrapping: false,
            workspace_names: default_workspace_names(),
            default_workspace: 0,
            reapply_app_rules_on_title_change: false,
            app_rules: Vec::new(),
            workspace_rules: Vec::new(),
        }
    }
}

impl VirtualWorkspaceSettings {
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        if self.default_workspace_count == 0 {
            issues.push("default_workspace_count must be at least 1".to_string());
        }
        if self.default_workspace_count > MAX_WORKSPACES {
            issues.push(format!(
                "default_workspace_count should not exceed {} for performance reasons",
                MAX_WORKSPACES
            ));
        }

        if self.workspace_names.len() > self.default_workspace_count {
            issues.push("More workspace names provided than default_workspace_count".to_string());
        }

        if self.default_workspace >= self.default_workspace_count {
            issues.push(format!(
                "default_workspace ({}) must be less than default_workspace_count ({})",
                self.default_workspace, self.default_workspace_count
            ));
        }

        // Validate rules and check duplicates in a single pass
        let mut seen_app_ids = crate::common::collections::HashSet::default();
        let mut seen_app_names = crate::common::collections::HashSet::default();
        let mut seen_title_regexes = crate::common::collections::HashSet::default();
        let mut seen_title_substrings = crate::common::collections::HashSet::default();
        let mut seen_ax_roles = crate::common::collections::HashSet::default();
        let mut seen_ax_subroles = crate::common::collections::HashSet::default();

        for (index, rule) in self.app_rules.iter().enumerate() {
            let app_id_empty = rule.app_id.as_ref().map_or(true, |id| id.is_empty());
            if app_id_empty
                && rule.app_name.is_none()
                && rule.title_regex.is_none()
                && rule.title_substring.is_none()
                && rule.ax_role.is_none()
                && rule.ax_subrole.is_none()
            {
                issues.push(format!(
                    "App rule {} has no app_id, app_name, title_regex, or title_substring specified",
                    index
                ));
            }

            if let Some(ref workspace) = rule.workspace {
                if let WorkspaceSelector::Index(idx) = workspace {
                    if *idx >= self.default_workspace_count {
                        issues.push(format!(
                            "App rule {} references workspace {} but only {} workspaces will be created",
                            index, idx, self.default_workspace_count
                        ));
                    }
                }
            }

            if let Some(position) = rule.position {
                if !position.x.is_finite()
                    || !position.y.is_finite()
                    || !(0.0..=1.0).contains(&position.x)
                    || !(0.0..=1.0).contains(&position.y)
                {
                    issues.push(format!(
                        "App rule {} position x and y must be finite values between 0 and 1",
                        index
                    ));
                }
                if !rule.floating {
                    issues.push(format!(
                        "App rule {} specifies position, but position only applies when floating = true",
                        index
                    ));
                }
            }

            if let Some(size) = rule.size {
                if size.w.is_none() && size.h.is_none() {
                    issues.push(format!(
                        "App rule {} size must specify at least one of w or h",
                        index
                    ));
                }
                if size.w.is_some_and(|value| !value.is_finite() || value <= 0.0)
                    || size.h.is_some_and(|value| !value.is_finite() || value <= 0.0)
                {
                    issues.push(format!(
                        "App rule {} size dimensions must be finite positive values",
                        index
                    ));
                }
            }

            if let Some(ref app_id) = rule.app_id {
                if !app_id.is_empty() && !app_id.contains('.') {
                    issues.push(format!(
                        "App rule {} has suspicious app_id '{}' (should be bundle identifier like 'com.example.app')",
                        index, app_id
                    ));
                }

                let has_specific_match = rule.app_name.is_some()
                    || rule.title_regex.is_some()
                    || rule.title_substring.is_some()
                    || rule.ax_role.is_some()
                    || rule.ax_subrole.is_some();
                if !app_id.is_empty() && !has_specific_match && !seen_app_ids.insert(app_id) {
                    issues.push(format!("Duplicate app_id '{}' in rule {}", app_id, index));
                }
            }

            if let Some(ref app_name) = rule.app_name {
                if !seen_app_names.insert(app_name) {
                    issues.push(format!("Duplicate app_name '{}' in rule {}", app_name, index));
                }
            }

            if let Some(ref title_re) = rule.title_regex {
                if title_re.is_empty() {
                    issues.push(format!("App rule {} has empty title_regex", index));
                } else if let Err(error) =
                    RegexBuilder::new(title_re).case_insensitive(true).build()
                {
                    issues.push(format!(
                        "App rule {} has invalid title_regex '{}': {}",
                        index, title_re, error
                    ));
                } else if !seen_title_regexes.insert(title_re) {
                    issues.push(format!("Duplicate title_regex '{}' in rule {}", title_re, index));
                }
            }

            if rule.manage == Some(false)
                && (rule.workspace.is_some()
                    || rule.floating
                    || rule.position.is_some()
                    || rule.size.is_some()
                    || rule.focus)
            {
                issues.push(format!(
                    "App rule {} sets manage = false, so its workspace, floating, position, size, and focus effects are ignored",
                    index
                ));
            }

            if let Some(ref title_sub) = rule.title_substring {
                if title_sub.is_empty() {
                    issues.push(format!("App rule {} has empty title_substring", index));
                } else if !seen_title_substrings.insert(title_sub) {
                    issues.push(format!(
                        "Duplicate title_substring '{}' in rule {}",
                        title_sub, index
                    ));
                }
            }

            if let Some(ref ax_role) = rule.ax_role {
                if ax_role.is_empty() {
                    issues.push(format!("App rule {} has empty ax_role", index));
                } else if !seen_ax_roles.insert(ax_role) {
                    issues.push(format!("Duplicate ax_role '{}' in rule {}", ax_role, index));
                }
            }

            if let Some(ref ax_sub) = rule.ax_subrole {
                if ax_sub.is_empty() {
                    issues.push(format!("App rule {} has empty ax_subrole", index));
                } else if !seen_ax_subroles.insert(ax_sub) {
                    issues.push(format!("Duplicate ax_subrole '{}' in rule {}", ax_sub, index));
                }
            }
        }

        issues
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    settings: Settings,
    keys: HashMap<String, WmCommand>,
    #[serde(default)]
    virtual_workspaces: VirtualWorkspaceSettings,
    /// Modifier combinations that can be reused in key bindings
    /// e.g., "comb1" = "Alt + Shift" allows using "comb1 + C" in keys
    #[serde(default)]
    modifier_combinations: HashMap<String, String>,
}

fn migrate_legacy_resize_bindings(document: &mut toml::Value) -> bool {
    let Some(keys) = document.get_mut("keys").and_then(toml::Value::as_table_mut) else {
        return false;
    };

    let mut migrated = false;
    for (_, command) in keys.iter_mut() {
        let legacy_name = match command.as_str() {
            Some("resize_window_grow") => "resize_window_grow",
            Some("resize_window_shrink") => "resize_window_shrink",
            _ => continue,
        };
        *command = toml::Value::Table(toml::map::Map::from_iter([(
            legacy_name.to_string(),
            toml::Value::String("horizontal".to_string()),
        )]));
        migrated = true;
    }
    migrated
}

fn parse_config_file(buf: &str) -> Result<ConfigFile, toml::de::Error> {
    toml::from_str(buf).or_else(|original_error| {
        let Ok(mut document) = toml::from_str::<toml::Value>(buf) else {
            return Err(original_error);
        };
        if !migrate_legacy_resize_bindings(&mut document) {
            return Err(original_error);
        }
        document.try_into()
    })
}

#[derive(Serialize, Clone, Debug)]
pub struct Config {
    pub settings: Settings,
    pub keys: Vec<(Hotkey, WmCommand)>,
    #[serde(default)]
    pub key_specs: Vec<(String, WmCommand)>,
    pub virtual_workspaces: VirtualWorkspaceSettings,
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D>(deserializer: D) -> Result<Config, D::Error>
    where D: serde::Deserializer<'de> {
        #[derive(Deserialize)]
        struct ConfigSerde {
            settings: Settings,
            keys: Vec<(Hotkey, WmCommand)>,
            #[serde(default)]
            key_specs: Vec<(String, WmCommand)>,
            virtual_workspaces: VirtualWorkspaceSettings,
        }

        let config = ConfigSerde::deserialize(deserializer)?;
        let key_specs = if config.key_specs.is_empty() && !config.keys.is_empty() {
            config
                .keys
                .iter()
                .map(|(hotkey, command)| (hotkey.to_string(), command.clone()))
                .collect()
        } else {
            config.key_specs
        };

        Ok(Config {
            settings: config.settings,
            keys: config.keys,
            key_specs,
            virtual_workspaces: config.virtual_workspaces,
        })
    }
}

unsafe impl Send for Config {}
unsafe impl Sync for Config {}

/// What happens to a display's windows while that display is disconnected.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum DisplacedWindows {
    /// They stay on their own desktops. Where every window and desktop was
    /// is recorded when the display leaves; the survivor's merged windows
    /// get a desktop made for them meanwhile; and when the display returns
    /// the scripting addition puts everything back where the record has it
    /// in one pass. Windows and desktops the user moved or made meanwhile
    /// stay as they were left, and so does a desktop they rearranged;
    /// `RestoreDepartureLayout` puts one back on demand. See
    /// `actor::reactor::display_record`.
    #[default]
    Spaces,
    /// They float on the surviving display, leaving its layout as it was. A
    /// layout that lost its screen does not get to reshape one that did not.
    Float,
    /// They are tiled into the surviving display's tree as one cluster.
    Tile,
}

/// Which macOS desktop `space create` and `space destroy` act on.
///
/// `CGSGetActiveSpace` answers with the desktop of whichever display owns the
/// menu bar, so a desktop shown on a display that holds no focus is invisible
/// to it. Switching the other display to an empty desktop takes no focus —
/// there is no window there to take it — and a destroy aimed at the empty
/// desktop in front of you then takes the one you were working in instead.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum SpaceCommandTarget {
    /// The desktop shown on the display under the pointer, falling back to the
    /// focused desktop when the pointer is not over a display rift manages.
    ///
    /// You reach a desktop on another display either by gesturing on that
    /// display or through a rift command that warps the pointer there, so the
    /// pointer is on the display you mean; and when you do mean the desktop
    /// holding the focused window, the pointer is almost always on that
    /// display too, because that is where you are working.
    #[default]
    Pointer,
    /// The focused desktop, whatever the pointer is over.
    Focus,
}

/// How rift changes the active macOS space.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum SpaceSwitchMethod {
    /// Use the scripting addition when it is loaded, and the gesture
    /// otherwise.
    ///
    /// The addition answers each switch with whether it issued it, and the
    /// gesture runs only on a refusal — the addition not loaded, or Dock not
    /// knowing the space — so the two never both act on one switch.
    Auto,
    /// Post a synthetic dock swipe at a velocity that makes the window server
    /// skip its slide. Needs no elevated privileges, and is the only option on
    /// a machine without the scripting addition. Because it drives the
    /// Dock's real swipe machinery, a single frame of movement is visible.
    #[default]
    Gesture,
    /// Ask the scripting addition to switch, which teleports with no
    /// animation at all. Requires the addition (and so the SIP configuration it
    /// needs); falls back to `Gesture` when it is not loaded.
    ScriptingAddition,
}

/// The timing of the animation that finishes a trackpad space switch.
///
/// After the fingers lift from a swipe between macOS spaces, Dock slides the
/// rest of the way with a velocity spring of its own. With this enabled, the
/// scripting addition replaces that spring with a fixed duration and a curve,
/// while Dock keeps tracking the fingers, rendering, and committing the switch.
/// It is applied at startup and on every config reload; `rift sa status` says
/// whether the addition found the routine in this Dock (`space switch step`).
///
/// ```toml
/// [settings.space_switch_animation]
/// enabled = true
/// duration_ms = 200
/// easing = "ease-out"        # or a cubic bezier: [0.25, 0.1, 0.25, 1.0]
/// ```
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct SpaceSwitchAnimationSettings {
    /// Replace Dock's spring with the duration and curve below. Needs the
    /// scripting addition.
    #[serde(default = "no")]
    pub enabled: bool,
    /// How long the slide takes once the fingers lift, whatever the distance
    /// left to cover.
    #[serde(default = "default_space_switch_duration_ms")]
    pub duration_ms: u64,
    /// The curve of that slide. See `Easing`.
    #[serde(default)]
    pub easing: Easing,
}

fn default_space_switch_duration_ms() -> u64 { 250 }

impl Default for SpaceSwitchAnimationSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            duration_ms: default_space_switch_duration_ms(),
            easing: Easing::default(),
        }
    }
}

/// A timing curve: one of the CSS names, or a cubic bezier given as its two
/// control points `[x1, y1, x2, y2]`, the way `cubic-bezier()` takes them.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(untagged)]
pub enum Easing {
    Named(EasingName),
    Bezier([f64; 4]),
}

impl Default for Easing {
    fn default() -> Self { Easing::Named(EasingName::default()) }
}

impl Easing {
    /// The control points, with the names resolved to their CSS definitions.
    pub fn bezier(self) -> [f64; 4] {
        match self {
            Easing::Bezier(points) => points,
            Easing::Named(name) => match name {
                EasingName::Linear => [0.0, 0.0, 1.0, 1.0],
                EasingName::Ease => [0.25, 0.1, 0.25, 1.0],
                EasingName::EaseIn => [0.42, 0.0, 1.0, 1.0],
                EasingName::EaseOut => [0.0, 0.0, 0.58, 1.0],
                EasingName::EaseInOut => [0.42, 0.0, 0.58, 1.0],
                EasingName::AppleDefault => [0.25, 0.1, 0.25, 1.0],
            },
        }
    }
}

/// The CSS timing-function names, plus Core Animation's default.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Default)]
#[serde(rename_all = "kebab-case")]
pub enum EasingName {
    Linear,
    Ease,
    EaseIn,
    #[default]
    EaseOut,
    EaseInOut,
    /// `kCAMediaTimingFunctionDefault`, the curve Core Animation uses when
    /// nothing else is asked for. The same points as `ease`, under the name
    /// people look for.
    AppleDefault,
}

/// What a modifier-held drag of a mouse button does.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum MouseAction {
    /// Leave the button alone; the drag reaches the application as usual.
    #[default]
    None,
    /// Move the window under the cursor.
    Move,
    /// Resize the window under the cursor from its nearest corner.
    Resize,
}

/// A modifier-only spec, e.g. "Alt" or "Ctrl + Alt".
///
/// Deserialized from a string rather than reusing `Modifiers` directly, whose
/// derived impl reads the raw bitfield integer.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct MouseModifier(pub Modifiers);

/// Written the way it is read — as the modifier's name — so a recorded
/// config (a trace header) deserializes again.
impl Serialize for MouseModifier {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: serde::Serializer {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for MouseModifier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: serde::Deserializer<'de> {
        let raw = String::deserialize(deserializer)?;
        crate::sys::hotkey::modifiers_from_str(&raw)
            .map(MouseModifier)
            .map_err(serde::de::Error::custom)
    }
}

/// yabai's `mouse_modifier` / `mouse_action1` / `mouse_action2`, which have no
/// equivalent in a stock rift: dragging a window there means grabbing its title
/// bar, and resizing means hitting its edge.
///
/// Both actions apply to floating windows. A tiled window's geometry belongs to
/// its layout, so a modifier-drag on one is ignored rather than fought with.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Default)]
#[serde(deny_unknown_fields)]
pub struct MouseSettings {
    /// Modifier that turns a drag into a move or resize. Omit to disable
    /// modifier-dragging entirely, which is the default.
    #[serde(default)]
    pub modifier: Option<MouseModifier>,
    /// What a modifier + left-button drag does.
    #[serde(default)]
    pub action1: MouseAction,
    /// What a modifier + right-button drag does.
    #[serde(default)]
    pub action2: MouseAction,
    /// Take over plain drags that grab a floating window by its top strip
    /// (title bar / tab bar): the app never sees the drag, rift moves the
    /// window. Stops apps that animate their own drags (Warp's tab bar)
    /// from fighting macOS over the display seam — the vibrating,
    /// snapping-back drag. Opt-in: it also swallows everything else a strip
    /// drag can mean (tearing a tab out into its own window, drag-to-tile,
    /// drag-to-menu-bar), which no heuristic can tell apart from a move.
    #[serde(default)]
    pub takeover_float_drags: bool,
}

impl MouseSettings {
    /// The action for a button, or `None` when modifier-dragging is off.
    pub fn action_for(&self, button: MouseButton) -> Option<(Modifiers, MouseAction)> {
        let modifier = self.modifier?.0;
        let action = match button {
            MouseButton::Left => self.action1,
            MouseButton::Right => self.action2,
        };
        (action != MouseAction::None).then_some((modifier, action))
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MouseButton {
    Left,
    Right,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default = "no")]
    pub animate: bool,
    #[serde(default = "default_animation_duration")]
    pub animation_duration: f64,
    #[serde(default = "default_animation_fps")]
    pub animation_fps: f64,
    #[serde(default)]
    pub animation_easing: AnimationEasing,
    #[serde(default = "yes")]
    pub default_disable: bool,
    #[serde(default = "yes")]
    pub mouse_follows_focus: bool,
    #[serde(default = "yes")]
    pub mouse_hides_on_focus: bool,
    #[serde(default = "yes")]
    pub focus_follows_mouse: bool,
    /// Hotkey that disables focus-follows-mouse while held.
    /// Accepts either a full hotkey (e.g. "Ctrl + A") or a modifier-only spec (e.g. "Ctrl")
    #[serde(default)]
    pub focus_follows_mouse_disable_hotkey: Option<HotkeySpec>,
    /// Apps that should not trigger automatic workspace switching when activated.
    /// List of bundle identifiers (e.g., "com.apple.Spotlight") that often
    /// inappropriately steal focus and shouldn't cause workspace switches.
    #[serde(default)]
    pub auto_focus_blacklist: Vec<String>,
    /// Apps the mouse should never be warped onto when `mouse_follows_focus` is
    /// on. List of bundle identifiers. Screenshot and overlay tools are the
    /// usual case: they take focus for a transient, often invisible window, and
    /// dragging the cursor onto it interrupts whatever the user was pointing at.
    /// Focus itself is unaffected — only the warp is suppressed.
    #[serde(default)]
    pub mouse_follows_focus_blacklist: Vec<String>,
    /// How to change the active macOS space. See `SpaceSwitchMethod`.
    #[serde(default)]
    pub space_switch_method: SpaceSwitchMethod,
    /// Which desktop `space create` and `space destroy` act on. See
    /// `SpaceCommandTarget`.
    #[serde(default)]
    pub space_target: SpaceCommandTarget,
    /// The timing of the trackpad space switch. See
    /// `SpaceSwitchAnimationSettings`.
    #[serde(default)]
    pub space_switch_animation: SpaceSwitchAnimationSettings,
    /// Remember a display's layout when it disconnects (unplug, sleep, lid
    /// close) and put it back when the same display returns, moving its
    /// windows home through the scripting addition. macOS hands a returning
    /// display a brand-new space, so without this the old layout is orphaned
    /// and every window is left piled on whichever display survived.
    #[serde(default = "yes")]
    pub restore_display_layouts: bool,
    /// Where a disconnected display's windows go in the meantime. See
    /// `DisplacedWindows`.
    #[serde(default)]
    pub displaced_windows: DisplacedWindows,
    /// Modifier-drag: hold a modifier and drag anywhere in a window to move or
    /// resize it, instead of aiming for its title bar or edges.
    #[serde(default)]
    pub mouse: MouseSettings,
    #[serde(default)]
    pub layout: LayoutSettings,
    #[serde(default)]
    pub ui: UiSettings,
    /// Trackpad gesture settings
    #[serde(default)]
    pub gestures: GestureSettings,

    #[serde(default)]
    pub window_snapping: WindowSnappingSettings,

    /// Commands to run on startup (e.g., for subscribing to events)
    #[serde(default)]
    pub run_on_start: Vec<String>,

    /// Whether to reapply app rules when a window title changes.
    /// Enable hot-reloading of the config file when it changes
    #[serde(default = "yes")]
    pub hot_reload: bool,

    /// Carrying the layout across a restart. See `LayoutRestoreSettings`.
    #[serde(default)]
    pub layout_restore: LayoutRestoreSettings,
}

fn default_restore_max_age_secs() -> u64 { 120 }

fn default_autosave_secs() -> u64 { 60 }

/// Putting the layout back after rift restarts.
///
/// rift has always been able to save its layout and start from a saved one, but
/// only by hand: nothing wrote the file on the way out and nothing read it on
/// the way in. Under a float-by-default config that means re-tiling everything
/// after each restart.
///
/// Putting the *windows* back is deliberately not unconditional. Where each
/// window sat is only worth reasserting if rift is coming straight back up — a
/// crash respawn, a `brew services restart`, a dev rebuild. After a reboot or
/// an afternoon away the windows have moved on without it, and putting them
/// back would fight the user rather than help. `max_age_secs` is how long that
/// stays worth doing; `autosave_secs` keeps the file's timestamp tracking the
/// last moment rift was known to be alive, so the age measures downtime.
///
/// The desktops themselves have no such shelf life. Which layout a desktop is
/// in, and what workspaces it has, is a setting the user chose; it is as true
/// after a reboot as before one, so it is restored however old the snapshot is.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct LayoutRestoreSettings {
    /// Restore `~/.rift/layout.ron` at startup, if it is fresh enough. The
    /// `--restore` flag forces this on for one run regardless.
    #[serde(default = "no")]
    pub on_start: bool,
    /// How old the snapshot's *window positions* may be and still be put back,
    /// in seconds. Measured from when the file was last written. 0 puts back
    /// whatever is there. Past it the desktops still come back in the layouts
    /// they were in — that is a setting, not a snapshot of a moment — and only
    /// the windows are left where they now are.
    #[serde(default = "default_restore_max_age_secs")]
    pub max_age_secs: u64,
    /// Save the layout every this many seconds. 0 disables it, leaving only
    /// the save on shutdown — which a crash or a SIGKILL does not reach.
    #[serde(default = "default_autosave_secs")]
    pub autosave_secs: u64,
}

impl Default for LayoutRestoreSettings {
    fn default() -> Self {
        Self {
            on_start: false,
            max_age_secs: default_restore_max_age_secs(),
            autosave_secs: default_autosave_secs(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct UiSettings {
    #[serde(default)]
    pub menu_bar: MenuBarSettings,
    #[serde(default)]
    pub stack_line: StackLineSettings,
    #[serde(default)]
    pub mission_control: MissionControlSettings,
    #[serde(default)]
    pub drop_overlay: DropOverlaySettings,
    #[serde(default)]
    pub tile_halo: TileHaloSettings,
}

/// The region shown while dragging a window, marking where it would land.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct DropOverlaySettings {
    #[serde(default = "no")]
    pub enabled: bool,
    /// Corner rounding, in points.
    #[serde(default = "default_drop_overlay_corner_radius")]
    pub corner_radius: f64,
    /// Use the clearer of the two Liquid Glass styles.
    #[serde(default = "no")]
    pub clear_style: bool,
    /// How far the region travels toward a new target each frame, 0..1.
    /// Higher is snappier; 1.0 removes the motion and snaps instead.
    #[serde(default = "default_drop_overlay_follow_rate")]
    pub follow_rate: f64,
}

fn default_drop_overlay_corner_radius() -> f64 { 10.0 }
fn default_drop_overlay_follow_rate() -> f64 { 0.35 }

impl Default for DropOverlaySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            corner_radius: default_drop_overlay_corner_radius(),
            clear_style: false,
            follow_rate: default_drop_overlay_follow_rate(),
        }
    }
}

/// The focus ring flashed over a window that the float toggle has just pulled
/// into the tiling tree, or pushed back out of it.
///
/// It exists for the case where the transition leaves no trace on screen: a
/// window whose own size already matched the frame the layout hands it does
/// not move, and one that lands in a stack covers its neighbours exactly.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct TileHaloSettings {
    #[serde(default = "no")]
    pub enabled: bool,
    /// Also flash, in `float_color` and drifting outward rather than springing
    /// in, when the toggle takes a window back out of the tree.
    #[serde(default = "yes")]
    pub on_float: bool,
    /// How far outside the frame the ring starts before it springs on, in
    /// points. 0 draws it in place and only fades it, which is also what
    /// macOS's Reduce Motion setting forces regardless of this value.
    #[serde(default = "default_tile_halo_grow")]
    pub grow: f64,
    /// Stroke width, in points.
    #[serde(default = "default_tile_halo_thickness")]
    pub thickness: f64,
    /// Corner rounding of the outermost ring, in points. Match it to the
    /// window corners of the macOS you are on; the inner rings derive theirs
    /// from this one so the curves stay concentric.
    #[serde(default = "default_tile_halo_corner_radius")]
    pub corner_radius: f64,
    /// The whole flash, in milliseconds. The last 40% of it is the fade.
    #[serde(default = "default_tile_halo_duration_ms")]
    pub duration_ms: f64,
    /// Colour of the ring when a window joins the tree, as `[r, g, b]` in
    /// 0..1. Left out, it takes the accent colour the user chose in System
    /// Settings, which is what makes it look like part of macOS.
    #[serde(default)]
    pub tile_color: Option<[f64; 3]>,
    /// Colour of the ring when a window leaves the tree. Left out, it is a
    /// neutral grey — macOS's register for "selected, but not the focus".
    #[serde(default)]
    pub float_color: Option<[f64; 3]>,
    /// Draw a fainter rim inside the ring for each window sharing the stack a
    /// window landed in, up to two. That landing is the one where nothing on
    /// screen moves at all, so the depth is the only thing distinguishing it
    /// from an ordinary tile.
    #[serde(default = "yes")]
    pub stack_depth: bool,
}

fn default_tile_halo_grow() -> f64 { 14.0 }
fn default_tile_halo_thickness() -> f64 { 3.5 }
fn default_tile_halo_corner_radius() -> f64 { 14.0 }
fn default_tile_halo_duration_ms() -> f64 { 420.0 }

impl Default for TileHaloSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            on_float: true,
            grow: default_tile_halo_grow(),
            thickness: default_tile_halo_thickness(),
            corner_radius: default_tile_halo_corner_radius(),
            duration_ms: default_tile_halo_duration_ms(),
            tile_color: None,
            float_color: None,
            stack_depth: true,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct GestureSettings {
    /// Enable horizontal swipes to switch virtual workspaces
    #[serde(default = "no")]
    pub enabled: bool,
    /// If true, consume horizontal swipe events owned by Rift so macOS and the
    /// foreground app do not also handle them.
    #[serde(default = "yes")]
    pub consume_dock_swipe: bool,
    /// Invert horizontal direction (swap next/prev)
    #[serde(default)]
    pub invert_horizontal_swipe: bool,
    /// Maximum absolute Y delta allowed for the gesture to count as horizontal
    #[serde(default = "default_swipe_vertical_tolerance")]
    pub swipe_vertical_tolerance: f64,
    /// If true, attempt to skip empty workspaces on swipe (if supported)
    #[serde(default)]
    pub skip_empty: bool,
    /// Number of fingers required for swipe (default = 3)
    #[serde(default = "default_swipe_fingers")]
    pub fingers: usize,
    /// Normalized horizontal distance (0..1) required to fire a swipe
    #[serde(default = "default_distance_pct")]
    pub distance_pct: f64,
    /// Enable haptic feedback when a swipe commits
    #[serde(default = "yes")]
    pub haptics_enabled: bool,
    /// Haptic feedback pattern (generic | alignment | level_change)
    #[serde(default)]
    pub haptic_pattern: HapticPattern,
}

impl Default for GestureSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            consume_dock_swipe: true,
            invert_horizontal_swipe: false,
            swipe_vertical_tolerance: default_swipe_vertical_tolerance(),
            skip_empty: true,
            fingers: default_swipe_fingers(),
            distance_pct: default_distance_pct(),
            haptics_enabled: true,
            haptic_pattern: HapticPattern::LevelChange,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default, Copy)]
#[serde(deny_unknown_fields)]
pub struct WindowSnappingSettings {
    #[serde(default = "default_drag_swap_fraction")]
    pub drag_swap_fraction: f64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum MenuBarDisplayMode {
    #[default]
    All,
    Active,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActiveWorkspaceLabel {
    #[default]
    Index,
    Name,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceDisplayStyle {
    #[default]
    Layout,
    Label,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct MenuBarSettings {
    #[serde(default = "no")]
    pub enabled: bool,
    #[serde(default = "no")]
    pub show_empty: bool,
    #[serde(default)]
    pub mode: MenuBarDisplayMode,
    #[serde(default)]
    pub active_label: ActiveWorkspaceLabel,
    #[serde(default)]
    pub display_style: WorkspaceDisplayStyle,
    #[serde(default = "default_layout_folder")]
    pub layout_folder: PathBuf,
}

impl MenuBarSettings {
    pub fn resolved_layout_folder(&self) -> PathBuf {
        let Ok(relative) = self.layout_folder.strip_prefix("~") else {
            return self.layout_folder.clone();
        };
        dirs::home_dir()
            .map(|home| home.join(relative))
            .unwrap_or_else(|| self.layout_folder.clone())
    }
}

impl Default for MenuBarSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            show_empty: false,
            mode: MenuBarDisplayMode::default(),
            active_label: ActiveWorkspaceLabel::default(),
            display_style: WorkspaceDisplayStyle::default(),
            layout_folder: default_layout_folder(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct StackLineSettings {
    #[serde(default = "no")]
    pub enabled: bool,
    #[serde(default)]
    pub hover: StackLineHoverMode,
    #[serde(default = "default_stack_line_thickness")]
    pub thickness: f64,
    #[serde(default)]
    pub horiz_placement: HorizontalPlacement,
    #[serde(default)]
    pub vert_placement: VerticalPlacement,
    /// Distance to position the stack line away from the window edge (in points)
    /// This creates spacing between the window and the stack line
    #[serde(default = "default_stack_line_spacing")]
    pub spacing: f64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum StackLineHoverMode {
    Click,
    #[default]
    Hover,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct MissionControlSettings {
    #[serde(default = "no")]
    pub enabled: bool,
    #[serde(default = "no")]
    pub fade_enabled: bool,
    #[serde(default = "default_mission_control_fade_duration_ms")]
    pub fade_duration_ms: f64,
}

fn default_mission_control_fade_duration_ms() -> f64 { 180.0 }

fn default_drag_swap_fraction() -> f64 { 0.3 }

fn default_master_stack_ratio() -> f64 { 0.6 }

fn default_master_stack_count() -> usize { 1 }

fn default_scrolling_column_width_ratio() -> f64 { 0.7 }

fn default_scrolling_min_column_width_ratio() -> f64 { 0.3 }

fn default_scrolling_max_column_width_ratio() -> f64 { 0.9 }

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum HorizontalPlacement {
    #[default]
    Top,
    Bottom,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum VerticalPlacement {
    #[default]
    Left,
    Right,
}

impl StackLineSettings {
    pub fn thickness(&self) -> f64 { if self.enabled { self.thickness } else { 0.0 } }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum WindowInsertionPoint {
    /// Insert a new window immediately after the current selection.
    #[default]
    NextToSelection,
    /// Append a new window at the end of the layout tree.
    EndOfTree,
}

/// Options understood by every layout system.
///
/// These fields are flattened into both `[settings.layout]` and every
/// per-layout table. A per-layout value overrides the layout-wide value.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct BaseLayoutSettings {
    /// Where newly managed windows are inserted.
    #[serde(default)]
    pub window_insertion_point: Option<WindowInsertionPoint>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct TraditionalLayoutSettings {
    #[serde(flatten)]
    pub base: BaseLayoutSettings,
    /// Use Sway-style sibling normalization when inserting nodes. New nodes receive the
    /// average sibling weight instead of splitting the selected node's share.
    #[serde(default = "yes")]
    pub equalize_nodes: bool,
}

impl Default for TraditionalLayoutSettings {
    fn default() -> Self {
        Self {
            base: BaseLayoutSettings::default(),
            equalize_nodes: true,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct BspLayoutSettings {
    #[serde(flatten)]
    pub base: BaseLayoutSettings,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct LayoutSettings {
    /// Settings inherited by every layout type unless overridden by its table.
    #[serde(flatten)]
    pub base: BaseLayoutSettings,
    /// Layout mode: "traditional", "bsp", "stack", "master_stack", or "scrolling"
    #[serde(default)]
    pub mode: LayoutMode,
    /// Whether directional focus may land on a floating window.
    ///
    /// Off by default, matching upstream: when the focused window floats,
    /// left/right/up/down move between floating windows. Set true for yabai's
    /// behaviour, where directional focus only ever walks the tiling tree and
    /// floating windows are reached another way.
    #[serde(default = "no")]
    pub directional_focus_skips_floating: bool,
    /// Whether directional focus and movement continue onto the neighbouring
    /// display when they reach the edge of the tree.
    ///
    /// On by default, matching upstream. yabai resolved a direction inside a
    /// single view, so focusing or swapping simply stopped at the edge and a
    /// window changed display only when told to; set false for that.
    #[serde(default = "yes")]
    pub move_across_displays: bool,
    /// Traditional layout configuration
    #[serde(default)]
    pub traditional: TraditionalLayoutSettings,
    /// BSP layout configuration
    #[serde(default)]
    pub bsp: BspLayoutSettings,
    /// Stack system configuration
    #[serde(default)]
    pub stack: StackSettings,
    /// Master/stack layout configuration
    #[serde(default)]
    pub master_stack: MasterStackSettings,
    /// Gap configuration for window spacing
    #[serde(default)]
    pub gaps: GapSettings,
    /// Scrolling layout configuration (niri-style columns)
    #[serde(default)]
    pub scrolling: ScrollingLayoutSettings,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct ScrollingLayoutSettings {
    #[serde(flatten)]
    pub base: BaseLayoutSettings,
    /// Whether to animate window transitions in this layout.
    #[serde(default)]
    pub animate: Option<bool>,
    /// Default width of the active column, as a fraction of the screen width.
    #[serde(default = "default_scrolling_column_width_ratio")]
    pub column_width_ratio: f64,
    /// Minimum column width ratio allowed by resize commands.
    #[serde(default = "default_scrolling_min_column_width_ratio")]
    pub min_column_width_ratio: f64,
    /// Maximum column width ratio allowed by resize commands.
    #[serde(default = "default_scrolling_max_column_width_ratio")]
    pub max_column_width_ratio: f64,
    /// Alignment for the focused column (left, center, right).
    #[serde(default)]
    pub alignment: ScrollingAlignment,
    /// Horizontal focus navigation behavior:
    /// - niri: reveal only as needed based on navigation direction.
    /// - anchored: always align focused column to `alignment`.
    #[serde(default)]
    pub focus_navigation_style: ScrollingFocusNavigationStyle,
    /// Trackpad gestures for scrolling layout
    #[serde(default)]
    pub gestures: ScrollingGestureSettings,
}

impl Default for ScrollingLayoutSettings {
    fn default() -> Self {
        Self {
            base: BaseLayoutSettings::default(),
            animate: None,
            column_width_ratio: default_scrolling_column_width_ratio(),
            min_column_width_ratio: default_scrolling_min_column_width_ratio(),
            max_column_width_ratio: default_scrolling_max_column_width_ratio(),
            alignment: ScrollingAlignment::default(),
            focus_navigation_style: ScrollingFocusNavigationStyle::default(),
            gestures: ScrollingGestureSettings::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum MasterStackSide {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScrollingAlignment {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScrollingFocusNavigationStyle {
    #[default]
    Niri,
    Anchored,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct MasterStackSettings {
    #[serde(flatten)]
    pub base: BaseLayoutSettings,
    /// Fraction of space reserved for the master area (0.05..0.95)
    #[serde(default = "default_master_stack_ratio")]
    pub master_ratio: f64,
    /// Number of windows kept in the master area (>= 1)
    #[serde(default = "default_master_stack_count")]
    pub master_count: usize,
    /// Which side the master area occupies
    #[serde(default)]
    pub master_side: MasterStackSide,
    /// Where new windows are inserted when the master area is already full
    #[serde(default = "default_master_stack_new_window_placement")]
    pub new_window_placement: MasterStackNewWindowPlacement,
    /// Orientation arrangement for the master area (override default derived from master_side)
    #[serde(default)]
    pub master_arrangement: Option<crate::layout_engine::Orientation>,
    /// Orientation arrangement for the stack area (override default derived from master_side)
    #[serde(default)]
    pub stack_arrangement: Option<crate::layout_engine::Orientation>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum MasterStackNewWindowPlacement {
    Master,
    Stack,
    Focused,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub struct ScrollingGestureSettings {
    /// Enable horizontal scroll gestures to switch columns
    #[serde(default = "no")]
    pub enabled: bool,
    /// Invert horizontal direction (swap left/right)
    #[serde(default)]
    pub invert_horizontal: bool,
    /// Maximum absolute Y delta allowed for the gesture to count as horizontal
    #[serde(default = "default_swipe_vertical_tolerance")]
    pub vertical_tolerance: f64,
    /// Number of fingers required for scroll gesture
    #[serde(default = "default_swipe_fingers")]
    pub fingers: usize,
    /// Normalized horizontal distance (0..1) required to fire a scroll step
    #[serde(default = "default_distance_pct")]
    pub distance_pct: f64,
    /// If true, scrolling past the end of the strip will trigger a workspace switch
    #[serde(default = "no")]
    pub propagate_to_workspace_swipe: bool,
    /// Amount of overscroll (in steps) required to trigger a workspace switch
    #[serde(default = "default_overscroll_threshold")]
    pub workspace_switch_threshold: f64,
}

impl Default for ScrollingGestureSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            invert_horizontal: false,
            vertical_tolerance: default_swipe_vertical_tolerance(),
            fingers: default_swipe_fingers(),
            distance_pct: default_distance_pct(),
            propagate_to_workspace_swipe: false,
            workspace_switch_threshold: default_overscroll_threshold(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum StackDefaultOrientation {
    Perpendicular,
    Same,
    Horizontal,
    Vertical,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct StackSettings {
    #[serde(flatten)]
    pub base: BaseLayoutSettings,
    /// Stack offset - how much each stacked window is offset (in pixels)
    /// With the enhanced stacking system, this creates meaningful visible edges
    /// for each window in the stack while the focused window remains fully visible.
    /// Recommended values: 30-50 pixels for good visibility.
    #[serde(default = "default_stack_offset")]
    pub stack_offset: f64,

    /// Default orientation behavior when stacking windows.
    /// Options:
    /// - "perpendicular" (default): choose the perpendicular orientation to the parent layout
    /// - "same": use the same orientation as the parent layout
    /// - "horizontal"/"vertical": explicitly use a specific orientation
    #[serde(default = "default_stack_orientation")]
    pub default_orientation: StackDefaultOrientation,
}

/// Gap configuration for window spacing
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct GapSettings {
    /// Outer gaps (space between windows and screen edges)
    #[serde(default)]
    pub outer: OuterGaps,
    /// Inner gaps (space between windows)
    #[serde(default)]
    pub inner: InnerGaps,
    /// Display-specific gap overrides keyed by display UUID
    #[serde(default)]
    pub per_display: HashMap<String, GapOverride>,
}

/// Outer gap configuration (space between windows and screen edges)
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct OuterGaps {
    /// Gap at the top of the screen
    #[serde(default)]
    pub top: f64,
    /// Gap at the left of the screen
    #[serde(default)]
    pub left: f64,
    /// Gap at the bottom of the screen
    #[serde(default)]
    pub bottom: f64,
    /// Gap at the right of the screen
    #[serde(default)]
    pub right: f64,
}

/// Inner gap configuration (space between windows)
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct InnerGaps {
    /// Horizontal gap between windows
    #[serde(default)]
    pub horizontal: f64,
    /// Vertical gap between windows
    #[serde(default)]
    pub vertical: f64,
}

/// Overrides for gaps on a per-display basis
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct GapOverride {
    /// Override outer gaps completely for the display
    #[serde(default)]
    pub outer: Option<OuterGaps>,
    /// Override inner gaps completely for the display
    #[serde(default)]
    pub inner: Option<InnerGaps>,
}

impl Default for StackSettings {
    fn default() -> Self {
        Self {
            base: BaseLayoutSettings::default(),
            stack_offset: default_stack_offset(),
            default_orientation: default_stack_orientation(),
        }
    }
}

impl Default for MasterStackSettings {
    fn default() -> Self {
        Self {
            base: BaseLayoutSettings::default(),
            master_ratio: default_master_stack_ratio(),
            master_count: default_master_stack_count(),
            master_side: MasterStackSide::Left,
            new_window_placement: default_master_stack_new_window_placement(),
            master_arrangement: None,
            stack_arrangement: None,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        if self.animation_duration < 0.0 {
            issues.push(format!(
                "animation_duration must be non-negative, got {}",
                self.animation_duration
            ));
        }

        if self.animation_fps <= 0.0 {
            issues.push(format!(
                "animation_fps must be positive, got {}",
                self.animation_fps
            ));
        }

        issues.extend(self.layout.validate());

        if self.space_switch_animation.enabled && self.space_switch_animation.duration_ms == 0 {
            issues.push(
                "space_switch_animation.duration_ms must be positive when enabled (0 would \
                 hand the animation back to Dock)"
                    .to_string(),
            );
        }
        if let Easing::Bezier([x1, _, x2, _]) = self.space_switch_animation.easing
            && !((0.0..=1.0).contains(&x1) && (0.0..=1.0).contains(&x2))
        {
            issues.push(format!(
                "space_switch_animation.easing control points need x1 and x2 within 0..=1, got \
                 {x1} and {x2}"
            ));
        }

        if self.gestures.swipe_vertical_tolerance < 0.0 {
            issues.push(format!(
                "gestures.swipe_vertical_tolerance must be non-negative, got {}",
                self.gestures.swipe_vertical_tolerance
            ));
        }

        issues
    }
}

impl LayoutSettings {
    pub fn base_for(&self, mode: LayoutMode) -> &BaseLayoutSettings {
        match mode {
            LayoutMode::Traditional => &self.traditional.base,
            LayoutMode::Bsp => &self.bsp.base,
            LayoutMode::Stack => &self.stack.base,
            LayoutMode::MasterStack => &self.master_stack.base,
            LayoutMode::Scrolling => &self.scrolling.base,
        }
    }

    pub fn window_insertion_point_for(&self, mode: LayoutMode) -> WindowInsertionPoint {
        self.base_for(mode)
            .window_insertion_point
            .or(self.base.window_insertion_point)
            .unwrap_or_default()
    }

    pub fn resolved_base_for(&self, mode: LayoutMode) -> BaseLayoutSettings {
        BaseLayoutSettings {
            window_insertion_point: Some(self.window_insertion_point_for(mode)),
        }
    }

    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        issues.extend(self.stack.validate());

        issues.extend(self.master_stack.validate());

        issues.extend(self.gaps.validate());

        issues.extend(self.scrolling.validate());

        issues
    }
}

impl ScrollingLayoutSettings {
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        if !(0.0..=1.0).contains(&self.column_width_ratio) {
            issues.push(format!(
                "layout.scrolling.column_width_ratio must be between 0.0 and 1.0, got {}",
                self.column_width_ratio
            ));
        }

        if !(0.0..=1.0).contains(&self.min_column_width_ratio) {
            issues.push(format!(
                "layout.scrolling.min_column_width_ratio must be between 0.0 and 1.0, got {}",
                self.min_column_width_ratio
            ));
        }

        if !(0.0..=1.0).contains(&self.max_column_width_ratio) {
            issues.push(format!(
                "layout.scrolling.max_column_width_ratio must be between 0.0 and 1.0, got {}",
                self.max_column_width_ratio
            ));
        }

        if self.min_column_width_ratio > self.max_column_width_ratio {
            issues.push(format!(
                "layout.scrolling.min_column_width_ratio ({}) must be <= max_column_width_ratio ({})",
                self.min_column_width_ratio, self.max_column_width_ratio
            ));
        }

        if !(self.min_column_width_ratio..=self.max_column_width_ratio)
            .contains(&self.column_width_ratio)
        {
            issues.push(format!(
                "layout.scrolling.column_width_ratio ({}) must be within min/max bounds",
                self.column_width_ratio
            ));
        }

        if self.gestures.vertical_tolerance < 0.0 {
            issues.push(format!(
                "layout.scrolling.gestures.vertical_tolerance must be non-negative, got {}",
                self.gestures.vertical_tolerance
            ));
        }

        issues
    }
}

impl StackSettings {
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        if self.stack_offset < 0.0 {
            issues.push(format!(
                "stack_offset must be non-negative, got {}",
                self.stack_offset
            ));
        }

        issues
    }
}

impl MasterStackSettings {
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        if !(0.05..=0.95).contains(&self.master_ratio) {
            issues.push(format!(
                "master_stack.master_ratio must be between 0.05 and 0.95, got {}",
                self.master_ratio
            ));
        }

        if self.master_count == 0 {
            issues.push("master_stack.master_count must be at least 1".to_string());
        }

        issues
    }
}

impl GapSettings {
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        // Validate outer gaps
        issues.extend(self.outer.validate());

        // Validate inner gaps
        issues.extend(self.inner.validate());

        for (uuid, overrides) in &self.per_display {
            if let Some(outer) = &overrides.outer {
                for issue in outer.validate() {
                    issues.push(format!("per_display[{uuid}] {issue}"));
                }
            }
            if let Some(inner) = &overrides.inner {
                for issue in inner.validate() {
                    issues.push(format!("per_display[{uuid}] {issue}"));
                }
            }
        }

        issues
    }

    pub fn effective_for_display(&self, display_uuid: Option<&str>) -> GapSettings {
        let mut resolved = GapSettings {
            outer: self.outer.clone(),
            inner: self.inner.clone(),
            per_display: HashMap::default(),
        };
        if let Some(uuid) = display_uuid {
            if let Some(overrides) = self.per_display.get(uuid) {
                if let Some(outer_override) = &overrides.outer {
                    resolved.outer = outer_override.clone();
                }
                if let Some(inner_override) = &overrides.inner {
                    resolved.inner = inner_override.clone();
                }
            }
        }
        resolved
    }
}

impl OuterGaps {
    /// Validates outer gap configuration values and returns a list of issues found.
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        if self.top < 0.0 {
            issues.push(format!("outer.top gap must be non-negative, got {}", self.top));
        }

        if self.left < 0.0 {
            issues.push(format!("outer.left gap must be non-negative, got {}", self.left));
        }

        if self.bottom < 0.0 {
            issues.push(format!(
                "outer.bottom gap must be non-negative, got {}",
                self.bottom
            ));
        }

        if self.right < 0.0 {
            issues.push(format!(
                "outer.right gap must be non-negative, got {}",
                self.right
            ));
        }

        issues
    }
}

impl InnerGaps {
    /// Validates inner gap configuration values and returns a list of issues found.
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        if self.horizontal < 0.0 {
            issues.push(format!(
                "inner.horizontal gap must be non-negative, got {}",
                self.horizontal
            ));
        }

        if self.vertical < 0.0 {
            issues.push(format!(
                "inner.vertical gap must be non-negative, got {}",
                self.vertical
            ));
        }

        issues
    }
}

fn yes() -> bool { true }

fn default_stack_offset() -> f64 { 40.0 }

pub fn default_stack_orientation() -> StackDefaultOrientation {
    StackDefaultOrientation::Perpendicular
}

fn default_master_stack_new_window_placement() -> MasterStackNewWindowPlacement {
    MasterStackNewWindowPlacement::Master
}

fn default_animation_duration() -> f64 { 0.3 }

fn default_animation_fps() -> f64 { 100.0 }

#[allow(dead_code)]
pub fn no() -> bool { false }

fn default_layout_folder() -> PathBuf { PathBuf::from("~/.config/rift/layouts") }

fn default_workspace_count() -> usize { 4 }

fn default_workspace_names() -> Vec<String> {
    vec![
        "Main".to_string(),
        "Development".to_string(),
        "Communication".to_string(),
        "Utilities".to_string(),
    ]
}

// Interpreted as normalized fraction when <= 1.0. If > 1.0 and <= 100.0,
// it is treated as a percentage (e.g. 40.0 -> 0.40).
fn default_swipe_vertical_tolerance() -> f64 { 0.4 }
fn default_swipe_fingers() -> usize { 3 }
fn default_distance_pct() -> f64 { 0.08 }
fn default_overscroll_threshold() -> f64 { 0.15 }

fn default_stack_line_spacing() -> f64 { 1.0 }
fn default_stack_line_thickness() -> f64 { 20.0 }

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum HapticPattern {
    Generic,
    Alignment,
    #[default]
    LevelChange,
}

impl Config {
    pub fn read(path: &Path) -> anyhow::Result<Config> {
        let buf = std::fs::read_to_string(path)?;
        Self::parse(&buf)
    }

    pub fn default() -> Config { Self::parse(include_str!("../../rift.default.toml")).unwrap() }

    /// Save the current config to a file
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let config_file = ConfigFile {
            settings: self.settings.clone(),
            keys: self
                .key_specs
                .iter()
                .map(|(hotkey, command)| (hotkey.clone(), command.clone()))
                .collect(),
            virtual_workspaces: self.virtual_workspaces.clone(),
            modifier_combinations: HashMap::default(),
        };

        let toml_string = toml::to_string_pretty(&config_file)?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, toml_string.as_bytes())?;

        Ok(())
    }

    /// Validates the entire configuration and returns a list of issues found.
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        // Validate settings
        issues.extend(self.settings.validate());

        // Validate virtual workspace settings
        issues.extend(self.virtual_workspaces.validate());

        issues
    }

    fn normalize_hotkey_string(key: &str) -> String {
        let mut out = String::with_capacity(key.len());
        let mut word = String::new();

        for ch in key.chars() {
            if ch.is_alphabetic() {
                word.push(ch);
            } else {
                if !word.is_empty() {
                    let token = if word.len() == 1 {
                        word.to_ascii_uppercase()
                    } else {
                        match word.to_lowercase().as_str() {
                            "up" => "ArrowUp".to_string(),
                            "down" => "ArrowDown".to_string(),
                            "left" => "ArrowLeft".to_string(),
                            "right" => "ArrowRight".to_string(),
                            _ => word.clone(),
                        }
                    };
                    out.push_str(&token);
                    word.clear();
                }
                out.push(ch);
            }
        }

        if !word.is_empty() {
            let token = if word.len() == 1 {
                word.to_ascii_uppercase()
            } else {
                match word.to_lowercase().as_str() {
                    "up" => "ArrowUp".to_string(),
                    "down" => "ArrowDown".to_string(),
                    "left" => "ArrowLeft".to_string(),
                    "right" => "ArrowRight".to_string(),
                    _ => word.clone(),
                }
            };
            out.push_str(&token);
        }

        out
    }

    fn expand_modifier_combinations(key: &str, combinations: &HashMap<String, String>) -> String {
        if let Some(plus_pos) = key.find(" + ") {
            let potential_combo = &key[..plus_pos];
            if let Some(combo_value) = combinations.get(potential_combo) {
                let rest = &key[plus_pos + 3..];
                return format!("{} + {}", combo_value, rest);
            }
        }
        key.to_string()
    }

    /// no need to pull in a dep for just this
    fn levenshtein(a: &str, b: &str) -> usize {
        let a_chars: Vec<char> = a.chars().collect();
        let b_chars: Vec<char> = b.chars().collect();
        let mut d = vec![vec![0usize; b_chars.len() + 1]; a_chars.len() + 1];
        for i in 0..=a_chars.len() {
            d[i][0] = i;
        }
        for j in 0..=b_chars.len() {
            d[0][j] = j;
        }
        for i in 1..=a_chars.len() {
            for j in 1..=b_chars.len() {
                let cost = if a_chars[i - 1] == b_chars[j - 1] {
                    0
                } else {
                    1
                };
                d[i][j] = std::cmp::min(
                    std::cmp::min(d[i - 1][j] + 1, d[i][j - 1] + 1),
                    d[i - 1][j - 1] + cost,
                );
            }
        }
        d[a_chars.len()][b_chars.len()]
    }

    // Extracts an "unknown variant `...`" token from serde error string when present.
    // Additionally, if serde's error message contains an "expected" list (backtick-delimited),
    // embed those expected tokens alongside the unknown token using the separator "||".
    // The resulting returned string may therefore be:
    //   - "unknown_token" (no expected candidates found)
    //   - "unknown_token||cand1,cand2,..." (candidates appended)
    fn extract_unknown_variant(err: &str) -> Option<String> {
        let needle = "unknown variant `";
        if let Some(start) = err.find(needle) {
            let rest = &err[start + needle.len()..];
            if let Some(end) = rest.find('`') {
                let unknown = rest[..end].to_string();

                // Collect all backtick-enclosed tokens in the error message and
                // treat them as candidate variants (excluding the unknown itself).
                let mut variants: Vec<String> = Vec::new();
                let mut i = 0usize;
                while let Some(open) = err[i..].find('`') {
                    let open_abs = i + open + 1;
                    if let Some(close_off) = err[open_abs..].find('`') {
                        let close_abs = open_abs + close_off;
                        let token = &err[open_abs..close_abs];
                        if token != unknown {
                            variants.push(token.to_string());
                        }
                        i = close_abs + 1;
                    } else {
                        break;
                    }
                }

                if !variants.is_empty() {
                    // dedupe while preserving order
                    let mut seen = std::collections::HashSet::new();
                    let mut deduped = Vec::new();
                    for v in variants {
                        if seen.insert(v.clone()) {
                            deduped.push(v);
                        }
                    }
                    return Some(format!("{}||{}", unknown, deduped.join(",")));
                }

                return Some(unknown);
            }
        }

        if let Some(unknown_pos) = err.find("unknown") {
            if let Some(backtick_pos) = err[unknown_pos..].find('`') {
                let rest = &err[unknown_pos + backtick_pos + 1..];
                if let Some(end) = rest.find('`') {
                    return Some(rest[..end].to_string());
                }
            }
        }
        None
    }

    // Provide suggestion by comparing the unknown token to a list of known commands.
    // If the `unknown` string was produced by `extract_unknown_variant` and contains
    // an embedded serde candidate list (format: "token||cand1,cand2"), prefer those
    // candidates when computing the best suggestion. Otherwise fall back to the
    // conservative builtin list.
    //
    // Returns the best candidate if its distance is within a reasonable threshold.
    fn suggest_similar_command(unknown: &str) -> Option<(String, Option<String>)> {
        // Detect if `unknown` was augmented with serde-provided expected variants.
        let (unknown_token, serde_candidates): (String, Option<Vec<String>>) =
            if let Some(idx) = unknown.find("||") {
                let (u, rest) = unknown.split_at(idx);
                let rest = &rest[2..];
                let candidates: Vec<String> = rest
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                (u.to_lowercase(), Some(candidates))
            } else {
                (unknown.to_lowercase(), None)
            };

        // Choose candidate set: prefer serde-provided ones when available.
        let mut best: Option<(String, usize)> = None;

        if let Some(cands) = serde_candidates {
            for cand in cands.iter() {
                let cand_norm = cand.to_lowercase();
                let dist = Self::levenshtein(&unknown_token, &cand_norm);
                if best.is_none() || dist < best.as_ref().unwrap().1 {
                    best = Some((cand.clone(), dist));
                }
            }
        } else {
            // Use dynamically generated builtin candidates.
            let builtin_candidates = crate::actor::wm_controller::WmCommand::builtin_candidates();
            for cand in builtin_candidates.iter() {
                let dist = Self::levenshtein(&unknown_token, &cand.to_lowercase());
                if best.is_none() || dist < best.as_ref().unwrap().1 {
                    best = Some((cand.to_string(), dist));
                }
            }
        }

        if let Some((best_cand, dist)) = best {
            // Heuristic threshold: allow suggestions if distance is <= half the length (or <=3).
            let threshold = std::cmp::max(3usize, best_cand.len() / 2);
            if dist <= threshold {
                // If the best candidate is in deprecated map, return the non-deprecated suggestion.
                let mut replacement = None;
                for &(dep, repl) in DEPRECATED_MAP.iter() {
                    if dep == best_cand {
                        replacement = Some(repl.to_string());
                        break;
                    }
                }
                return Some((best_cand.to_string(), replacement));
            }
        }

        // Also check if the unknown token itself matched a deprecated name exactly
        for &(dep, repl) in DEPRECATED_MAP.iter() {
            if dep == unknown_token {
                return Some((repl.to_string(), None)); // recommend replacement
            }
        }

        None
    }

    fn parse(buf: &str) -> anyhow::Result<Config> {
        // Attempt to deserialize. If it fails, and the error indicates an unknown enum
        // variant, attempt to provide a helpful suggestion.
        match parse_config_file(buf) {
            Ok(c) => {
                let mut keys = Vec::new();
                let mut key_specs = Vec::new();
                for (key, cmd) in c.keys {
                    let expanded_key =
                        Self::expand_modifier_combinations(&key, &c.modifier_combinations);
                    let normalized_key = Self::normalize_hotkey_string(&expanded_key);
                    let Ok(hotkey) = Hotkey::from_str(&normalized_key) else {
                        bail!("Could not parse hotkey: {key}");
                    };
                    keys.push((hotkey, cmd.clone()));
                    key_specs.push((normalized_key, cmd));
                }
                Ok(Config {
                    settings: c.settings,
                    keys,
                    key_specs,
                    virtual_workspaces: c.virtual_workspaces,
                })
            }
            Err(e) => {
                let msg = e.to_string();
                if let Some(unknown_token) = Self::extract_unknown_variant(&msg) {
                    if let Some((suggestion, deprecated_replacement)) =
                        Self::suggest_similar_command(&unknown_token)
                    {
                        if let Some(repl) = deprecated_replacement {
                            bail!(
                                "{msg}\nDid you mean `{}`? Note: `{}` is deprecated; use `{}` instead.",
                                suggestion,
                                suggestion,
                                repl
                            );
                        } else {
                            bail!("{msg}\nDid you mean `{}`?", suggestion);
                        }
                    } else {
                        bail!("{msg}");
                    }
                } else {
                    bail!("{msg}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rift_protocol::{
        FloatingWindowSize, FloatingWindowSizePreset, ToggleWindowFloatingOptions,
    };

    use super::*;
    use crate::actor::reactor;
    use crate::actor::wm_controller::ConfiguredLayoutCommand;
    use crate::layout_engine::{LayoutCommand, ResizeOrientation};

    #[test]
    fn layout_insertion_point_supports_global_default_and_per_mode_override() {
        let settings: LayoutSettings = toml::from_str(
            r#"
                window_insertion_point = "end_of_tree"

                [traditional]
                window_insertion_point = "next_to_selection"
                equalize_nodes = true

                [scrolling]
                animate = false
            "#,
        )
        .unwrap();

        assert_eq!(
            settings.window_insertion_point_for(LayoutMode::Traditional),
            WindowInsertionPoint::NextToSelection
        );
        assert_eq!(
            settings.window_insertion_point_for(LayoutMode::Bsp),
            WindowInsertionPoint::EndOfTree
        );
        assert!(settings.traditional.equalize_nodes);
        assert_eq!(settings.scrolling.animate, Some(false));
    }

    #[test]
    fn virtual_workspace_prevent_wrapping_defaults_to_false_and_accepts_suggested_alias() {
        let defaults: VirtualWorkspaceSettings = toml::from_str("").unwrap();
        assert!(!defaults.prevent_wrapping);

        let settings: VirtualWorkspaceSettings =
            toml::from_str("prevent_wrapping_around = true").unwrap();
        assert!(settings.prevent_wrapping);
    }

    #[test]
    fn app_rules_parse_placement_size_and_focus() {
        let settings: VirtualWorkspaceSettings = toml::from_str(
            r#"
                app_rules = [{
                    app_id = "com.example.Tool",
                    floating = true,
                    position = { x = 0.4, y = 0.7 },
                    size = { w = 640, h = 480 },
                    focus = true
                }]
            "#,
        )
        .unwrap();

        let rule = &settings.app_rules[0];
        assert_eq!(rule.position, Some(AppRulePosition { x: 0.4, y: 0.7 }));
        assert_eq!(rule.size, Some(AppRuleSize { w: Some(640.0), h: Some(480.0) }));
        assert!(rule.focus);
        assert!(settings.validate().is_empty());

        let height_only: VirtualWorkspaceSettings = toml::from_str(
            r#"
                app_rules = [{
                    app_id = "com.example.Panel",
                    size = { h = 320 }
                }]
            "#,
        )
        .unwrap();
        assert_eq!(
            height_only.app_rules[0].size,
            Some(AppRuleSize { w: None, h: Some(320.0) })
        );
        assert!(height_only.validate().is_empty());
    }

    #[test]
    fn app_rule_geometry_validation_rejects_invalid_values() {
        let mut settings = VirtualWorkspaceSettings::default();
        settings.app_rules.push(AppWorkspaceRule {
            app_id: Some("com.example.Tool".into()),
            workspace: None,
            floating: false,
            position: Some(AppRulePosition { x: -0.1, y: 1.1 }),
            size: Some(AppRuleSize {
                w: Some(0.0),
                h: Some(f64::NAN),
            }),
            focus: false,
            manage: Some(true),
            app_name: None,
            title_regex: None,
            title_substring: None,
            ax_role: None,
            ax_subrole: None,
        });

        let issues = settings.validate();
        assert!(issues.iter().any(|issue| issue.contains("between 0 and 1")));
        assert!(issues.iter().any(|issue| issue.contains("only applies")));
        assert!(issues.iter().any(|issue| issue.contains("finite positive")));
    }

    #[test]
    fn app_rule_validation_reports_invalid_regex_and_ignored_effects() {
        let mut settings = VirtualWorkspaceSettings::default();
        settings.app_rules.push(AppWorkspaceRule {
            app_id: Some("com.example.Tool".into()),
            workspace: Some(WorkspaceSelector::Index(1)),
            floating: true,
            focus: true,
            manage: Some(false),
            title_regex: Some("[".into()),
            ..Default::default()
        });

        let issues = settings.validate();
        assert!(issues.iter().any(|issue| issue.contains("invalid title_regex")));
        assert!(issues.iter().any(|issue| issue.contains("effects are ignored")));
    }

    #[test]
    fn resize_command_config_supports_legacy_and_oriented_forms() {
        #[derive(Deserialize)]
        struct TestConfig {
            keys: HashMap<String, WmCommand>,
        }

        let mut document: toml::Value = toml::from_str(
            r#"
            [keys]
            legacy = "resize_window_grow"
            vertical = { resize_window_shrink = "vertical" }
            smart = { resize_window_grow = "smart" }
            "#,
        )
        .unwrap();
        assert!(migrate_legacy_resize_bindings(&mut document));
        let config: TestConfig = document.try_into().unwrap();

        assert_eq!(
            config.keys["legacy"],
            WmCommand::ReactorCommand(reactor::Command::Layout(LayoutCommand::ResizeWindowGrow(
                ResizeOrientation::Horizontal
            )))
        );
        assert_eq!(
            config.keys["vertical"],
            WmCommand::ReactorCommand(reactor::Command::Layout(LayoutCommand::ResizeWindowShrink(
                ResizeOrientation::Vertical
            )))
        );
        assert_eq!(
            config.keys["smart"],
            WmCommand::ReactorCommand(reactor::Command::Layout(LayoutCommand::ResizeWindowGrow(
                ResizeOrientation::Smart
            )))
        );
    }

    #[test]
    fn toggle_window_floating_accepts_its_documented_option_forms() {
        #[derive(Deserialize)]
        struct TestConfig {
            keys: HashMap<String, WmCommand>,
        }

        let config: TestConfig = toml::from_str(
            r#"
            [keys]
            bare = "toggle_window_floating"
            centered = { toggle_window_floating = { center = true } }
            smart = { toggle_window_floating = { center = true, size = "smart" } }
            sized = { toggle_window_floating = { center = false, size = { w = 640.0, h = 480.0 } } }
            "#,
        )
        .unwrap();

        assert_eq!(
            config.keys["bare"],
            WmCommand::ReactorCommand(reactor::Command::Layout(
                LayoutCommand::ToggleWindowFloating
            ))
        );
        assert_eq!(
            config.keys["centered"],
            WmCommand::ConfiguredLayout(ConfiguredLayoutCommand::ToggleWindowFloating(
                ToggleWindowFloatingOptions { center: true, size: None }
            ))
        );
        assert_eq!(
            config.keys["smart"],
            WmCommand::ConfiguredLayout(ConfiguredLayoutCommand::ToggleWindowFloating(
                ToggleWindowFloatingOptions {
                    center: true,
                    size: Some(FloatingWindowSize::Preset(FloatingWindowSizePreset::Smart)),
                }
            ))
        );
        assert_eq!(
            config.keys["sized"],
            WmCommand::ConfiguredLayout(ConfiguredLayoutCommand::ToggleWindowFloating(
                ToggleWindowFloatingOptions {
                    center: false,
                    size: Some(FloatingWindowSize::Dimensions { w: 640.0, h: 480.0 }),
                }
            ))
        );
    }

    #[test]
    fn mouse_modifier_drag_settings_parse_and_default_to_off() {
        let default: MouseSettings = toml::from_str("").unwrap();
        assert_eq!(default.modifier, None);
        assert_eq!(default.action_for(MouseButton::Left), None);

        let settings: MouseSettings = toml::from_str(
            r#"
            modifier = "Alt"
            action1 = "move"
            action2 = "resize"
            "#,
        )
        .unwrap();
        assert_eq!(
            settings.action_for(MouseButton::Left),
            Some((Modifiers::ALT, MouseAction::Move))
        );
        assert_eq!(
            settings.action_for(MouseButton::Right),
            Some((Modifiers::ALT, MouseAction::Resize))
        );

        // An action set to none stays inert even with a modifier configured.
        let one_sided: MouseSettings =
            toml::from_str("modifier = \"Ctrl + Alt\"\naction1 = \"move\"").unwrap();
        assert_eq!(one_sided.action_for(MouseButton::Right), None);

        // A modifier spec naming a real key is a config error, not a silent
        // binding of the modifier alone.
        assert!(toml::from_str::<MouseSettings>("modifier = \"Alt + T\"").is_err());
    }

    #[test]
    fn menu_bar_layout_folder_defaults_and_expands_home() {
        let settings: MenuBarSettings = toml::from_str("").unwrap();

        assert_eq!(settings.layout_folder, PathBuf::from("~/.config/rift/layouts"));
        assert_eq!(
            settings.resolved_layout_folder(),
            dirs::home_dir().unwrap().join(".config/rift/layouts")
        );
    }

    #[test]
    fn menu_bar_layout_folder_preserves_absolute_paths() {
        let settings: MenuBarSettings =
            toml::from_str("layout_folder = \"/tmp/rift-layouts\"").unwrap();

        assert_eq!(
            settings.resolved_layout_folder(),
            PathBuf::from("/tmp/rift-layouts")
        );
    }

    #[test]
    fn test_normalize_hotkey_string() {
        assert_eq!(
            Config::normalize_hotkey_string("Alt + Shift + Down"),
            "Alt + Shift + ArrowDown"
        );
        assert_eq!(Config::normalize_hotkey_string("Ctrl + Up"), "Ctrl + ArrowUp");
        assert_eq!(
            Config::normalize_hotkey_string("Shift + Left"),
            "Shift + ArrowLeft"
        );
        assert_eq!(
            Config::normalize_hotkey_string("Meta + Right"),
            "Meta + ArrowRight"
        );
    }

    #[test]
    fn test_modifier_combinations_in_config() {
        let toml = r#"
            [settings]
            animate = false

            [modifier_combinations]
            comb1 = "Alt + Shift"
            leader = "Ctrl + Alt"

            [keys]
            "comb1 + C" = "toggle_space_activated"
            "leader + Tab" = "next_workspace"
            "Alt + H" = { move_focus = "left" }
        "#;

        let cfg = Config::parse(toml).unwrap();
        // We expect keys to be parsed into hotkeys
        assert!(!cfg.keys.is_empty());
    }

    #[test]
    fn serde_round_trip_preserves_key_specs() {
        let cfg = Config::default();
        assert!(!cfg.key_specs.is_empty());

        let json = serde_json::to_string(&cfg).unwrap();
        let round_tripped: Config = serde_json::from_str(&json).unwrap();

        assert_eq!(round_tripped.key_specs, cfg.key_specs);
    }

    #[test]
    fn serde_without_key_specs_reconstructs_from_keys() {
        let cfg = Config::default();
        let mut json = serde_json::to_value(&cfg).unwrap();
        json.as_object_mut().unwrap().remove("key_specs");

        let round_tripped: Config = serde_json::from_value(json).unwrap();

        assert_eq!(round_tripped.key_specs.len(), round_tripped.keys.len());
        assert!(!round_tripped.key_specs.is_empty());
    }

    #[test]
    fn test_levenshtein_suggests() {
        let err =
            "unknown variant `toggle_stak`, expected one of `toggle_stack`, `toggle_orientation`";
        let token = Config::extract_unknown_variant(err).unwrap();
        assert_eq!(token, "toggle_stak||toggle_stack,toggle_orientation");
        let suggestion = Config::suggest_similar_command(&token);
        assert!(suggestion.is_some());
        let (s, _maybe_dep) = suggestion.unwrap();
        assert_eq!(s, "toggle_stack");
    }
}
