use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use serde::{Deserialize, Serialize};
use slotmap::Key;

use crate::actor::app::{WindowId, pid_t};
use crate::common::collections::HashMap;
use crate::common::config::WindowInsertionPoint;
use crate::layout_engine::systems::constraints::{AxisConstraints, solve_axis_lengths};
use crate::layout_engine::systems::{
    LayoutSystem, WindowLayoutConstraints, reconcile_app_membership,
};
use crate::layout_engine::utils::compute_tiling_area;
use crate::layout_engine::{Direction, LayoutId, LayoutKind, Orientation, ResizeOrientation};
use crate::model::selection::*;
use crate::model::tree::{NodeId, NodeMap, Tree};

#[derive(Serialize, Deserialize, Clone, Debug)]
enum NodeKind {
    Split {
        orientation: Orientation,
        ratio: f32,
    },
    Leaf {
        window: Option<WindowId>,
        fullscreen: bool,
        fullscreen_within_gaps: bool,
        preselected: Option<Direction>,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
struct LayoutState {
    root: NodeId,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BspLayoutSystem {
    layouts: slotmap::SlotMap<crate::layout_engine::LayoutId, LayoutState>,
    tree: Tree<Components>,
    kind: slotmap::SecondaryMap<NodeId, NodeKind>,
    window_to_node: HashMap<WindowId, NodeId>,
    #[serde(skip, default)]
    window_insertion_point: WindowInsertionPoint,
}

impl BspLayoutSystem {
    /// Every node of a layout, collected before any mutation.
    ///
    /// Rotating and mirroring relink children as they go, so the traversal has
    /// to finish before the tree starts moving underneath it.
    fn nodes_in_layout(&self, layout: LayoutId) -> Vec<NodeId> {
        self.layouts
            .get(layout)
            .map(|state| state.root.traverse_preorder(&self.tree.map).collect())
            .unwrap_or_default()
    }

    /// Reverses a split's two children and its ratio with them, so the
    /// proportions travel with the windows instead of staying put.
    fn reverse_split_children(&mut self, node: NodeId) {
        let children: Vec<NodeId> = node.children(&self.tree.map).collect();
        if children.len() != 2 {
            return;
        }
        if let Some(NodeKind::Split { ratio, .. }) = self.kind.get_mut(node) {
            *ratio = 1.0 - *ratio;
        }
        children[0].detach(&mut self.tree).push_back(node);
    }

    fn split_orientation(&self, node: NodeId) -> Option<Orientation> {
        match self.kind.get(node) {
            Some(NodeKind::Split { orientation, .. }) => Some(*orientation),
            _ => None,
        }
    }

    fn flip_split_orientation(&mut self, node: NodeId) {
        if let Some(NodeKind::Split { orientation, .. }) = self.kind.get_mut(node) {
            *orientation = match *orientation {
                Orientation::Horizontal => Orientation::Vertical,
                Orientation::Vertical => Orientation::Horizontal,
            };
        }
    }

    fn find_neighbor_leaf(&self, from_leaf: NodeId, direction: Direction) -> Option<NodeId> {
        let mut current = from_leaf;

        while let Some(parent) = current.parent(&self.tree.map) {
            if let Some(NodeKind::Split { orientation, .. }) = self.kind.get(parent) {
                if *orientation == direction.orientation() {
                    let children: Vec<_> = parent.children(&self.tree.map).collect();
                    if children.len() == 2 {
                        let is_first = children[0] == current;
                        let target_child = match direction {
                            Direction::Left | Direction::Up => {
                                if !is_first {
                                    Some(children[0])
                                } else {
                                    None
                                }
                            }
                            Direction::Right | Direction::Down => {
                                if is_first {
                                    Some(children[1])
                                } else {
                                    None
                                }
                            }
                        };

                        if let Some(target) = target_child {
                            return Some(self.find_closest_leaf_in_direction(target, direction));
                        }
                    }
                }
            }
            current = parent;
        }

        None
    }

    fn find_closest_leaf_in_direction(&self, root: NodeId, direction: Direction) -> NodeId {
        match self.kind.get(root) {
            Some(NodeKind::Leaf { .. }) => root,
            Some(NodeKind::Split { orientation, .. }) => {
                let children: Vec<_> = root.children(&self.tree.map).collect();
                if children.is_empty() {
                    return root;
                }

                let target_child = if *orientation == direction.orientation() {
                    match direction {
                        Direction::Left | Direction::Up => children.last().copied(),
                        Direction::Right | Direction::Down => children.first().copied(),
                    }
                } else {
                    children.first().copied()
                };

                if let Some(child) = target_child {
                    self.find_closest_leaf_in_direction(child, direction)
                } else {
                    root
                }
            }
            None => root,
        }
    }

    fn window_in_direction_from(&self, node: NodeId, direction: Direction) -> Option<WindowId> {
        match self.kind.get(node) {
            Some(NodeKind::Leaf { window: Some(w), .. }) => Some(*w),
            Some(NodeKind::Leaf { .. }) => None,
            Some(NodeKind::Split { .. }) => {
                let mut children: Vec<_> = node.children(&self.tree.map).collect();
                match direction {
                    Direction::Left | Direction::Up => children.reverse(),
                    Direction::Right | Direction::Down => {}
                }
                for child in children {
                    if let Some(window) = self.window_in_direction_from(child, direction) {
                        return Some(window);
                    }
                }
                None
            }
            None => None,
        }
    }

    fn smart_insert_window(&mut self, layout: LayoutId, window: WindowId) -> bool {
        if let Some(sel) = self.selection_of_layout(layout) {
            let leaf = self.descend_to_leaf(sel);

            if let Some(NodeKind::Leaf {
                preselected: Some(direction), ..
            }) = self.kind.get(leaf).cloned()
            {
                self.split_leaf_in_direction(leaf, direction, window);

                if let Some(NodeKind::Leaf { preselected, .. }) = self.kind.get_mut(leaf) {
                    *preselected = None;
                }
                return true;
            }
        }
        false
    }

    fn split_leaf_in_direction(
        &mut self,
        leaf: NodeId,
        direction: Direction,
        new_window: WindowId,
    ) {
        if let Some(NodeKind::Leaf {
            window,
            fullscreen,
            fullscreen_within_gaps,
            ..
        }) = self.kind.get(leaf).cloned()
        {
            let orientation = direction.orientation();

            let existing_node = self.make_leaf(window);
            self.keep_fullscreen(existing_node, fullscreen, fullscreen_within_gaps);
            let new_node = self.make_leaf(Some(new_window));

            if let Some(w) = window {
                self.index_window(w, existing_node);
            }
            self.index_window(new_window, new_node);

            self.kind.insert(leaf, NodeKind::Split { orientation, ratio: 0.5 });

            let (first_child, second_child) = match direction {
                Direction::Left | Direction::Up => (new_node, existing_node),
                Direction::Right | Direction::Down => (existing_node, new_node),
            };

            first_child.detach(&mut self.tree).push_back(leaf);
            second_child.detach(&mut self.tree).push_back(leaf);

            self.tree.data.selection.select(&self.tree.map, new_node);
        }
    }
}

impl Default for BspLayoutSystem {
    fn default() -> Self {
        Self {
            layouts: Default::default(),
            tree: Tree::with_observer(Components::default()),
            kind: Default::default(),
            window_to_node: Default::default(),
            window_insertion_point: WindowInsertionPoint::default(),
        }
    }
}

impl BspLayoutSystem {
    pub fn new(window_insertion_point: WindowInsertionPoint) -> Self {
        Self {
            window_insertion_point,
            ..Self::default()
        }
    }

    pub fn set_window_insertion_point(&mut self, value: WindowInsertionPoint) {
        self.window_insertion_point = value;
    }

    /// Every leaf holding `wid`, wherever it sits. `window_to_node` is meant
    /// to hold one node per window, but a leaf that lost its index entry —
    /// an identity replaced onto a window that already had a leaf, an insert
    /// that overwrote the entry — stays in the tree, rendered and given
    /// space, with nothing able to reach it: the "ghost tile".
    fn leaves_holding(&self, wid: WindowId) -> Vec<NodeId> {
        self.kind
            .iter()
            .filter_map(|(node, kind)| match kind {
                NodeKind::Leaf { window: Some(window), .. } if *window == wid => Some(node),
                _ => None,
            })
            .collect()
    }

    /// Takes every leaf for `wid` out of the tree, indexed or not, so that
    /// an insert that follows leaves exactly one — and a removal leaves none.
    /// A leaf the index has lost is otherwise unreachable: it stays in the
    /// tree, is laid out with the rest, and its frame writes drag the window
    /// back to a space it has left.
    fn retire_all_leaves(&mut self, wid: WindowId) {
        let indexed = self.node_for_window(wid);
        for node in self.leaves_holding(wid) {
            if self.kind.get(node).is_none() {
                continue;
            }
            if indexed != Some(node) {
                // Worth knowing about after the fact: which window, and
                // whether the index pointed elsewhere or nowhere.
                crate::sys::trace::act("ghost_leaf", &(wid.idx.get(), indexed.is_some()));
            }
            self.window_to_node.insert(wid, node);
            self.remove_indexed_window(wid);
        }
        self.unindex_window(wid);
    }

    /// Removes the leaf the index points at for `wid`, if any.
    fn remove_indexed_window(&mut self, wid: WindowId) {
        if let Some(node_id) = self.node_for_window_mut(wid) {
            let root = self.find_layout_root(node_id);
            let layout = self
                .layouts
                .iter()
                .find_map(|(id, s)| if s.root == root { Some(id) } else { None });
            if let Some(l) = layout {
                self.remove_window_internal(l, wid);
            }
        }
    }

    fn index_window(&mut self, wid: WindowId, node: NodeId) {
        debug_assert!(
            matches!(self.kind.get(node), Some(NodeKind::Leaf { .. })),
            "window index must reference a leaf node"
        );
        self.window_to_node.insert(wid, node);
    }

    fn unindex_window(&mut self, wid: WindowId) { self.window_to_node.remove(&wid); }

    fn node_for_window(&self, wid: WindowId) -> Option<NodeId> {
        self.window_to_node.get(&wid).copied()
    }

    fn node_for_window_mut(&mut self, wid: WindowId) -> Option<NodeId> {
        let node = self.window_to_node.get(&wid).copied()?;
        if matches!(self.kind.get(node), Some(NodeKind::Leaf { .. })) {
            Some(node)
        } else {
            self.unindex_window(wid);
            None
        }
    }

    /// A window's leaf is rebuilt when a neighbour is split in beside it, and
    /// a fresh leaf is not fullscreen: a window rejoining the layout while
    /// another was fullscreen took the fullscreen away. In the VM a TextEdit
    /// coming back after a replug knocked Safari out of fullscreen, and the
    /// next toggle turned it on again instead of off. The window keeps it.
    fn keep_fullscreen(&mut self, node: NodeId, fullscreen: bool, within_gaps: bool) {
        if let Some(NodeKind::Leaf {
            fullscreen: f,
            fullscreen_within_gaps: g,
            ..
        }) = self.kind.get_mut(node)
        {
            *f = fullscreen;
            *g = within_gaps;
        }
    }

    fn make_leaf(&mut self, window: Option<WindowId>) -> NodeId {
        let id = self.tree.mk_node().into_id();
        self.kind.insert(id, NodeKind::Leaf {
            window,
            fullscreen: false,
            fullscreen_within_gaps: false,
            preselected: None,
        });
        if let Some(w) = window {
            self.index_window(w, id);
        }
        id
    }

    fn descend_to_leaf(&self, mut node: NodeId) -> NodeId {
        loop {
            match self.kind.get(node) {
                Some(NodeKind::Leaf { .. }) => return node,
                Some(NodeKind::Split { .. }) => {
                    if let Some(child) = node.first_child(&self.tree.map) {
                        node = child;
                    } else {
                        return node;
                    }
                }
                None => return node,
            }
        }
    }

    /// Calculates the depth of a node in the BSP tree by counting parent levels.
    /// The root node has depth 0, its direct children have depth 1, and so on.
    fn node_depth(&self, node: NodeId) -> usize {
        let mut depth = 0;
        let mut current = node;
        while let Some(parent) = current.parent(&self.tree.map) {
            depth += 1;
            current = parent;
        }
        depth
    }

    /// Returns the orientation for a new split based on the node's depth.
    /// Creates a fibonacci spiral pattern by alternating orientations:
    /// - Even depth (0, 2, 4, ...) → Horizontal split
    /// - Odd depth (1, 3, 5, ...) → Vertical split
    fn orientation_for_depth(&self, depth: usize) -> Orientation {
        if depth % 2 == 0 {
            Orientation::Horizontal
        } else {
            Orientation::Vertical
        }
    }

    fn collect_windows_under(&self, node: NodeId, out: &mut Vec<WindowId>) {
        match self.kind.get(node) {
            Some(NodeKind::Leaf { window, .. }) => {
                if let Some(w) = window {
                    out.push(*w);
                }
            }
            Some(NodeKind::Split { .. }) => {
                for child in node.children(&self.tree.map) {
                    self.collect_windows_under(child, out);
                }
            }
            None => {}
        }
    }

    fn has_fullscreen_in_subtree(&self, node: NodeId) -> bool {
        match self.kind.get(node) {
            Some(NodeKind::Leaf {
                fullscreen,
                fullscreen_within_gaps,
                ..
            }) => *fullscreen || *fullscreen_within_gaps,
            Some(NodeKind::Split { .. }) => {
                node.children(&self.tree.map).any(|child| self.has_fullscreen_in_subtree(child))
            }
            None => false,
        }
    }

    fn find_layout_root(&self, mut node: NodeId) -> NodeId {
        while let Some(p) = node.parent(&self.tree.map) {
            node = p;
        }
        node
    }

    fn belongs_to_layout(&self, layout: LayoutState, node: NodeId) -> bool {
        if self.kind.get(node).is_none() {
            return false;
        }
        self.find_layout_root(node) == layout.root
    }

    fn cleanup_after_removal(&mut self, node: NodeId) -> NodeId {
        let Some(parent_id) = node.parent(&self.tree.map) else {
            return node;
        };

        if let Some(NodeKind::Split { .. }) = self.kind.get(parent_id) {
        } else {
            return parent_id;
        }

        let children: Vec<_> = parent_id.children(&self.tree.map).collect();
        if children.len() != 2 {
            return parent_id;
        }
        let sibling = if children[0] == node {
            children[1]
        } else {
            children[0]
        };

        let sibling_kind = match self.kind.get(sibling) {
            Some(k) => k.clone(),
            None => return parent_id,
        };

        self.kind.insert(parent_id, sibling_kind.clone());
        match sibling_kind {
            NodeKind::Split { .. } => {
                let sib_children: Vec<_> = sibling.children(&self.tree.map).collect();
                for c in sib_children {
                    c.detach(&mut self.tree).push_back(parent_id);
                }
            }
            NodeKind::Leaf {
                window,
                fullscreen,
                fullscreen_within_gaps,
                preselected,
            } => {
                if let Some(w) = window {
                    self.index_window(w, parent_id);
                }
                self.kind.insert(parent_id, NodeKind::Leaf {
                    window,
                    fullscreen,
                    fullscreen_within_gaps,
                    preselected,
                });
            }
        }

        node.detach(&mut self.tree).remove();
        sibling.detach(&mut self.tree).remove();
        self.kind.remove(node);
        self.kind.remove(sibling);
        parent_id
    }

    fn selection_of_layout(&self, layout: crate::layout_engine::LayoutId) -> Option<NodeId> {
        self.layouts
            .get(layout)
            .map(|s| self.tree.data.selection.current_selection(s.root))
    }

    fn insert_window_at_selection(
        &mut self,
        layout: crate::layout_engine::LayoutId,
        wid: WindowId,
    ) {
        let Some(state) = self.layouts.get(layout).copied() else {
            return;
        };
        let sel = self.tree.data.selection.current_selection(state.root);
        match self.kind.get_mut(sel) {
            Some(NodeKind::Leaf {
                window,
                fullscreen,
                fullscreen_within_gaps,
                ..
            }) => {
                if window.is_none() {
                    *window = Some(wid);
                    *fullscreen = false;
                    *fullscreen_within_gaps = false;
                    self.index_window(wid, sel);
                } else {
                    let existing = *window;
                    let (was_fullscreen, was_within_gaps) = (*fullscreen, *fullscreen_within_gaps);
                    let left = self.make_leaf(existing);
                    self.keep_fullscreen(left, was_fullscreen, was_within_gaps);
                    let right = self.make_leaf(Some(wid));
                    self.index_window(wid, right);
                    if let Some(w) = existing {
                        self.index_window(w, left);
                    }
                    // Use alternating orientations based on depth for fibonacci spiral
                    let depth = self.node_depth(sel);
                    let orientation = self.orientation_for_depth(depth);
                    self.kind.insert(sel, NodeKind::Split { orientation, ratio: 0.5 });
                    left.detach(&mut self.tree).push_back(sel);
                    right.detach(&mut self.tree).push_back(sel);
                    self.tree.data.selection.select(&self.tree.map, right);
                }
            }
            Some(NodeKind::Split { .. }) => {
                let leaf = self.descend_to_leaf(sel);
                self.tree.data.selection.select(&self.tree.map, leaf);
                self.insert_window_at_selection(layout, wid);
            }
            None => {}
        }
    }

    fn remove_window_internal(&mut self, layout: crate::layout_engine::LayoutId, wid: WindowId) {
        if let Some(node_id) = self.node_for_window_mut(wid) {
            if let Some(state) = self.layouts.get(layout).copied() {
                if !self.belongs_to_layout(state, node_id) {
                    return;
                }
            }
            if let Some(NodeKind::Leaf { window, .. }) = self.kind.get_mut(node_id) {
                *window = None;
            }
            self.unindex_window(wid);
            let fallback = self.cleanup_after_removal(node_id);

            let sel_snapshot = self
                .layouts
                .get(layout)
                .map(|s| self.tree.data.selection.current_selection(s.root));
            let new_sel = match sel_snapshot {
                Some(sel) if self.kind.get(sel).is_some() => self.descend_to_leaf(sel),
                _ => self.descend_to_leaf(fallback),
            };
            self.tree.data.selection.select(&self.tree.map, new_sel);
        }
    }

    fn apply_outer_gaps(screen: CGRect, gaps: &crate::common::config::GapSettings) -> CGRect {
        compute_tiling_area(screen, gaps)
    }

    fn calculate_layout_recursive(
        &self,
        node: NodeId,
        rect: CGRect,
        screen: CGRect,
        constraints: &HashMap<WindowId, WindowLayoutConstraints>,
        gaps: &crate::common::config::GapSettings,
        out: &mut Vec<(WindowId, CGRect)>,
        nodes: &mut Vec<(NodeId, CGRect)>,
    ) {
        nodes.push((node, rect));
        match self.kind.get(node) {
            Some(NodeKind::Leaf {
                window,
                fullscreen,
                fullscreen_within_gaps,
                ..
            }) => {
                if let Some(w) = window {
                    let mut target = if *fullscreen {
                        screen
                    } else if *fullscreen_within_gaps {
                        compute_tiling_area(screen, gaps)
                    } else {
                        rect
                    };
                    if !*fullscreen
                        && !*fullscreen_within_gaps
                        && let Some(c) = constraints.get(w).copied()
                    {
                        let c = c.normalized();
                        let desired_w = c
                            .fixed_for_axis(true)
                            .unwrap_or(target.size.width)
                            .max(c.min_for_axis(true));
                        let desired_h = c
                            .fixed_for_axis(false)
                            .unwrap_or(target.size.height)
                            .max(c.min_for_axis(false));
                        let desired_w = if c.max_for_axis(true) > 0.0 {
                            desired_w.min(c.max_for_axis(true))
                        } else {
                            desired_w
                        };
                        let desired_h = if c.max_for_axis(false) > 0.0 {
                            desired_h.min(c.max_for_axis(false))
                        } else {
                            desired_h
                        };
                        target.size.width = desired_w.min(target.size.width).max(0.0);
                        target.size.height = desired_h.min(target.size.height).max(0.0);
                    }
                    out.push((*w, target));
                }
            }
            Some(NodeKind::Split { orientation, ratio }) => match orientation {
                Orientation::Horizontal => {
                    let gap = gaps.inner.horizontal as f64;
                    let total = rect.size.width;
                    let available = (total - gap).max(0.0);
                    let mut it = node.children(&self.tree.map);
                    let first = it.next();
                    let second = it.next();
                    let (first_w, second_w) =
                        if let (Some(first_node), Some(second_node)) = (first, second) {
                            let (first_min, first_fixed, first_max, first_can_grow) =
                                self.subtree_axis_constraints(first_node, true, constraints, gaps);
                            let (second_min, second_fixed, second_max, second_can_grow) =
                                self.subtree_axis_constraints(second_node, true, constraints, gaps);
                            let solved = solve_axis_lengths(
                                &[
                                    AxisConstraints {
                                        min: first_min,
                                        fixed: first_fixed,
                                        max: first_max,
                                        weight: (*ratio as f64).max(0.0),
                                        can_grow: first_can_grow,
                                    },
                                    AxisConstraints {
                                        min: second_min,
                                        fixed: second_fixed,
                                        max: second_max,
                                        weight: (1.0 - *ratio as f64).max(0.0),
                                        can_grow: second_can_grow,
                                    },
                                ],
                                available,
                            );
                            (
                                solved.first().copied().unwrap_or(available * (*ratio as f64)),
                                solved.get(1).copied().unwrap_or(0.0),
                            )
                        } else {
                            let first_w_f = available * (*ratio as f64);
                            let first_w = first_w_f.max(0.0);
                            let second_w = (available - first_w).max(0.0);
                            (first_w, second_w)
                        };
                    let r1 = CGRect::new(rect.origin, CGSize::new(first_w, rect.size.height));
                    let r2 = CGRect::new(
                        CGPoint::new(rect.origin.x + first_w + gap, rect.origin.y),
                        CGSize::new(second_w, rect.size.height),
                    );
                    let mut it = node.children(&self.tree.map);
                    if let Some(first) = it.next() {
                        self.calculate_layout_recursive(
                            first,
                            r1,
                            screen,
                            constraints,
                            gaps,
                            out,
                            nodes,
                        );
                    }
                    if let Some(second) = it.next() {
                        self.calculate_layout_recursive(
                            second,
                            r2,
                            screen,
                            constraints,
                            gaps,
                            out,
                            nodes,
                        );
                    }
                }
                Orientation::Vertical => {
                    let gap = gaps.inner.vertical as f64;
                    let total = rect.size.height;
                    let available = (total - gap).max(0.0);
                    let mut it = node.children(&self.tree.map);
                    let first = it.next();
                    let second = it.next();
                    let (first_h, second_h) =
                        if let (Some(first_node), Some(second_node)) = (first, second) {
                            let (first_min, first_fixed, first_max, first_can_grow) =
                                self.subtree_axis_constraints(first_node, false, constraints, gaps);
                            let (second_min, second_fixed, second_max, second_can_grow) = self
                                .subtree_axis_constraints(second_node, false, constraints, gaps);
                            let solved = solve_axis_lengths(
                                &[
                                    AxisConstraints {
                                        min: first_min,
                                        fixed: first_fixed,
                                        max: first_max,
                                        weight: (*ratio as f64).max(0.0),
                                        can_grow: first_can_grow,
                                    },
                                    AxisConstraints {
                                        min: second_min,
                                        fixed: second_fixed,
                                        max: second_max,
                                        weight: (1.0 - *ratio as f64).max(0.0),
                                        can_grow: second_can_grow,
                                    },
                                ],
                                available,
                            );
                            (
                                solved.first().copied().unwrap_or(available * (*ratio as f64)),
                                solved.get(1).copied().unwrap_or(0.0),
                            )
                        } else {
                            let first_h_f = available * (*ratio as f64);
                            let first_h = first_h_f.max(0.0);
                            let second_h = (available - first_h).max(0.0);
                            (first_h, second_h)
                        };
                    let r1 = CGRect::new(rect.origin, CGSize::new(rect.size.width, first_h));
                    let r2 = CGRect::new(
                        CGPoint::new(rect.origin.x, rect.origin.y + first_h + gap),
                        CGSize::new(rect.size.width, second_h),
                    );
                    let mut it = node.children(&self.tree.map);
                    if let Some(first) = it.next() {
                        self.calculate_layout_recursive(
                            first,
                            r1,
                            screen,
                            constraints,
                            gaps,
                            out,
                            nodes,
                        );
                    }
                    if let Some(second) = it.next() {
                        self.calculate_layout_recursive(
                            second,
                            r2,
                            screen,
                            constraints,
                            gaps,
                            out,
                            nodes,
                        );
                    }
                }
            },
            None => {}
        }
    }

    fn subtree_axis_constraints(
        &self,
        node: NodeId,
        horizontal: bool,
        constraints: &HashMap<WindowId, WindowLayoutConstraints>,
        gaps: &crate::common::config::GapSettings,
    ) -> (f64, Option<f64>, Option<f64>, bool) {
        match self.kind.get(node) {
            Some(NodeKind::Leaf { window, .. }) => {
                if let Some(wid) = window {
                    if let Some(c) = constraints.get(wid).copied() {
                        let c = c.normalized();
                        return (
                            c.min_for_axis(horizontal),
                            c.fixed_for_axis(horizontal),
                            (c.max_for_axis(horizontal) > 0.0).then(|| c.max_for_axis(horizontal)),
                            c.resizable_for_axis(horizontal),
                        );
                    }
                }
                (0.0, None, None, true)
            }
            Some(NodeKind::Split { orientation, .. }) => {
                let children: Vec<_> = node.children(&self.tree.map).collect();
                if children.is_empty() {
                    return (0.0, None, None, true);
                }
                let axis_aligned = *orientation
                    == if horizontal {
                        Orientation::Horizontal
                    } else {
                        Orientation::Vertical
                    };
                let inner_gap = if horizontal {
                    gaps.inner.horizontal
                } else {
                    gaps.inner.vertical
                };
                let mut mins = Vec::with_capacity(children.len());
                let mut fixed_parts = Vec::with_capacity(children.len());
                let mut max_parts = Vec::with_capacity(children.len());
                let mut any_grow = false;
                for child in children {
                    let (min, fixed, max, can_grow) =
                        self.subtree_axis_constraints(child, horizontal, constraints, gaps);
                    mins.push(min.max(0.0));
                    fixed_parts.push(fixed.map(|v| v.max(0.0)));
                    max_parts.push(max.map(|v| v.max(0.0)));
                    any_grow |= can_grow;
                }
                if axis_aligned {
                    let gap_total = inner_gap * (fixed_parts.len().saturating_sub(1) as f64);
                    let min_total = mins.iter().sum::<f64>() + gap_total;
                    let fixed_total = fixed_parts
                        .iter()
                        .copied()
                        .try_fold(0.0, |acc, part| part.map(|p| acc + p));
                    let max_total =
                        max_parts.iter().copied().try_fold(0.0, |acc, part| part.map(|p| acc + p));
                    (
                        min_total,
                        fixed_total.map(|v| v + gap_total),
                        max_total.map(|v| v + gap_total),
                        any_grow,
                    )
                } else {
                    let min_max = mins
                        .into_iter()
                        .fold(0.0, |acc, value| if value > acc { value } else { acc });
                    let fixed_max = fixed_parts.into_iter().try_fold(0.0, |acc, part| match part {
                        Some(value) => Some(if value > acc { value } else { acc }),
                        None => None,
                    });
                    (min_max, fixed_max, None, any_grow)
                }
            }
            None => (0.0, None, None, true),
        }
    }

    fn selection_window(&self, state: &LayoutState) -> Option<WindowId> {
        let sel = self.tree.data.selection.current_selection(state.root);
        match self.kind.get(sel) {
            Some(NodeKind::Leaf { window, .. }) => *window,
            _ => None,
        }
    }
}

#[derive(Default, Serialize, Deserialize, Debug)]
struct Components {
    selection: Selection,
}

impl crate::model::tree::Observer for Components {
    fn added_to_forest(&mut self, map: &NodeMap, node: NodeId) {
        self.selection.handle_event(map, TreeEvent::AddedToForest(node))
    }

    fn added_to_parent(&mut self, map: &NodeMap, node: NodeId) {
        self.selection.handle_event(map, TreeEvent::AddedToParent(node))
    }

    fn removing_from_parent(&mut self, map: &NodeMap, node: NodeId) {
        self.selection.handle_event(map, TreeEvent::RemovingFromParent(node))
    }

    fn removed_child(_tree: &mut Tree<Self>, _parent: NodeId) {}

    fn removed_from_forest(&mut self, map: &NodeMap, node: NodeId) {
        self.selection.handle_event(map, TreeEvent::RemovedFromForest(node))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(idx: u32) -> WindowId { WindowId::new(1, idx) }

    /// A leaf the index no longer points at is invisible to every removal,
    /// so the next insert of the same window used to leave two leaves: one
    /// live, one a ghost that kept its share of the screen.
    #[test]
    fn inserting_a_window_retires_a_leaf_the_index_lost() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        system.add_window_after_selection(layout, w(1));
        system.add_window_after_selection(layout, w(2));
        system.window_to_node.remove(&w(2));

        system.add_window_after_selection(layout, w(2));
        assert_eq!(system.leaves_holding(w(2)).len(), 1);
        assert_eq!(system.windows_for_app(layout, 1), vec![w(1), w(2)]);

        system.window_to_node.remove(&w(2));
        assert!(system.insert_window_next_to(layout, w(1), Direction::Left, w(2)));
        assert_eq!(system.leaves_holding(w(2)).len(), 1);
        assert_eq!(system.windows_for_app(layout, 1).len(), 2);
    }

    /// A removal takes out a leaf the index has lost too: left in, it is
    /// laid out with the rest, and its frame writes fight the tree the
    /// window has moved to.
    #[test]
    fn removing_a_window_retires_a_leaf_the_index_lost() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        system.add_window_after_selection(layout, w(1));
        system.add_window_after_selection(layout, w(2));
        system.window_to_node.remove(&w(2));

        system.remove_window(w(2));
        assert!(system.leaves_holding(w(2)).is_empty());
        assert_eq!(system.windows_for_app(layout, 1), vec![w(1)]);
    }

    /// Replacing an identity onto a window that already has a leaf keeps
    /// one leaf, not the old one as a ghost.
    #[test]
    fn replacing_onto_a_window_with_a_leaf_keeps_one_leaf() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        system.add_window_after_selection(layout, w(1));
        system.add_window_after_selection(layout, w(2));

        system.replace_window(w(1), w(2));
        assert_eq!(system.leaves_holding(w(2)).len(), 1);
        assert_eq!(system.windows_for_app(layout, 1), vec![w(2)]);
    }

    #[test]
    fn window_in_direction_prefers_leftmost_when_moving_right() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        system.add_window_after_selection(layout, w(1));
        system.add_window_after_selection(layout, w(2));

        assert_eq!(system.window_in_direction(layout, Direction::Right), Some(w(1)));
        assert_eq!(system.window_in_direction(layout, Direction::Left), Some(w(2)));
    }

