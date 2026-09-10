use objc2_core_foundation::CGSize;
use serde::{Deserialize, Serialize};

use super::{LayoutId, LayoutSystem};

/// Every workspace's layout configurations, by workspace.
///
/// Keyed by `VirtualWorkspaceId` alone, and deliberately not by the native
/// space the workspace sits on. Workspace ids come from one slot map shared by
/// every space, so they are already unique on their own; the native space in
/// the key was redundant, and worse, it was a key the window server owns and
/// re-mints — destroying a desktop at an unplug and minting a fresh id for it
/// at the replug. Every layout here then had to be carried from the dead id to
/// the new one by hand, and a carry that missed left the tree stranded under an
/// id nothing pointed at any more: a stacked desktop came back tiled. Keyed by
/// the workspace, there is nothing to carry — the layout belongs to the
/// workspace, and the workspace is what survives.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub(crate) struct WorkspaceLayouts {
    map: crate::common::collections::HashMap<crate::model::VirtualWorkspaceId, SpaceLayoutInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct SpaceLayoutInfo {
    configurations: crate::common::collections::HashMap<Size, LayoutId>,
    active_size: Size,
    last_saved: Option<LayoutId>,
}

/// Opaque workspace-layout payload used by transactional restore code.
/// Keeping `SpaceLayoutInfo` private prevents persistence from depending on its internal maps.
pub(crate) struct WorkspaceLayoutSnapshot(SpaceLayoutInfo);

impl SpaceLayoutInfo {
    fn active(&self) -> Option<LayoutId> { self.configurations.get(&self.active_size).copied() }
}

#[derive(Serialize, Deserialize, Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd, Debug)]
pub(crate) struct Size {
    width: i32,
    height: i32,
}

impl From<CGSize> for Size {
    fn from(value: CGSize) -> Self {
        Self {
            width: value.width.round() as i32,
            height: value.height.round() as i32,
        }
    }
}

impl WorkspaceLayouts {
    pub(crate) fn active_size(
        &self,
        workspace: crate::model::VirtualWorkspaceId,
    ) -> Option<CGSize> {
        self.map
            .get(&workspace)
            .map(|info| CGSize::new(info.active_size.width.into(), info.active_size.height.into()))
    }

