use super::*;

pub(super) const CURRENT_SCHEMA_VERSION: u32 = 3;

fn legacy_schema_version() -> u32 { 0 }

/// Owned, versioned representation of the layout file.
///
/// Keep this type independent from runtime-only `LayoutEngine` fields. Adding an engine cache,
/// service, or transient index must never silently alter the persistence schema again.
#[derive(Deserialize)]
pub(super) struct PersistedLayout {
    #[serde(default = "legacy_schema_version")]
    pub(super) schema_version: u32,
    pub(super) workspace_layouts: WorkspaceLayouts,
    pub(super) floating: FloatingManager,
    pub(super) floating_positions: FloatingPositionStore,
    pub(super) virtual_workspace_manager: WorkspaceStore,
    #[serde(default)]
    pub(super) space_display_map: HashMap<SpaceId, Option<String>>,
    #[serde(default)]
    pub(super) display_last_space: HashMap<String, SpaceId>,
    #[serde(flatten)]
    pub(super) persistence: PersistenceState,
}

/// Borrowed serialization view, avoiding a deep clone of every layout tree during save.
#[derive(Serialize)]
struct PersistedLayoutRef<'a> {
    schema_version: u32,
    workspace_layouts: &'a WorkspaceLayouts,
    floating: &'a FloatingManager,
    floating_positions: &'a FloatingPositionStore,
    virtual_workspace_manager: &'a WorkspaceStore,
    space_display_map: &'a HashMap<SpaceId, Option<String>>,
    display_last_space: &'a HashMap<String, SpaceId>,
    #[serde(flatten)]
    persistence: &'a PersistenceState,
}

impl PersistedLayout {
    pub(super) fn deserialize(buf: &str) -> Result<Self, ron::error::SpannedError> {
        ron::from_str(buf)
    }

    pub(super) fn serialize_engine(engine: &LayoutEngine) -> String {
        ron::ser::to_string(&PersistedLayoutRef {
            schema_version: CURRENT_SCHEMA_VERSION,
            workspace_layouts: &engine.workspace_layouts,
            floating: &engine.floating,
            floating_positions: &engine.floating_positions,
            virtual_workspace_manager: &engine.virtual_workspace_manager,
            space_display_map: &engine.space_display_map,
            display_last_space: &engine.display_last_space,
            persistence: &engine.persistence,
        })
        .expect("persisted layout serialization must support all engine layout state")
    }

    /// Serialize an owned snapshot, the same way a live engine is.
    pub(super) fn serialize(&self) -> String {
        ron::ser::to_string(&PersistedLayoutRef {
            schema_version: self.schema_version,
            workspace_layouts: &self.workspace_layouts,
            floating: &self.floating,
            floating_positions: &self.floating_positions,
            virtual_workspace_manager: &self.virtual_workspace_manager,
            space_display_map: &self.space_display_map,
            display_last_space: &self.display_last_space,
            persistence: &self.persistence,
        })
        .expect("persisted layout serialization must support all engine layout state")
    }

    /// Drop spaces that have workspaces but no layout state. A space rift has
    /// listed but never shown (an inactive desktop on some display) is in
    /// that state, and the loader rejects a file that contains one — which
    /// made every snapshot taken on a machine with a spare desktop useless.
    /// The live engine is never pruned: it lays the space out on exposure,
    /// and so will the restored one.
    pub(super) fn prune_spaces_without_layout_state(&mut self) {
        let unexposed: Vec<SpaceId> =
            self.virtual_workspace_manager
                .initialized_spaces()
                .into_iter()
                .filter(|space| {
                    self.virtual_workspace_manager.existing_workspaces(*space).iter().any(
                        |(workspace, _)| !self.workspace_layouts.contains_workspace(*workspace),
                    )
                })
                .collect();
        for space in unexposed {
            // Collected before the store forgets the space, which is what
            // knows the space's workspaces.
            let workspaces: Vec<_> = self
                .virtual_workspace_manager
                .existing_workspaces(space)
                .into_iter()
                .map(|(workspace, _)| workspace)
                .collect();
            self.virtual_workspace_manager.forget_space(space);
            self.workspace_layouts.remove_workspaces(workspaces);
            self.floating_positions.remove_space(space);
            self.space_display_map.remove(&space);
            self.display_last_space.retain(|_, candidate| *candidate != space);
        }
    }

    /// The checks the loader runs before layout code indexes into slotmaps.
    /// A save runs the same ones, so a file rift writes is one it can read.
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        self.virtual_workspace_manager
            .validate_persisted_topology()
            .map_err(|error| anyhow::anyhow!("invalid workspace topology: {error}"))?;
        self.workspace_layouts
            .validate_persisted(&self.virtual_workspace_manager)
            .map_err(|error| anyhow::anyhow!("invalid workspace layouts: {error}"))?;
        self.floating_positions
            .validate_persisted(&self.virtual_workspace_manager)
            .map_err(|error| anyhow::anyhow!("invalid floating positions: {error}"))?;
        self.persistence.validate()
    }

    pub(super) fn into_engine(self) -> LayoutEngine {
        LayoutEngine {
            workspace_layouts: self.workspace_layouts,
            floating: self.floating,
            floating_positions: self.floating_positions,
            app_rules: AppRuleEngine::default(),
            focused_window: None,
            window_layout_constraints: HashMap::default(),
            observed_min_sizes: HashMap::default(),
            virtual_workspace_manager: self.virtual_workspace_manager,
            layout_settings: LayoutSettings::default(),
            broadcast_tx: None,
            space_display_map: self.space_display_map,
            display_last_space: self.display_last_space,
            persistence: self.persistence,
            startup_restore_pending: false,
            pending_float_placement: HashMap::default(),
            frozen_window: None,
        }
    }
}