    #[test]
    fn move_focus_raises_only_the_focus_target() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        system.add_window_after_selection(layout, w(1));
        system.add_window_after_selection(layout, w(2));
        system.add_window_after_selection(layout, w(3));

        let (focus, raise_windows) = system.move_focus(layout, Direction::Left);

        assert_eq!(focus, Some(w(1)));
        assert_eq!(
            raise_windows,
            vec![w(1)],
            "BSP windows never overlap; raising the other visible windows fronts every app in turn"
        );
    }

    #[test]
    fn window_in_direction_prefers_top_for_down_direction_after_orientation_toggle() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        system.add_window_after_selection(layout, w(1));
        system.add_window_after_selection(layout, w(2));
        system.toggle_tile_orientation(layout);

        assert_eq!(system.window_in_direction(layout, Direction::Down), Some(w(1)));
        assert_eq!(system.window_in_direction(layout, Direction::Up), Some(w(2)));
    }

    /// Tree shape as `draw_tree` renders it: the split line, then the leaves in
    /// child order. Rotation and mirroring are defined on that order, so this
    /// is what the assertions below compare.
    fn shape(system: &BspLayoutSystem, layout: LayoutId) -> Vec<String> {
        system.draw_tree(layout).lines().map(|line| line.trim().to_string()).collect()
    }

    /// Three windows in a row, laid out on `screen` with no gaps, returned
    /// left to right as (window, frame).
    fn three_in_a_row(screen: CGRect) -> (BspLayoutSystem, LayoutId, Vec<(WindowId, CGRect)>) {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        for idx in 1..=3 {
            system.add_window_after_selection(layout, w(idx));
        }
        // Whatever shape adding produced, make the splits all horizontal so
        // the windows sit side by side.
        for node in system.nodes_in_layout(layout) {
            if let Some(NodeKind::Split { orientation, .. }) = system.kind.get_mut(node) {
                *orientation = Orientation::Horizontal;
            }
        }
        system.rebalance(layout);
        let frames = row(&system, layout, screen);
        (system, layout, frames)
    }

    fn row(system: &BspLayoutSystem, layout: LayoutId, screen: CGRect) -> Vec<(WindowId, CGRect)> {
        let gaps = crate::common::config::GapSettings::default();
        let mut frames = system.calculate_layout(
            layout,
            screen,
            0.0,
            &HashMap::default(),
            &gaps,
            0.0,
            Default::default(),
            Default::default(),
        );
        frames.sort_by(|a, b| a.1.origin.x.partial_cmp(&b.1.origin.x).unwrap());
        frames
    }

    #[test]
    fn balance_gives_a_row_of_three_equal_thirds() {
        let screen = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1200.0, 800.0));
        let (_, _, frames) = three_in_a_row(screen);
        let widths: Vec<f64> = frames.iter().map(|(_, f)| f.size.width).collect();
        for width in &widths {
            assert!(
                (width - widths[0]).abs() < 1.0,
                "expected equal widths, got {widths:?}"
            );
        }
    }

    #[test]
    fn dragging_the_left_edge_of_the_middle_window_moves_only_that_boundary() {
        let screen = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1200.0, 800.0));
        let (mut system, layout, before) = three_in_a_row(screen);
        let gaps = crate::common::config::GapSettings::default();
        let (middle, old) = before[1];

        // Left edge 100 to the left, right edge where it was.
        let new = CGRect::new(
            CGPoint::new(old.origin.x - 100.0, old.origin.y),
            CGSize::new(old.size.width + 100.0, old.size.height),
        );
        system.on_window_resized(layout, middle, old, new, screen, &gaps);

        let after = row(&system, layout, screen);
        let find =
            |frames: &[(WindowId, CGRect)], wid| frames.iter().find(|(w, _)| *w == wid).unwrap().1;
        let middle_after = find(&after, middle);
        assert!(
            (middle_after.origin.x - new.origin.x).abs() < 1.0,
            "left edge should follow the drag: {middle_after:?} vs {new:?}"
        );
        assert!(
            (middle_after.max().x - old.max().x).abs() < 1.0,
            "right edge must stay put: {middle_after:?} vs {old:?}"
        );
        let (right, right_before) = before[2];
        let right_after = find(&after, right);
        assert!(
            (right_after.size.width - right_before.size.width).abs() < 1.0,
            "the window on the far side must not change: {right_after:?} vs {right_before:?}"
        );
        let (left, left_before) = before[0];
        assert!(
            (find(&after, left).size.width - (left_before.size.width - 100.0)).abs() < 1.0,
            "the neighbour across the boundary gives up the space"
        );
    }

    #[test]
    fn dragging_the_right_edge_of_the_middle_window_moves_only_that_boundary() {
        let screen = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1200.0, 800.0));
        let (mut system, layout, before) = three_in_a_row(screen);
        let gaps = crate::common::config::GapSettings::default();
        let (middle, old) = before[1];
        let new = CGRect::new(old.origin, CGSize::new(old.size.width + 100.0, old.size.height));
        system.on_window_resized(layout, middle, old, new, screen, &gaps);

        let after = row(&system, layout, screen);
        let find =
            |frames: &[(WindowId, CGRect)], wid| frames.iter().find(|(w, _)| *w == wid).unwrap().1;
        let middle_after = find(&after, middle);
        assert!(
            (middle_after.origin.x - old.origin.x).abs() < 1.0,
            "left edge stays: {middle_after:?}"
        );
        assert!(
            (middle_after.max().x - new.max().x).abs() < 1.0,
            "right edge follows: {middle_after:?}"
        );
        assert!((find(&after, before[0].0).size.width - before[0].1.size.width).abs() < 1.0);
    }

    fn two_window_layout() -> (BspLayoutSystem, LayoutId) {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        system.add_window_after_selection(layout, w(1));
        system.add_window_after_selection(layout, w(2));
        (system, layout)
    }

    #[test]
    fn re_adding_a_window_moves_it_instead_of_duplicating_its_leaf() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        system.add_window_after_selection(layout, w(1));
        system.add_window_after_selection(layout, w(2));

        // This is what a floating window being toggled back to tiled does, and
        // what a rule re-floating it used to leave possible: the window is
        // still in the tree when it is added again. Live trees were observed
        // holding five leaves for two real windows, so a newly tiled window got
        // a fifth of the screen instead of a half.
        system.add_window_after_selection(layout, w(1));
        system.add_window_after_selection(layout, w(1));

        let leaves = system.draw_tree(layout).lines().filter(|l| l.contains("Leaf")).count();
        assert_eq!(leaves, 2, "one leaf per window, however often it is re-added");
        assert_eq!(system.windows_for_app(layout, w(1).pid).len(), 2);
    }

    #[test]
    fn inserting_beside_a_target_splits_it_without_duplicating_the_window() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        system.add_window_after_selection(layout, w(1));
        system.add_window_after_selection(layout, w(2));

        // Drop w(2) below w(1): the tree must split w(1) vertically and hold
        // one leaf each, not gain a third from the window it already had.
        assert!(system.insert_window_next_to(layout, w(1), Direction::Down, w(2)));

        let tree = system.draw_tree(layout);
        let leaves = tree.lines().filter(|line| line.contains("Leaf")).count();
        assert_eq!(leaves, 2, "one leaf per window after inserting:\n{tree}");
        assert!(
            tree.contains("Vertical"),
            "a downward drop splits vertically:\n{tree}"
        );
        assert_eq!(system.windows_for_app(layout, w(1).pid).len(), 2);

        // A window cannot be dropped on itself.
        assert!(!system.insert_window_next_to(layout, w(1), Direction::Left, w(1)));

        // Dropping onto a window this tree does not hold must change nothing.
        // Removing the dragged window before discovering that would leave it in
        // no tree at all: untiled, unarranged, and stranded wherever the drag
        // ended.
        let before = system.draw_tree(layout);
        assert!(!system.insert_window_next_to(layout, w(99), Direction::Left, w(2)));
        assert_eq!(
            system.draw_tree(layout),
            before,
            "a refused insert must leave the tree untouched"
        );
        assert!(system.windows_for_app(layout, w(2).pid).contains(&w(2)));
    }

    #[test]
    fn balance_shares_each_split_by_the_windows_on_each_side() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        for id in 1..=4 {
            system.add_window_after_selection(layout, w(id));
        }

        // Skew a couple of splits away from even.
        system.resize_selection_by(layout, 0.3, ResizeOrientation::Horizontal);
        system.resize_selection_by(layout, 0.25, ResizeOrientation::Vertical);
        let skewed = system
            .draw_tree(layout)
            .lines()
            .filter(|line| line.contains("Split") && !line.contains("0.50"))
            .count();
        assert!(skewed > 0, "expected the resizes to skew at least one split");

        system.rebalance(layout);

        // Even means each split shares by how many windows sit on each side
        // along its axis, as yabai's `window_node_equalize` does — not every
        // split a half. The tree is `1 | (2 / (3 | 4))`: the root has one
        // window on the left and two side by side on the right, so it splits
        // a third to two thirds; the others are one against one.
        let ratios: Vec<String> = system
            .draw_tree(layout)
            .lines()
            .filter(|line| line.contains("Split"))
            .map(|line| line.trim().to_string())
            .collect();
        assert_eq!(ratios, [
            "Split Horizontal 0.33",
            "Split Vertical 0.50",
            "Split Horizontal 0.50"
        ]);
    }

    #[test]
    fn mirror_reverses_only_the_matching_axis() {
        use rift_protocol::MirrorAxis;

        let (mut system, layout) = two_window_layout();
        let before = shape(&system, layout);
        assert!(before[0].starts_with("Split Horizontal"));

        // Mirroring across x reorders vertical splits, of which there are none.
        system.mirror(layout, MirrorAxis::X);
        assert_eq!(
            shape(&system, layout),
            before,
            "x mirror must not touch a horizontal split"
        );

        // Mirroring across y reverses it.
        system.mirror(layout, MirrorAxis::Y);
        let after = shape(&system, layout);
        assert!(
            after[0].starts_with("Split Horizontal"),
            "mirroring must not turn the split"
        );
        assert_eq!(after[1], before[2]);
        assert_eq!(after[2], before[1]);
    }

    #[test]
    fn rotate_90_turns_the_split_and_reverses_it() {
        use rift_protocol::RotateDegrees;

        let (mut system, layout) = two_window_layout();
        let before = shape(&system, layout);

        system.rotate(layout, RotateDegrees::Ninety);

        let after = shape(&system, layout);
        assert!(
            after[0].starts_with("Split Vertical"),
            "a quarter turn changes the axis"
        );
        assert_eq!(after[1], before[2], "and reverses the children with it");
        assert_eq!(after[2], before[1]);
    }

    #[test]
    fn rotate_180_reverses_without_turning() {
        use rift_protocol::RotateDegrees;

        let (mut system, layout) = two_window_layout();
        let before = shape(&system, layout);

        system.rotate(layout, RotateDegrees::OneEighty);

        let after = shape(&system, layout);
        assert!(
            after[0].starts_with("Split Horizontal"),
            "a half turn keeps the axis"
        );
        assert_eq!(after[1], before[2]);
        assert_eq!(after[2], before[1]);
    }

    #[test]
    fn opposite_quarter_turns_restore_the_original_tree() {
        use rift_protocol::RotateDegrees;

        let (mut system, layout) = two_window_layout();
        let before = shape(&system, layout);

        system.rotate(layout, RotateDegrees::Ninety);
        system.rotate(layout, RotateDegrees::TwoSeventy);

        assert_eq!(shape(&system, layout), before);
    }

    #[test]
    fn fibonacci_spiral_alternates_split_orientation() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();

        // Add first window - it takes the full layout
        system.add_window_after_selection(layout, w(1));

        // Add second window - should split horizontally (depth 0)
        system.add_window_after_selection(layout, w(2));
        let tree = system.draw_tree(layout);
        assert!(
            tree.contains("Horizontal"),
            "Second window should create horizontal split at depth 0"
        );

        // Add third window - should split vertically (depth 1)
        system.add_window_after_selection(layout, w(3));
        let tree = system.draw_tree(layout);
        let horizontal_count = tree.matches("Horizontal").count();
        let vertical_count = tree.matches("Vertical").count();
        assert_eq!(horizontal_count, 1, "Should have 1 horizontal split");
        assert_eq!(vertical_count, 1, "Should have 1 vertical split");

        // Add fourth window - should split horizontally (depth 2)
        system.add_window_after_selection(layout, w(4));
        let tree = system.draw_tree(layout);
        let horizontal_count = tree.matches("Horizontal").count();
        let vertical_count = tree.matches("Vertical").count();
        assert_eq!(horizontal_count, 2, "Should have 2 horizontal splits");
        assert_eq!(vertical_count, 1, "Should have 1 vertical split");

        // Add fifth window - should split vertically (depth 3)
        system.add_window_after_selection(layout, w(5));
        let tree = system.draw_tree(layout);
        let horizontal_count = tree.matches("Horizontal").count();
        let vertical_count = tree.matches("Vertical").count();
        assert_eq!(horizontal_count, 2, "Should have 2 horizontal splits");
        assert_eq!(vertical_count, 2, "Should have 2 vertical splits");
    }

    #[test]
    fn max_only_width_cap_reclaims_space_for_sibling() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();

        let w1 = w(101);
        let w2 = w(102);
        system.add_window_after_selection(layout, w1);
        system.add_window_after_selection(layout, w2);

        let mut constraints = HashMap::default();
        constraints.insert(
            w1,
            WindowLayoutConstraints {
                is_resizable: true,
                locked_width: 0.0,
                locked_height: 0.0,
                min_width: 0.0,
                min_height: 0.0,
                max_width: 600.0,
                max_height: 0.0,
            }
            .normalized(),
        );

        let screen = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1600.0, 900.0));
        let frames: HashMap<WindowId, CGRect> = system
            .calculate_layout(
                layout,
                screen,
                0.0,
                &constraints,
                &Default::default(),
                0.0,
                Default::default(),
                Default::default(),
            )
            .into_iter()
            .collect();

        let f1 = frames.get(&w1).copied().expect("w1 frame missing");
        let f2 = frames.get(&w2).copied().expect("w2 frame missing");
        assert!((f1.size.width - 600.0).abs() < 1.0);
        assert!((f2.size.width - 1000.0).abs() < 1.0);
        assert!((f2.origin.x - 600.0).abs() < 1.0);
    }

    /// Dragging the outer edge of a window at the side of the screen used to
    /// do nothing at all. No split owns that edge -- the rightmost window's
    /// right edge is the screen's, not a boundary between two windows -- so
    /// the walk up the tree found nothing to move and returned silently, in
    /// both directions. Whether a drag worked came down to which half of the
    /// window the press landed in, which is what made it look intermittent.
    #[test]
    fn dragging_the_screen_side_edge_resizes_through_the_other_boundary() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        let w1 = w(101);
        let w2 = w(102);
        system.add_window_after_selection(layout, w1);
        system.add_window_after_selection(layout, w2);

        let screen = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1600.0, 900.0));
        let gaps = crate::common::config::GapSettings::default();
        let layout_of = |system: &mut BspLayoutSystem| -> HashMap<WindowId, CGRect> {
            system
                .calculate_layout(
                    layout,
                    screen,
                    0.0,
                    &Default::default(),
                    &Default::default(),
                    0.0,
                    Default::default(),
                    Default::default(),
                )
                .into_iter()
                .collect()
        };

        let before = layout_of(&mut system);
        let right = *before.get(&w2).expect("w2 frame missing");
        assert!(
            (right.max().x - screen.max().x).abs() < 1.0,
            "w2 must be the window against the right of the screen, or this \
             tests nothing"
        );

        // Drag w2's right edge inwards: its origin stays, its width shrinks.
        // That edge cannot move, so the split on its left has to instead.
        let asked = CGRect::new(right.origin, CGSize::new(400.0, right.size.height));
        system.on_window_resized(layout, w2, right, asked, screen, &gaps);

        let after = layout_of(&mut system);
        let now = *after.get(&w2).expect("w2 frame missing");
        assert!(
            (now.size.width - 400.0).abs() < 2.0,
            "w2 should be the width that was asked for, got {}",
            now.size.width
        );
        assert!(
            (now.max().x - screen.max().x).abs() < 1.0,
            "and should still sit against the right of the screen"
        );
        let neighbour = *after.get(&w1).expect("w1 frame missing");
        assert!(
            neighbour.size.width > before.get(&w1).unwrap().size.width,
            "the neighbour gives up the space instead"
        );
    }

    /// A drag is a run of resizes, not one. Each carries the *previous
    /// target* as its old frame, and while the immovable edge is the one being
    /// dragged none of those targets is ever realised -- so anchoring the
    /// replacement boundary to the old frame walks the anchor along with the
    /// drag, and the window resizes against the direction of the gesture.
    #[test]
    fn dragging_the_screen_side_edge_follows_the_gesture_over_a_whole_drag() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        let w1 = w(101);
        let w2 = w(102);
        system.add_window_after_selection(layout, w1);
        system.add_window_after_selection(layout, w2);

        let screen = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1600.0, 900.0));
        let gaps = crate::common::config::GapSettings::default();
        let widths = |system: &mut BspLayoutSystem| -> (f64, f64) {
            let frames: HashMap<WindowId, CGRect> = system
                .calculate_layout(
                    layout,
                    screen,
                    0.0,
                    &Default::default(),
                    &Default::default(),
                    0.0,
                    Default::default(),
                    Default::default(),
                )
                .into_iter()
                .collect();
            (
                frames.get(&w1).expect("w1").size.width,
                frames.get(&w2).expect("w2").size.width,
            )
        };

        let start = widths(&mut system).1;
        // The gesture: the pointer walks left, so the window it is pulling on
        // gets narrower, step by step. `old_frame` is the previous target, as
        // the reactor sends it.
        let origin = CGPoint::new(screen.max().x - start, 0.0);
        let mut previous = CGRect::new(origin, CGSize::new(start, 900.0));
        let mut seen = vec![start];
        for step in 1..=6 {
            let want = start - (step as f64) * 60.0;
            let asked = CGRect::new(previous.origin, CGSize::new(want, 900.0));
            system.on_window_resized(layout, w2, previous, asked, screen, &gaps);
            seen.push(widths(&mut system).1);
            previous = asked;
        }
        for pair in seen.windows(2) {
            assert!(
                pair[1] < pair[0] + 1.0,
                "the window must not grow while the drag is shrinking it: {seen:?}"
            );
        }
        assert!(
            seen.last().unwrap() < &(start - 200.0),
            "and must actually follow the gesture: {seen:?}"
        );
    }

    /// Holding still must hold still. The fallback anchored to the window's
    /// own rect, which its own output moves, so every step measured from a
    /// shifted anchor and pushed further: on the host the window ran from
    /// 707px to 982px while the drag asked for a width that changed by less
    /// than a pixel a step. Anchoring to the container, which the resize
    /// cannot move, is what makes repeating the same request a no-op.
    #[test]
    fn asking_for_the_same_width_again_does_not_move_the_window() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();
        let w1 = w(101);
        let w2 = w(102);
        system.add_window_after_selection(layout, w1);
        system.add_window_after_selection(layout, w2);

        let screen = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1600.0, 900.0));
        let gaps = crate::common::config::GapSettings::default();
        // The neighbour has a minimum, as Zen did at 500px. That is what makes
        // the tree's geometry and the frames actually applied diverge -- the
        // tree keeps handing the dragged window more, the constraint keeps
        // taking it back -- and the divergence is what the anchor drifts on.
        // Without it the two agree and the runaway cannot appear.
        let mut constraints = HashMap::default();
        constraints.insert(
            w1,
            WindowLayoutConstraints {
                is_resizable: true,
                min_width: 700.0,
                ..WindowLayoutConstraints::default()
            }
            .normalized(),
        );
        let width_of = |system: &mut BspLayoutSystem, wid: WindowId| -> f64 {
            system
                .calculate_layout(
                    layout,
                    screen,
                    0.0,
                    &constraints,
                    &Default::default(),
                    0.0,
                    Default::default(),
                    Default::default(),
                )
                .into_iter()
                .find(|(w, _)| *w == wid)
                .expect("frame missing")
                .1
                .size
                .width
        };

        let start = width_of(&mut system, w2);
        let origin = CGPoint::new(screen.max().x - start, 0.0);
        // One step to a new width, then the same width asked for over and
        // over -- a pointer held still while the app keeps reporting.
        let want = start - 150.0;
        let mut previous = CGRect::new(origin, CGSize::new(start, 900.0));
        let asked = CGRect::new(origin, CGSize::new(want, 900.0));
        system.on_window_resized(layout, w2, previous, asked, screen, &gaps);
        previous = asked;
        let settled = width_of(&mut system, w2);

        for _ in 0..8 {
            system.on_window_resized(layout, w2, previous, asked, screen, &gaps);
        }
        let after = width_of(&mut system, w2);
        assert!(
            (after - settled).abs() < 2.0,
            "repeating the same request must not walk the window: settled at \
             {settled}, ended at {after}"
        );
    }

    #[test]
    fn non_binding_window_minimum_keeps_half_split_centered() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();

        let browser = w(106);
        let finder = w(107);
        system.add_window_after_selection(layout, browser);
        system.add_window_after_selection(layout, finder);

        let mut constraints = HashMap::default();
        constraints.insert(
            finder,
            WindowLayoutConstraints {
                is_resizable: true,
                min_width: 400.0,
                ..Default::default()
            }
            .normalized(),
        );

        let screen = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1200.0, 900.0));
        let frames: HashMap<WindowId, CGRect> = system
            .calculate_layout(
                layout,
                screen,
                0.0,
                &constraints,
                &Default::default(),
                0.0,
                Default::default(),
                Default::default(),
            )
            .into_iter()
            .collect();

        let browser_frame = frames.get(&browser).copied().expect("browser frame missing");
        let finder_frame = frames.get(&finder).copied().expect("Finder frame missing");
        assert!((browser_frame.size.width - 600.0).abs() < 1.0);
        assert!((finder_frame.size.width - 600.0).abs() < 1.0);
        assert!((finder_frame.origin.x - 600.0).abs() < 1.0);
    }

    #[test]
    fn max_only_height_does_not_cap_cross_axis_subtree() {
        let mut system = BspLayoutSystem::default();
        let layout = system.create_layout();

        let constrained = w(103);
        let unconstrained = w(104);
        let sibling = w(105);
        system.add_window_after_selection(layout, constrained);
        system.split_selection(layout, LayoutKind::Vertical);
        system.add_window_after_selection(layout, sibling);
        assert!(system.select_window(layout, constrained));
        system.split_selection(layout, LayoutKind::Horizontal);
        system.add_window_after_selection(layout, unconstrained);

        let mut constraints = HashMap::default();
        constraints.insert(
            constrained,
            WindowLayoutConstraints {
                is_resizable: true,
                locked_width: 0.0,
                locked_height: 0.0,
                min_width: 0.0,
                min_height: 0.0,
                max_width: 0.0,
                max_height: 200.0,
            }
            .normalized(),
        );

        let screen = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1200.0, 800.0));
        let frames: HashMap<WindowId, CGRect> = system
            .calculate_layout(
                layout,
                screen,
                0.0,
                &constraints,
                &Default::default(),
                0.0,
                Default::default(),
                Default::default(),
            )
            .into_iter()
            .collect();

        let constrained_frame = frames.get(&constrained).copied().expect("constrained frame");
        let unconstrained_frame = frames.get(&unconstrained).copied().expect("unconstrained frame");
        let sibling_frame = frames.get(&sibling).copied().expect("sibling frame");

        assert!(
            constrained_frame.size.height <= 201.0,
            "constrained leaf should still honor its own max height"
        );
        assert!(
            unconstrained_frame.size.height >= 399.0,
            "unconstrained child in the orthogonal subtree should keep the subtree's full height"
        );
        assert!(
            (sibling_frame.size.height - 400.0).abs() < 1.0,
            "orthogonal max-only constraint should not change the parent split allocation"
        );
    }
}