    pub(crate) fn validate_persisted(
        &self,
        workspaces: &crate::model::WorkspaceStore,
    ) -> Result<(), String> {
        for (&workspace, info) in &self.map {
            let Some(workspace_info) = workspaces.workspaces.get(workspace) else {
                return Err(format!(
                    "layout state references missing workspace {workspace:?}"
                ));
            };
            if info.configurations.is_empty() {
                return Err(format!("workspace {workspace:?} has no layout configurations"));
            }
            if !info.configurations.contains_key(&info.active_size) {
                return Err(format!(
                    "workspace {workspace:?} has no configuration for its active display size"
                ));
            }
            for layout in info.configurations.values().copied().chain(info.last_saved) {
                if !workspace_info.layout_system.contains_layout(layout) {
                    return Err(format!(
                        "workspace {workspace:?} references missing layout {layout:?}"
                    ));
                }
            }
        }

        for space in workspaces.initialized_spaces() {
            for (workspace, _) in workspaces.existing_workspaces(space) {
                if !self.map.contains_key(&workspace) {
                    return Err(format!(
                        "workspace {workspace:?} on native space {} has no layout state",
                        space.get()
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn snapshot_workspace(
        &self,
        workspace: crate::model::VirtualWorkspaceId,
    ) -> Option<WorkspaceLayoutSnapshot> {
        self.map.get(&workspace).cloned().map(WorkspaceLayoutSnapshot)
    }

    pub(crate) fn install_workspace_snapshot(
        &mut self,
        workspace: crate::model::VirtualWorkspaceId,
        snapshot: WorkspaceLayoutSnapshot,
    ) {
        self.map.insert(workspace, snapshot.0);
    }

    pub(crate) fn contains_workspace(&self, workspace: crate::model::VirtualWorkspaceId) -> bool {
        self.map.contains_key(&workspace)
    }

    pub(crate) fn ensure_active_for_space(
        &mut self,
        size: CGSize,
        workspaces: impl IntoIterator<Item = crate::model::VirtualWorkspaceId>,
        tree: &mut impl LayoutSystem,
    ) {
        let size = Size::from(size);
        for workspace_id in workspaces {
            let (workspace_layout, previous_layout) = match self.map.entry(workspace_id) {
                crate::common::collections::hash_map::Entry::Vacant(entry) => (
                    entry.insert(SpaceLayoutInfo {
                        active_size: size,
                        configurations: Default::default(),
                        last_saved: None,
                    }),
                    None,
                ),
                crate::common::collections::hash_map::Entry::Occupied(entry) => {
                    let info = entry.into_mut();
                    let previous_layout = info.active();
                    info.active_size = size;
                    (info, previous_layout)
                }
            };

            let layout = match workspace_layout.configurations.entry(size) {
                crate::common::collections::hash_map::Entry::Vacant(entry) => {
                    *entry.insert(if let Some(source) = previous_layout {
                        tree.clone_layout(source)
                    } else if let Some(source) = workspace_layout.last_saved {
                        tree.clone_layout(source)
                    } else {
                        tree.create_layout()
                    })
                }
                crate::common::collections::hash_map::Entry::Occupied(entry) => {
                    workspace_layout.last_saved = Some(*entry.get());
                    *entry.get()
                }
            };

            tracing::debug!("Using layout {:?} for workspace {:?}", layout, workspace_id);
        }
    }

    pub(crate) fn active(
        &self,
        workspace_id: crate::model::VirtualWorkspaceId,
    ) -> Option<LayoutId> {
        self.map.get(&workspace_id).and_then(|l| l.active())
    }

    pub(crate) fn mark_last_saved(
        &mut self,
        workspace_id: crate::model::VirtualWorkspaceId,
        layout: LayoutId,
    ) {
        if let Some(info) = self.map.get_mut(&workspace_id) {
            info.last_saved = Some(layout);
        }
    }

    /// The active layout of each of `workspaces` that has one, in workspace order.
    pub(crate) fn active_layouts_for(
        &self,
        workspaces: impl IntoIterator<Item = crate::model::VirtualWorkspaceId>,
    ) -> Vec<(crate::model::VirtualWorkspaceId, LayoutId)> {
        let mut layouts = workspaces
            .into_iter()
            .filter_map(|workspace| {
                self.map.get(&workspace).and_then(|info| info.active()).map(|l| (workspace, l))
            })
            .collect::<Vec<_>>();
        layouts.sort_unstable();
        layouts
    }

    /// Enumerate every serialized layout configuration, not only the currently active display
    /// size. Old-size configurations are restored later and therefore must be sanitized too.
    pub(crate) fn all_layouts(&self) -> Vec<(crate::model::VirtualWorkspaceId, LayoutId)> {
        let mut layouts = Vec::new();
        for (&workspace, info) in &self.map {
            layouts.extend(info.configurations.values().map(|layout| (workspace, *layout)));
            if let Some(layout) = info.last_saved {
                layouts.push((workspace, layout));
            }
        }
        layouts.sort_unstable();
        layouts.dedup();
        layouts
    }

    #[cfg(test)]
    pub(crate) fn insert_layout_configuration_for_test(
        &mut self,
        workspace: crate::model::VirtualWorkspaceId,
        size: CGSize,
        layout: LayoutId,
    ) {
        self.map
            .get_mut(&workspace)
            .expect("test workspace must be initialized")
            .configurations
            .insert(Size::from(size), layout);
    }

    pub(crate) fn has_state(&self, workspace_id: crate::model::VirtualWorkspaceId) -> bool {
        self.map.contains_key(&workspace_id)
    }

    pub(crate) fn ensure_active_for_workspace(
        &mut self,
        size: CGSize,
        workspace_id: crate::model::VirtualWorkspaceId,
        tree: &mut impl LayoutSystem,
    ) {
        self.ensure_active_for_space(size, std::iter::once(workspace_id), tree);
    }

    pub(crate) fn replace_layouts_for_workspace(
        &mut self,
        workspace_id: crate::model::VirtualWorkspaceId,
        new_layout: LayoutId,
    ) {
        let active_size = self
            .map
            .get(&workspace_id)
            .map(|info| info.active_size)
            .unwrap_or_else(|| Size::from(CGSize::new(1000.0, 1000.0)));

        let mut configurations = crate::common::collections::HashMap::default();
        configurations.insert(active_size, new_layout);

        self.map.insert(workspace_id, SpaceLayoutInfo {
            configurations,
            active_size,
            last_saved: Some(new_layout),
        });
    }

    /// Drops the layout bookkeeping for a destroyed workspace.
    ///
    /// The layout trees themselves live on the workspace and go away with it;
    /// this is the side index, which would otherwise keep an entry pointing at
    /// a workspace id that no longer resolves.
    pub(crate) fn remove_workspace(&mut self, workspace_id: crate::model::VirtualWorkspaceId) {
        self.map.remove(&workspace_id);
    }

    /// Drops the layout bookkeeping of every workspace given.
    pub(crate) fn remove_workspaces(
        &mut self,
        workspaces: impl IntoIterator<Item = crate::model::VirtualWorkspaceId>,
    ) {
        for workspace in workspaces {
            self.map.remove(&workspace);
        }
    }

    /// Every workspace that has layout state.
    pub(crate) fn workspaces(
        &self,
    ) -> crate::common::collections::BTreeSet<crate::model::VirtualWorkspaceId> {
        self.map.keys().copied().collect()
    }
}