impl BspLayoutSystem {
    /// The rectangle every node of `layout` occupies, split containers
    /// included, as the layout pass would place them.
    fn node_rects(
        &self,
        layout: LayoutId,
        screen: CGRect,
        gaps: &crate::common::config::GapSettings,
    ) -> HashMap<NodeId, CGRect> {
        let mut nodes = Vec::new();
        if let Some(state) = self.layouts.get(layout).copied() {
            let rect = Self::apply_outer_gaps(screen, gaps);
            let mut out = Vec::new();
            self.calculate_layout_recursive(
                state.root,
                rect,
                screen,
                &HashMap::default(),
                gaps,
                &mut out,
                &mut nodes,
            );
        }
        nodes.into_iter().collect()
    }

    /// Moves the boundary on one side of `leaf` to `position`, an absolute
    /// coordinate along the split axis.
    ///
    /// The boundary belongs to the nearest ancestor split of that orientation
    /// where the subtree containing `leaf` sits on the near side: for a left
    /// or top edge that is the split where it is the second child, for a
    /// right or bottom edge the one where it is the first. A window in the
    /// middle of a row has each of its edges owned by a different split, and
    /// resizing by size alone always found the same one, so dragging its left
    /// edge moved its right edge instead.
    /// Move the boundary on one side of `leaf` to an absolute position.
    ///
    /// Returns whether a boundary was found to move. A window at the edge of
    /// the screen has no split owning its outer side -- the rightmost window's
    /// right edge is the screen's, not a boundary between two windows -- and
    /// the walk below then reaches the root having found nothing. That is not
    /// an error; it is the caller's cue to express the same size change
    /// through the other side, which for such a window is the only side that
    /// can move.
    #[must_use]
    fn move_edge_to(
        &mut self,
        rects: &HashMap<NodeId, CGRect>,
        leaf: NodeId,
        horizontal: bool,
        near_side: bool,
        position: f64,
        gap: f64,
    ) -> bool {
        let mut current = leaf;
        while let Some(parent) = current.parent(&self.tree.map) {
            let is_first = Some(current) == parent.first_child(&self.tree.map);
            let owns_edge = matches!(
                self.kind.get(parent),
                Some(NodeKind::Split { orientation, .. })
                    if (*orientation == Orientation::Horizontal) == horizontal
            ) && is_first != near_side;
            if !owns_edge {
                current = parent;
                continue;
            }
            let Some(rect) = rects.get(&parent) else {
                return false;
            };
            let (origin, total) = if horizontal {
                (rect.origin.x, rect.size.width)
            } else {
                (rect.origin.y, rect.size.height)
            };
            let available = total - gap;
            if available <= 0.0 {
                return false;
            }
            // The first child runs from the container's origin to the
            // boundary; the second starts a gap after it.
            let first_len = if near_side {
                position - origin - gap
            } else {
                position - origin
            };
            let ratio = (first_len / available).clamp(0.05, 0.95) as f32;
            if let Some(NodeKind::Split { ratio: r, .. }) = self.kind.get_mut(parent) {
                *r = ratio;
            }

            // The subtree on the near side of that boundary has just been
            // given a different extent, and its own splits are ratios of it,
            // so every boundary inside it would slide along. The user dragged
            // one edge; the others stay where they were on screen, which
            // means re-deriving each ratio on the way back down to the leaf
            // from the boundary's old absolute position.
            let first_len = f64::from(ratio) * available;
            let mut interval = if near_side {
                (origin + first_len + gap, available - first_len)
            } else {
                (origin, first_len)
            };
            let mut path = vec![leaf];
            while *path.last().unwrap() != current {
                let up =
                    path.last().unwrap().parent(&self.tree.map).expect("leaf lies under current");
                path.push(up);
            }
            for pair in path.windows(2).rev() {
                let (split, child) = (pair[1], pair[0]);
                let Some(NodeKind::Split { orientation, .. }) = self.kind.get(split).cloned()
                else {
                    continue;
                };
                if (orientation == Orientation::Horizontal) != horizontal {
                    continue;
                }
                let is_first = Some(child) == split.first_child(&self.tree.map);
                let (Some(first), Some(split_rect)) = (
                    split.first_child(&self.tree.map).and_then(|n| rects.get(&n)),
                    rects.get(&split),
                ) else {
                    return false;
                };
                let (split_origin, first_old) = if horizontal {
                    (split_rect.origin.x, first.size.width)
                } else {
                    (split_rect.origin.y, first.size.height)
                };
                let boundary = split_origin + first_old;
                let available = interval.1 - gap;
                if available <= 0.0 {
                    return false;
                }
                let ratio = ((boundary - interval.0) / available).clamp(0.05, 0.95) as f32;
                if let Some(NodeKind::Split { ratio: r, .. }) = self.kind.get_mut(split) {
                    *r = ratio;
                }
                let first_len = f64::from(ratio) * available;
                interval = if is_first {
                    (interval.0, first_len)
                } else {
                    (interval.0 + first_len + gap, available - first_len)
                };
            }
            return true;
        }
        // The walk reached the root without finding a split that owns this
        // side: the window is at the edge of the screen there.
        false
    }

    /// Gives every leaf under `node` an equal share along each axis, the way
    /// yabai's `space --balance` does (`window_node_equalize`).
    ///
    /// Returns how many leaves the subtree lays side by side horizontally and
    /// vertically. A split along an axis adds its children's counts on that
    /// axis and keeps the larger on the other, and its ratio is the first
    /// child's share of the sum: three windows in a row split 1:2 at the
    /// root and 1:1 below, which is thirds. Setting every ratio to a half
    /// gave the first window half the screen and the other two a quarter.
    fn equalize(&mut self, node: NodeId) -> (f64, f64) {
        let Some(NodeKind::Split { orientation, .. }) = self.kind.get(node).cloned() else {
            return (1.0, 1.0);
        };
        let children: Vec<NodeId> = node.children(&self.tree.map).collect();
        let [first, second] = children[..] else {
            return (1.0, 1.0);
        };
        let (fx, fy) = self.equalize(first);
        let (sx, sy) = self.equalize(second);
        let (ratio, weights) = match orientation {
            Orientation::Horizontal => (fx / (fx + sx), (fx + sx, fy.max(sy))),
            Orientation::Vertical => (fy / (fy + sy), (fx.max(sx), fy + sy)),
        };
        if let Some(NodeKind::Split { ratio: r, .. }) = self.kind.get_mut(node) {
            *r = ratio as f32;
        }
        weights
    }
}

impl LayoutSystem for BspLayoutSystem {
    /// A quarter turn is a swap plus an axis flip, applied to every split.
    ///
    /// Which splits swap depends on the direction of the turn: turning
    /// clockwise reverses the horizontal splits (what was left becomes top),
    /// counter-clockwise reverses the vertical ones, and a half turn reverses
    /// everything and flips nothing.
    fn rotate(&mut self, layout: LayoutId, degrees: rift_protocol::RotateDegrees) {
        use rift_protocol::RotateDegrees;

        for node in self.nodes_in_layout(layout) {
            let Some(orientation) = self.split_orientation(node) else {
                continue;
            };
            let reverse = match degrees {
                RotateDegrees::Ninety => orientation == Orientation::Horizontal,
                RotateDegrees::TwoSeventy => orientation == Orientation::Vertical,
                RotateDegrees::OneEighty => true,
            };
            if reverse {
                self.reverse_split_children(node);
            }
            if degrees != RotateDegrees::OneEighty {
                self.flip_split_orientation(node);
            }
        }
    }

    fn mirror(&mut self, layout: LayoutId, axis: rift_protocol::MirrorAxis) {
        let target = axis.orientation();
        for node in self.nodes_in_layout(layout) {
            if self.split_orientation(node) == Some(target) {
                self.reverse_split_children(node);
            }
        }
    }

    fn create_layout(&mut self) -> LayoutId {
        let leaf = self.make_leaf(None);
        let state = LayoutState { root: leaf };
        self.layouts.insert(state)
    }

    fn contains_layout(&self, layout: LayoutId) -> bool { self.layouts.contains_key(layout) }

    /// shallow
    fn clone_layout(&mut self, layout: LayoutId) -> LayoutId {
        let mut windows = Vec::new();
        if let Some(state) = self.layouts.get(layout).copied() {
            self.collect_windows_under(state.root, &mut windows);
        }
        let new_layout = self.create_layout();
        for w in windows {
            self.add_window_after_selection(new_layout, w);
        }
        new_layout
    }

    fn remove_layout(&mut self, layout: LayoutId) {
        if let Some(state) = self.layouts.remove(layout) {
            let mut windows = Vec::new();
            self.collect_windows_under(state.root, &mut windows);
            for w in windows {
                self.unindex_window(w);
            }
            let ids: Vec<_> = state.root.traverse_preorder(&self.tree.map).collect();
            for id in ids {
                self.kind.remove(id);
            }
            state.root.remove_root(&mut self.tree);
        }
    }

    fn draw_tree(&self, layout: LayoutId) -> String {
        fn write_node(this: &BspLayoutSystem, node: NodeId, out: &mut String, indent: usize) {
            for _ in 0..indent {
                out.push_str("  ");
            }
            match this.kind.get(node) {
                Some(NodeKind::Leaf { window, .. }) => {
                    out.push_str(&format!("Leaf {:?}\n", window));
                }
                Some(NodeKind::Split { orientation, ratio }) => {
                    out.push_str(&format!("Split {:?} {:.2}\n", orientation, ratio));
                    let mut it = node.children(&this.tree.map);
                    if let Some(first) = it.next() {
                        write_node(this, first, out, indent + 1);
                    }
                    if let Some(second) = it.next() {
                        write_node(this, second, out, indent + 1);
                    }
                }
                None => {}
            }
        }
        if let Some(state) = self.layouts.get(layout).copied() {
            let mut s = String::new();
            write_node(self, state.root, &mut s, 0);
            s
        } else {
            "<empty bsp>".to_string()
        }
    }

    fn container_tree(&self, layout: LayoutId) -> rift_protocol::ContainerTreeNode {
        fn snapshot(
            system: &BspLayoutSystem,
            node: NodeId,
            selected: NodeId,
        ) -> rift_protocol::ContainerTreeNode {
            let weight = node.parent(&system.tree.map).and_then(|parent| {
                let NodeKind::Split { ratio, .. } = system.kind.get(parent)? else {
                    return None;
                };
                let first = parent.first_child(&system.tree.map);
                Some(if first == Some(node) {
                    f64::from(*ratio)
                } else {
                    1.0 - f64::from(*ratio)
                })
            });

            match system.kind.get(node) {
                Some(NodeKind::Split { orientation, .. }) => rift_protocol::ContainerTreeNode {
                    node_id: node.data().as_ffi(),
                    node_type: rift_protocol::ContainerNodeType::Container,
                    frame: Default::default(),
                    layout_kind: Some(rift_protocol::LayoutKind::from(*orientation)),
                    weight,
                    window_id: None,
                    is_selected: node == selected,
                    is_fullscreen: false,
                    is_fullscreen_within_gaps: false,
                    role: None,
                    pending_split: None,
                    children: node
                        .children(&system.tree.map)
                        .map(|child| snapshot(system, child, selected))
                        .collect(),
                },
                Some(NodeKind::Leaf {
                    window,
                    fullscreen,
                    fullscreen_within_gaps,
                    preselected,
                }) => rift_protocol::ContainerTreeNode {
                    node_id: node.data().as_ffi(),
                    node_type: if window.is_some() {
                        rift_protocol::ContainerNodeType::Window
                    } else {
                        rift_protocol::ContainerNodeType::Placeholder
                    },
                    frame: Default::default(),
                    layout_kind: None,
                    weight,
                    window_id: window.map(Into::into),
                    is_selected: node == selected,
                    is_fullscreen: *fullscreen,
                    is_fullscreen_within_gaps: *fullscreen_within_gaps,
                    role: None,
                    pending_split: preselected.map(Into::into),
                    children: Vec::new(),
                },
                None => unreachable!("BSP layout contains a node without metadata"),
            }
        }

        let state = self.layouts.get(layout).expect("unknown BSP layout");
        let selected = self.tree.data.selection.current_selection(state.root);
        snapshot(self, state.root, selected)
    }

    fn calculate_layout(
        &self,
        layout: LayoutId,
        screen: CGRect,
        _stack_offset: f64,
        constraints: &HashMap<WindowId, WindowLayoutConstraints>,
        gaps: &crate::common::config::GapSettings,
        _stack_line_thickness: f64,
        _stack_line_horiz: crate::common::config::HorizontalPlacement,
        _stack_line_vert: crate::common::config::VerticalPlacement,
    ) -> Vec<(WindowId, CGRect)> {
        let mut out = Vec::new();
        if let Some(state) = self.layouts.get(layout).copied() {
            let rect = compute_tiling_area(screen, gaps);
            let mut nodes = Vec::new();
            self.calculate_layout_recursive(
                state.root,
                rect,
                screen,
                constraints,
                gaps,
                &mut out,
                &mut nodes,
            );
        }
        out
    }

    fn selected_window(&self, layout: LayoutId) -> Option<WindowId> {
        self.layouts.get(layout).and_then(|s| self.selection_window(s))
    }

    fn all_windows_in_layout(&self, layout: LayoutId) -> Vec<WindowId> {
        let mut out = Vec::new();
        if let Some(state) = self.layouts.get(layout).copied() {
            self.collect_windows_under(state.root, &mut out);
        }
        out
    }

    fn visible_windows_in_layout(&self, layout: LayoutId) -> Vec<WindowId> {
        let mut out = Vec::new();
        if let Some(state) = self.layouts.get(layout).copied() {
            self.collect_windows_under(state.root, &mut out);
        }
        out
    }

    fn visible_windows_under_selection(&self, layout: LayoutId) -> Vec<WindowId> {
        let mut out = Vec::new();
        if let Some(sel) = self.selection_of_layout(layout) {
            if self.kind.get(sel).is_some() {
                let leaf = self.descend_to_leaf(sel);
                self.collect_windows_under(leaf, &mut out);
            }
        }

        out
    }

    fn ascend_selection(&mut self, layout: LayoutId) -> bool {
        if let Some(sel) = self.selection_of_layout(layout) {
            if self.kind.get(sel).is_none() {
                return false;
            }
            let parent_opt = sel.parent(&self.tree.map);
            if let Some(parent) = parent_opt {
                let new_sel = self.descend_to_leaf(parent);
                self.tree.data.selection.select(&self.tree.map, new_sel);
                return true;
            }
        }
        false
    }

    fn descend_selection(&mut self, layout: LayoutId) -> bool {
        if let Some(sel) = self.selection_of_layout(layout) {
            let new_sel = self.descend_to_leaf(sel);
            if new_sel != sel {
                self.tree.data.selection.select(&self.tree.map, new_sel);
                return true;
            }
        }
        false
    }

    fn move_focus(
        &mut self,
        layout: LayoutId,
        direction: Direction,
    ) -> (Option<WindowId>, Vec<WindowId>) {
        if self.visible_windows_in_layout(layout).is_empty() {
            return (None, vec![]);
        }
        let sel_snapshot = self.selection_of_layout(layout);
        let Some(current_sel) = sel_snapshot else {
            return (None, vec![]);
        };
        let current_leaf = self.descend_to_leaf(current_sel);
        let Some(next_leaf) = self.find_neighbor_leaf(current_leaf, direction) else {
            return (None, vec![]);
        };
        self.tree.data.selection.select(&self.tree.map, next_leaf);
        let focus = match self.kind.get(next_leaf) {
            Some(NodeKind::Leaf { window, .. }) => *window,
            _ => None,
        };
        // Tiled BSP windows never overlap, so only the focus target needs to be
        // raised. Raising every visible window makes the raise manager front
        // each app in turn (via make_key_window) before the target, which
        // flickers the menu bar and any focus border on every focus move. The
        // traditional engine already raises only the revealed group.
        (focus, focus.into_iter().collect())
    }

    fn window_in_direction(&self, layout: LayoutId, direction: Direction) -> Option<WindowId> {
        self.layouts
            .get(layout)
            .and_then(|state| self.window_in_direction_from(state.root, direction))
    }

    fn add_window_after_selection(&mut self, layout: LayoutId, wid: WindowId) {
        // Re-adding an existing window means "move it to the selection", so
        // retire the old leaf first. window_to_node holds one node per
        // window, so inserting without this strands the previous leaf: the
        // tree goes on rendering it and dividing space for it, while
        // nothing can reach it to take it out again. Every leaf, not only
        // the indexed one: a leaf the index has already lost is exactly the
        // one that would otherwise survive as a ghost.
        self.retire_all_leaves(wid);
        if self.layouts.get(layout).is_some() {
            if self.window_insertion_point == WindowInsertionPoint::EndOfTree {
                let root = self.layouts[layout].root;
                if let Some(leaf) = root
                    .traverse_preorder(&self.tree.map)
                    .filter(|node| matches!(self.kind.get(*node), Some(NodeKind::Leaf { .. })))
                    .last()
                {
                    self.tree.data.selection.select(&self.tree.map, leaf);
                }
                self.insert_window_at_selection(layout, wid);
                return;
            }
            // Try smart insertion first (with preselection support)
            if !self.smart_insert_window(layout, wid) {
                // Fall back to default insertion
                self.insert_window_at_selection(layout, wid);
            }
        }
    }

    fn replace_window(&mut self, from: WindowId, to: WindowId) {
        if from == to {
            return;
        }
        // `to` may already have a leaf of its own; re-pointing the index at
        // `from`'s leaf would strand it. Retired first: the removal reshapes
        // the tree, and `from`'s node is looked up after it settles.
        self.retire_all_leaves(to);
        let Some(node) = self.window_to_node.remove(&from) else {
            return;
        };
        if let Some(NodeKind::Leaf { window, .. }) = self.kind.get_mut(node)
            && *window == Some(from)
        {
            *window = Some(to);
            self.window_to_node.insert(to, node);
        }
    }

    fn remove_window(&mut self, wid: WindowId) { self.retire_all_leaves(wid); }

    fn remove_windows_for_app(&mut self, pid: pid_t) {
        let windows: Vec<_> =
            self.window_to_node.keys().copied().filter(|w| w.pid == pid).collect();
        for w in windows {
            self.remove_window(w);
        }
    }

    fn set_windows_for_app(&mut self, layout: LayoutId, pid: pid_t, desired: Vec<WindowId>) {
        let current = if let Some(state) = self.layouts.get(layout).copied() {
            let mut under: Vec<WindowId> = Vec::new();
            self.collect_windows_under(state.root, &mut under);
            under.into_iter().filter(|w| w.pid == pid).collect()
        } else {
            Vec::new()
        };
        let delta = reconcile_app_membership(pid, current, desired);
        for wid in delta.removals {
            if let Some(node) = self.node_for_window(wid)
                && let Some(NodeKind::Leaf {
                    fullscreen,
                    fullscreen_within_gaps,
                    ..
                }) = self.kind.get(node)
                && (*fullscreen || *fullscreen_within_gaps)
            {
                continue;
            }
            self.remove_window_internal(layout, wid);
        }
        for wid in delta.additions {
            self.add_window_after_selection(layout, wid);
        }
    }

    fn contains_window(&self, layout: LayoutId, wid: WindowId) -> bool {
        if let Some(node) = self.node_for_window(wid) {
            if let Some(state) = self.layouts.get(layout).copied() {
                return self.belongs_to_layout(state, node);
            }
        }
        false
    }

    fn select_window(&mut self, layout: LayoutId, wid: WindowId) -> bool {
        if let Some(node) = self.node_for_window_mut(wid) {
            if let Some(state) = self.layouts.get(layout).copied() {
                let belongs = self.belongs_to_layout(state, node);
                if belongs {
                    self.tree.data.selection.select(&self.tree.map, node);
                    return true;
                }
            }
        }
        false
    }

    fn on_window_resized(
        &mut self,
        layout: LayoutId,
        wid: WindowId,
        old_frame: CGRect,
        new_frame: CGRect,
        screen: CGRect,
        gaps: &crate::common::config::GapSettings,
    ) {
        let note = |why: &str, extra: serde_json::Value| {
            crate::sys::trace::act(
                "bsp_resize",
                &serde_json::json!({ "wid": wid.idx.get(), "why": why, "extra": extra }),
            );
        };
        let Some(node) = self.node_for_window_mut(wid) else {
            // Every screen's workspace is offered the resize; the trees that
            // do not hold the window are not worth a line.
            return;
        };
        {
            let Some(state) = self.layouts.get(layout).copied() else {
                note("no layout state", serde_json::Value::Null);
                return;
            };
            {
                if !self.belongs_to_layout(state, node) {
                    note("node is not in this layout", serde_json::Value::Null);
                    return;
                }
                let tiling = compute_tiling_area(screen, gaps);
                let mut fullscreen_transition = false;
                if let Some(NodeKind::Leaf {
                    window: _,
                    fullscreen,
                    fullscreen_within_gaps,
                    ..
                }) = self.kind.get_mut(node)
                {
                    if new_frame == screen {
                        *fullscreen = true;
                        *fullscreen_within_gaps = false;
                        fullscreen_transition = true;
                    } else if old_frame == screen {
                        *fullscreen = false;
                        fullscreen_transition = true;
                    } else {
                        if new_frame == tiling {
                            *fullscreen_within_gaps = true;
                            *fullscreen = false;
                            fullscreen_transition = true;
                        } else if old_frame == tiling {
                            *fullscreen_within_gaps = false;
                            fullscreen_transition = true;
                        }
                    }
                }

                if fullscreen_transition {
                    note("read as a fullscreen transition", serde_json::Value::Null);
                    return;
                }

                let width_changed = (new_frame.size.width - old_frame.size.width).abs() > 0.5;
                let height_changed = (new_frame.size.height - old_frame.size.height).abs() > 0.5;
                if !width_changed && !height_changed {
                    note(
                        "no change in size",
                        serde_json::json!([old_frame.size.width, new_frame.size.width]),
                    );
                    return;
                }

                // Which edges moved says which boundaries to move; the new
                // frame says where to. Working from absolute edge positions
                // rather than size deltas means a stale `old_frame` — the app
                // not having reported the last arrange yet — cannot make the
                // change accumulate.
                let moved = |a: f64, b: f64| (a - b).abs() > 0.5;
                let rects = self.node_rects(layout, screen, gaps);
                if width_changed {
                    let left = moved(new_frame.origin.x, old_frame.origin.x);
                    let right = moved(new_frame.max().x, old_frame.max().x);
                    let gap = gaps.inner.horizontal as f64;
                    note(
                        "moving a horizontal edge",
                        serde_json::json!({
                            "left": left,
                            "right": right,
                            "to": if left && !right { new_frame.origin.x } else { new_frame.max().x },
                            "screen": [screen.origin.x, screen.size.width],
                        }),
                    );
                    // Both moving is a resize about the centre; treat it as
                    // the far edge, which is what a plain size change was.
                    //
                    // Either may turn out not to exist. A window against the
                    // side of the screen has no split owning its outer edge --
                    // the rightmost window's right edge is the screen's, not a
                    // boundary between two windows -- so dragging that edge
                    // moved nothing at all, in either direction, and the
                    // gesture did nothing. Whether it did depended on which
                    // half of the window the press landed in, which is what
                    // made it look intermittent.
                    //
                    // The size the user is asking for is still reachable: it
                    // is the *other* boundary that has to move instead, to
                    // wherever leaves the window that wide. The edge that
                    // cannot move stays where it is, so that is the edge to
                    // measure the new width from.
                    let moved = if left && !right {
                        self.move_edge_to(&rects, node, true, true, new_frame.origin.x, gap)
                    } else {
                        self.move_edge_to(&rects, node, true, false, new_frame.max().x, gap)
                    };
                    if !moved {
                        // Anchored to where the layout actually has the window,
                        // not to `old_frame`. During a drag `old_frame` is the
                        // previous *target*, and while the immovable edge was
                        // being dragged none of those targets were ever
                        // realised -- so anchoring to it walked the anchor
                        // along with the drag and moved the boundary the wrong
                        // way. The edge that cannot move is where the tree says
                        // it is, and that is the fixed point the new width has
                        // to be measured from.
                        let width = new_frame.size.width;
                        let here = rects.get(&node).copied().unwrap_or(old_frame);
                        note(
                            "no split owns that edge; resizing through the other one",
                            serde_json::json!({
                                "here": [here.origin.x, here.size.width],
                                "want_width": width,
                                "to": if left && !right {
                                    here.origin.x + width
                                } else {
                                    here.max().x - width
                                },
                            }),
                        );
                        if left && !right {
                            let _ = self.move_edge_to(
                                &rects,
                                node,
                                true,
                                false,
                                here.origin.x + width,
                                gap,
                            );
                        } else {
                            let _ = self.move_edge_to(
                                &rects,
                                node,
                                true,
                                true,
                                here.max().x - width,
                                gap,
                            );
                        }
                    }
                }
                if height_changed {
                    let top = moved(new_frame.origin.y, old_frame.origin.y);
                    let bottom = moved(new_frame.max().y, old_frame.max().y);
                    let gap = gaps.inner.vertical as f64;
                    let moved = if top && !bottom {
                        self.move_edge_to(&rects, node, false, true, new_frame.origin.y, gap)
                    } else {
                        self.move_edge_to(&rects, node, false, false, new_frame.max().y, gap)
                    };
                    if !moved {
                        let height = new_frame.size.height;
                        let here = rects.get(&node).copied().unwrap_or(old_frame);
                        if top && !bottom {
                            let _ = self.move_edge_to(
                                &rects,
                                node,
                                false,
                                false,
                                here.origin.y + height,
                                gap,
                            );
                        } else {
                            let _ = self.move_edge_to(
                                &rects,
                                node,
                                false,
                                true,
                                here.max().y - height,
                                gap,
                            );
                        }
                    }
                }
            }
        }
    }

    fn move_selection(&mut self, layout: LayoutId, direction: Direction) -> bool {
        let sel_snapshot = self.selection_of_layout(layout);
        let Some(sel) = sel_snapshot else {
            return false;
        };
        let sel_leaf = self.descend_to_leaf(sel);
        let Some(neighbor_leaf) = self.find_neighbor_leaf(sel_leaf, direction) else {
            return false;
        };
        let (mut a_window, mut b_window) = (None, None);
        if let Some(NodeKind::Leaf { window, .. }) = self.kind.get_mut(sel_leaf) {
            a_window = *window;
        }
        if let Some(NodeKind::Leaf { window, .. }) = self.kind.get_mut(neighbor_leaf) {
            b_window = *window;
        }
        if a_window.is_none() && b_window.is_none() {
            return false;
        }
        if let Some(NodeKind::Leaf { window, .. }) = self.kind.get_mut(sel_leaf) {
            *window = b_window;
        }
        if let Some(NodeKind::Leaf { window, .. }) = self.kind.get_mut(neighbor_leaf) {
            *window = a_window;
        }
        if let Some(w) = a_window {
            self.index_window(w, neighbor_leaf);
        }
        if let Some(w) = b_window {
            self.index_window(w, sel_leaf);
        }
        self.tree.data.selection.select(&self.tree.map, neighbor_leaf);
        true
    }

    fn swap_windows(&mut self, layout: LayoutId, a: WindowId, b: WindowId) -> bool {
        let Some(node_a) = self.node_for_window_mut(a) else {
            return false;
        };
        let Some(node_b) = self.node_for_window_mut(b) else {
            return false;
        };
        if node_a == node_b {
            return false;
        }

        if let Some(state) = self.layouts.get(layout).copied() {
            if !self.belongs_to_layout(state, node_a) || !self.belongs_to_layout(state, node_b) {
                return false;
            }
        } else {
            return false;
        }

        let mut a_window = None;
        let mut b_window = None;
        if let Some(NodeKind::Leaf { window, .. }) = self.kind.get(node_a) {
            a_window = *window;
        }
        if let Some(NodeKind::Leaf { window, .. }) = self.kind.get(node_b) {
            b_window = *window;
        }

        if a_window.is_none() && b_window.is_none() {
            return false;
        }

        if let Some(NodeKind::Leaf { window, .. }) = self.kind.get_mut(node_a) {
            *window = b_window;
        }
        if let Some(NodeKind::Leaf { window, .. }) = self.kind.get_mut(node_b) {
            *window = a_window;
        }

        if let Some(w) = a_window {
            self.index_window(w, node_b);
        }
        if let Some(w) = b_window {
            self.index_window(w, node_a);
        }

        true
    }

    fn move_selection_to_layout_after_selection(
        &mut self,
        from_layout: LayoutId,
        to_layout: LayoutId,
    ) {
        let sel = self.selected_window(from_layout);
        if let Some(w) = sel {
            self.remove_window_internal(from_layout, w);
            self.add_window_after_selection(to_layout, w);
        }
    }

    fn split_selection(&mut self, layout: LayoutId, kind: LayoutKind) {
        let orientation = match kind {
            LayoutKind::Horizontal => Orientation::Horizontal,
            LayoutKind::Vertical => Orientation::Vertical,
            _ => return,
        };
        let state = if let Some(s) = self.layouts.get(layout).copied() {
            s
        } else {
            return;
        };

        let sel = self.tree.data.selection.current_selection(state.root);
        let target = self.descend_to_leaf(sel);
        match self.kind.get(target).cloned() {
            Some(NodeKind::Leaf { window, .. }) => {
                let left = self.make_leaf(window);
                let right = self.make_leaf(None);
                if let Some(w) = window {
                    self.index_window(w, left);
                }
                self.kind.insert(target, NodeKind::Split { orientation, ratio: 0.5 });
                left.detach(&mut self.tree).push_back(target);
                right.detach(&mut self.tree).push_back(target);
                self.tree.data.selection.select(&self.tree.map, right);
            }
            _ => {}
        }
    }

    fn toggle_fullscreen_of_selection(&mut self, layout: LayoutId) -> Vec<WindowId> {
        if let Some(sel) = self.selection_of_layout(layout) {
            let sel_leaf = self.descend_to_leaf(sel);
            if let Some(NodeKind::Leaf {
                window: Some(w),
                fullscreen,
                fullscreen_within_gaps,
                ..
            }) = self.kind.get_mut(sel_leaf)
            {
                *fullscreen = !*fullscreen;
                if *fullscreen {
                    *fullscreen_within_gaps = false;
                }
                return vec![*w];
            }
        }
        vec![]
    }

    fn toggle_fullscreen_within_gaps_of_selection(&mut self, layout: LayoutId) -> Vec<WindowId> {
        if let Some(sel) = self.selection_of_layout(layout) {
            let sel_leaf = self.descend_to_leaf(sel);
            if let Some(NodeKind::Leaf {
                window: Some(w),
                fullscreen_within_gaps,
                fullscreen,
                ..
            }) = self.kind.get_mut(sel_leaf)
            {
                *fullscreen_within_gaps = !*fullscreen_within_gaps;
                if *fullscreen_within_gaps {
                    *fullscreen = false;
                }
                return vec![*w];
            }
        }
        vec![]
    }

    fn has_any_fullscreen_node(&self, layout: LayoutId) -> bool {
        if let Some(state) = self.layouts.get(layout).copied() {
            self.has_fullscreen_in_subtree(state.root)
        } else {
            false
        }
    }

    fn join_selection_with_direction(&mut self, layout: LayoutId, direction: Direction) {
        let Some(sel) = self.selection_of_layout(layout) else {
            return;
        };
        let sel_leaf = self.descend_to_leaf(sel);

        let Some(neighbor) = self.find_neighbor_leaf(sel_leaf, direction) else {
            return;
        };

        let mut current = sel_leaf;
        while let Some(parent) = current.parent(&self.tree.map) {
            let children: Vec<_> = parent.children(&self.tree.map).collect();
            if children.contains(&neighbor) {
                if let Some(grandparent) = parent.parent(&self.tree.map) {
                    let mut windows = Vec::new();
                    self.collect_windows_under(parent, &mut windows);

                    let _ = parent.detach(&mut self.tree);
                    self.kind.remove(parent);

                    if let Some(first_window) = windows.first() {
                        let new_leaf = self.make_leaf(Some(*first_window));
                        new_leaf.detach(&mut self.tree).push_back(grandparent);

                        for window in windows {
                            self.index_window(window, new_leaf);
                        }

                        self.tree.data.selection.select(&self.tree.map, new_leaf);
                    }
                }
                break;
            }
            current = parent;
        }
    }

    fn consume_or_expel_selection(&mut self, layout: LayoutId, direction: Direction) {
        let is_joined = self
            .selection_of_layout(layout)
            .map(|selection| self.descend_to_leaf(selection))
            .and_then(|leaf| leaf.parent(&self.tree.map))
            .and_then(|parent| parent.parent(&self.tree.map))
            .is_some();

        if is_joined {
            self.unjoin_selection(layout);
        } else {
            self.join_selection_with_direction(layout, direction);
        }
    }

    fn unjoin_selection(&mut self, layout: LayoutId) {
        let Some(sel) = self.selection_of_layout(layout) else {
            return;
        };
        let sel_leaf = self.descend_to_leaf(sel);
        let map = &self.tree.map;

        let Some(parent) = sel_leaf.parent(map) else {
            return;
        };

        let Some(grandparent) = parent.parent(map) else {
            return;
        };

        let mut windows: Vec<WindowId> = Vec::new();
        self.collect_windows_under(parent, &mut windows);
        if windows.is_empty() {
            return;
        }

        let _ = parent.detach(&mut self.tree);

        let ids: Vec<_> = parent.traverse_preorder(&self.tree.map).collect();
        for id in ids {
            self.kind.remove(id);
        }

        let mut first_new_leaf: Option<NodeId> = None;
        for w in windows {
            let new_leaf = self.make_leaf(Some(w));
            new_leaf.detach(&mut self.tree).push_back(grandparent);
            self.index_window(w, new_leaf);
            if first_new_leaf.is_none() {
                first_new_leaf = Some(new_leaf);
            }
        }

        if let Some(n) = first_new_leaf {
            self.tree.data.selection.select(&self.tree.map, n);
        }
    }

    fn resize_selection_by(
        &mut self,
        layout: LayoutId,
        amount: f64,
        orientation: ResizeOrientation,
    ) {
        let sel_snapshot = self.selection_of_layout(layout);
        let Some(mut node) = sel_snapshot else {
            return;
        };

        while let Some(parent) = node.parent(&self.tree.map) {
            if let Some(NodeKind::Split {
                orientation: split_orientation,
                ratio,
            }) = self.kind.get_mut(parent)
                && match orientation {
                    ResizeOrientation::Horizontal => *split_orientation == Orientation::Horizontal,
                    ResizeOrientation::Vertical => *split_orientation == Orientation::Vertical,
                    ResizeOrientation::Smart => true,
                }
            {
                let is_first = Some(node) == parent.first_child(&self.tree.map);
                let delta = (amount as f32) * 0.5;
                if is_first {
                    *ratio = (*ratio + delta).clamp(0.05, 0.95);
                } else {
                    *ratio = (*ratio - delta).clamp(0.05, 0.95);
                }
                break;
            }
            node = parent;
        }
    }

    fn can_insert_next_to(&self) -> bool { true }

    fn insert_window_next_to(
        &mut self,
        layout: LayoutId,
        target: WindowId,
        direction: Direction,
        window: WindowId,
    ) -> bool {
        if target == window || !self.layouts.contains_key(layout) {
            return false;
        }
        // Check the target is in this tree before touching anything. Removing
        // the dragged window first and only then discovering there is nowhere
        // to put it leaves it in no tree at all: it stops being tiled, stops
        // being laid out, and sits wherever the drag left it.
        if !self.window_to_node.contains_key(&target) {
            return false;
        }
        // It is normally already somewhere in this tree, so it has to leave
        // before it can be re-inserted; splitting first would strand its old
        // leaf, since window_to_node holds one node per window.
        self.retire_all_leaves(window);
        // Re-read the target: removing a window collapses its parent split, so
        // the node the target lives in may not be the one seen above.
        let Some(&leaf) = self.window_to_node.get(&target) else {
            return false;
        };
        self.split_leaf_in_direction(leaf, direction, window);
        true
    }

    fn slot_of(&self, layout: LayoutId, window: WindowId) -> Option<crate::layout_engine::Slot> {
        if !self.layouts.contains_key(layout) {
            return None;
        }
        let node = self.node_for_window(window)?;
        let parent = node.parent(&self.tree.map)?;
        let Some(NodeKind::Split { orientation, ratio }) = self.kind.get(parent).cloned() else {
            return None;
        };
        let mut children = parent.children(&self.tree.map);
        let first = children.next()?;
        let second = children.next()?;
        let (sibling, is_first) = if first == node {
            (second, true)
        } else {
            (first, false)
        };
        let anchor =
            sibling.traverse_preorder(&self.tree.map).find_map(|n| match self.kind.get(n) {
                Some(NodeKind::Leaf { window: Some(w), .. }) => Some(*w),
                _ => None,
            })?;
        let side = match (orientation, is_first) {
            (Orientation::Horizontal, true) => Direction::Left,
            (Orientation::Horizontal, false) => Direction::Right,
            (Orientation::Vertical, true) => Direction::Up,
            (Orientation::Vertical, false) => Direction::Down,
        };
        Some(crate::layout_engine::Slot { anchor, side, ratio })
    }

    fn restore_slot(
        &mut self,
        layout: LayoutId,
        slot: crate::layout_engine::Slot,
        window: WindowId,
    ) -> bool {
        if !self.insert_window_next_to(layout, slot.anchor, slot.side, window) {
            return false;
        }
        // `split_leaf_in_direction` recreates the same child order the slot
        // was recorded with, so the recorded first-child share applies as is.
        if let Some(node) = self.node_for_window(window)
            && let Some(parent) = node.parent(&self.tree.map)
            && let Some(NodeKind::Split { ratio, .. }) = self.kind.get_mut(parent)
        {
            *ratio = slot.ratio.clamp(0.05, 0.95);
        }
        true
    }

    /// Gives every window an equal share, yabai's `space --balance`.
    fn rebalance(&mut self, layout: LayoutId) {
        if let Some(state) = self.layouts.get(layout).copied() {
            self.equalize(state.root);
        }
    }

    fn toggle_tile_orientation(&mut self, layout: LayoutId) {
        let sel_snapshot = self.selection_of_layout(layout);

        let start_node = if let Some(sel) = sel_snapshot {
            sel
        } else {
            let Some(state) = self.layouts.get(layout) else {
                return;
            };
            state.root
        };

        let mut node_opt = Some(start_node);
        while let Some(node) = node_opt {
            if let Some(NodeKind::Split { orientation, .. }) = self.kind.get_mut(node) {
                *orientation = match *orientation {
                    Orientation::Horizontal => Orientation::Vertical,
                    Orientation::Vertical => Orientation::Horizontal,
                };
                return;
            }
            node_opt = node.parent(&self.tree.map);
        }

        if let Some(state) = self.layouts.get_mut(layout) {
            let root = state.root;
            if let Some(NodeKind::Split { orientation, .. }) = self.kind.get_mut(root) {
                *orientation = match *orientation {
                    Orientation::Horizontal => Orientation::Vertical,
                    Orientation::Vertical => Orientation::Horizontal,
                };
            }
        }
    }
}
