//! BSP tree layout for tiling panes within a workspace.

use std::cmp::Reverse;

use ratatui::{
    layout::{Direction, Rect},
    widgets::Borders,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PaneId(u32);

/// Global atomic counter for unique PaneId generation across all workspaces.
static NEXT_PANE_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

impl PaneId {
    /// Allocate a globally unique PaneId.
    pub fn alloc() -> Self {
        Self(NEXT_PANE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    pub fn raw(self) -> u32 {
        self.0
    }

    /// Reconstruct from a saved u32 (persistence only).
    pub fn from_raw(id: u32) -> Self {
        Self(id)
    }
}

/// A pane's membership in a stack, when it is in one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackSlot {
    /// Position within the stack, in list order.
    pub index: usize,
    /// Number of panes in the stack.
    pub len: usize,
    /// True for every member except the active one. Collapsed members render as a
    /// single title row instead of terminal content.
    pub collapsed: bool,
    /// The single row carrying this member's name. Every member has one, the visible
    /// member included, and its terminal starts on the row below.
    pub header_rect: Rect,
    /// Rect the terminal occupies whenever this is the active member. Every member of
    /// a stack is sized to this rect so cycling the stack never resizes a PTY.
    pub content_rect: Rect,
    /// The whole region the stack occupies, bars included. UI chrome is decided for
    /// the region as a unit so every member ends up with identical borders — and so
    /// the same terminal size.
    pub region_rect: Rect,
    /// Borders drawn around the region. Only the visible member carries them on its
    /// own `borders`, so every member records them here to know which sides of the box
    /// its title row has to join.
    pub region_borders: Borders,
}

/// Snapshot of a pane's position and focus state after layout.
#[derive(Clone)]
pub struct PaneInfo {
    pub id: PaneId,
    /// Outer rect (including borders if present).
    pub rect: Rect,
    /// Inner rect (content area, excluding borders). Used for selection.
    pub inner_rect: Rect,
    /// Visible scrollbar lane, when scrollback is present. `inner_rect` may still
    /// exclude a stable hidden gutter when this is `None`.
    pub scrollbar_rect: Option<Rect>,
    /// Borders drawn around this pane after UI chrome is applied.
    pub borders: Borders,
    pub is_focused: bool,
    /// Set when this pane is a member of a stack.
    pub stack: Option<StackSlot>,
}

/// Info about a split boundary, used for mouse drag resize.
#[derive(Clone)]
pub struct SplitBorder {
    /// Position of the divider line (x for horizontal split, y for vertical).
    pub pos: u16,
    /// Direction of the split that created this border.
    pub direction: Direction,
    /// Ratio assigned to the first child of this split.
    pub ratio: f32,
    /// Total area of the split node.
    pub area: Rect,
    /// Path from root to this split node (false=first, true=second).
    pub path: Vec<bool>,
}

/// Where a newly created pane goes relative to the pane it was created from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanePlacement {
    /// Split the target's region. `ratio` defaults to an even split.
    Split {
        direction: Direction,
        ratio: Option<f32>,
    },
    /// Join the target's stack, creating one when the target is a plain pane.
    Stacked,
}

/// Cardinal direction for pane navigation.
#[derive(Debug, Clone, Copy)]
pub enum NavDirection {
    Left,
    Right,
    Up,
    Down,
}

/// A node in the BSP tree. Public for serialization.
#[derive(Clone)]
pub enum Node {
    Pane(PaneId),
    Split {
        direction: Direction,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
    /// Several panes sharing one region. The active member shows terminal content;
    /// the rest collapse to a single title row. Always holds at least two panes —
    /// a stack that drops to one member is demoted back to `Pane`.
    Stack {
        panes: Vec<PaneId>,
        active: usize,
    },
}

/// BSP tiling layout. Tracks a tree of splits and a focused pane.
pub struct TileLayout {
    root: Node,
    focus: PaneId,
    /// Pane focused before `focus`, used by `close_focused`. Only a real focus
    /// move writes it; tree edits go through the target-taking primitives
    /// (`split_pane`, `close_pane`, unfocused `insert_pane_near`) so internal
    /// focus excursions never corrupt it.
    prev_focus: Option<PaneId>,
}

impl TileLayout {
    /// Create a new layout with a single pane (globally unique ID).
    /// Returns (layout, root_pane_id) so the caller can create the pane.
    pub fn new() -> (Self, PaneId) {
        let root_id = PaneId::alloc();
        (
            Self {
                root: Node::Pane(root_id),
                focus: root_id,
                prev_focus: None,
            },
            root_id,
        )
    }

    /// Move focus, recording the pane being left. No-op when focus is unchanged.
    fn set_focus(&mut self, id: PaneId) {
        if id != self.focus {
            self.prev_focus = Some(self.focus);
            self.focus = id;
        }
        self.sync_stack_active();
    }

    /// A focused pane is always the visible member of its stack.
    fn sync_stack_active(&mut self) {
        activate_in_stack(&mut self.root, self.focus);
    }

    pub fn focused(&self) -> PaneId {
        self.focus
    }

    pub fn pane_count(&self) -> usize {
        count_panes(&self.root)
    }

    /// Compute rects for all panes given the available area.
    pub fn panes(&self, area: Rect) -> Vec<PaneInfo> {
        let mut result = Vec::new();
        collect_panes(&self.root, area, self.focus, &mut result);
        result
    }

    /// Compute pane rects as they will be after splitting `target`, without
    /// changing the layout. Returns the rects and the new pane's index; the
    /// new pane's entry carries `target`'s id.
    pub fn panes_after_split(
        &self,
        area: Rect,
        target: PaneId,
        direction: Direction,
        ratio: f32,
    ) -> Option<(Vec<PaneInfo>, usize)> {
        // Splitting a stacked pane splits the region of the whole stack, which the
        // single-rect shortcut below cannot express.
        if stack_neighbor_in(&self.root, target, 0).is_some() {
            return self.panes_after_edit(area, target, |root, new_id| {
                if let Some(node) = find_pane_mut(root, target) {
                    wrap_in_split(node, direction, new_id, valid_split_ratio(ratio));
                }
            });
        }
        let mut panes = self.panes(area);
        let index = panes.iter().position(|info| info.id == target)?;
        let (first, second) = split_rect(panes[index].rect, direction, valid_split_ratio(ratio));
        panes[index].rect = first;
        panes[index].inner_rect = first;
        let mut new_pane = panes[index].clone();
        new_pane.rect = second;
        new_pane.inner_rect = second;
        new_pane.is_focused = false;
        panes.insert(index + 1, new_pane);
        Some((panes, index + 1))
    }

    /// Compute pane rects as they will be after stacking a new pane onto `target`,
    /// without changing the layout. Returns the rects and the new pane's index.
    pub fn panes_after_stack(&self, area: Rect, target: PaneId) -> Option<(Vec<PaneInfo>, usize)> {
        self.panes_after_edit(area, target, |root, new_id| {
            let old = std::mem::replace(root, Node::Pane(new_id));
            *root = stack_at(old, target, new_id);
        })
    }

    /// Lay out a copy of the tree after `edit` inserts a placeholder pane, and report
    /// where the placeholder landed.
    fn panes_after_edit(
        &self,
        area: Rect,
        target: PaneId,
        edit: impl FnOnce(&mut Node, PaneId),
    ) -> Option<(Vec<PaneInfo>, usize)> {
        if !self.pane_ids().contains(&target) {
            return None;
        }
        // Raw id 0 is never allocated, so it cannot collide with a real pane.
        let placeholder = PaneId::from_raw(0);
        let mut root = self.root.clone();
        edit(&mut root, placeholder);
        let mut panes = Vec::new();
        collect_panes(&root, area, self.focus, &mut panes);
        let index = panes.iter().position(|info| info.id == placeholder)?;
        Some((panes, index))
    }

    /// Collect all split boundaries for mouse drag resize.
    pub fn splits(&self, area: Rect) -> Vec<SplitBorder> {
        let mut result = Vec::new();
        collect_splits(&self.root, area, &mut Vec::new(), &mut result);
        result
    }

    /// Split the focused pane. Returns the new pane's id. Production splits
    /// flow through `Tab` so a failed runtime spawn can roll back; this remains
    /// as the user-split shape for tests.
    #[cfg(test)]
    pub fn split_focused(&mut self, direction: Direction) -> PaneId {
        self.split_focused_with_ratio(direction, 0.5)
    }

    /// Split the focused pane with a custom first-child ratio.
    #[cfg(test)]
    pub fn split_focused_with_ratio(&mut self, direction: Direction, ratio: f32) -> PaneId {
        let new_id = self
            .split_pane(self.focus, direction, ratio)
            .expect("focused pane is in the layout");
        self.set_focus(new_id);
        new_id
    }

    /// Split `target` without moving focus. Returns the new pane's id, or None
    /// when `target` is not in the layout.
    pub fn split_pane(
        &mut self,
        target: PaneId,
        direction: Direction,
        ratio: f32,
    ) -> Option<PaneId> {
        let node = find_pane_mut(&mut self.root, target)?;
        let new_id = PaneId::alloc();
        wrap_in_split(node, direction, new_id, valid_split_ratio(ratio));
        Some(new_id)
    }

    /// Create a new pane next to `target` according to `placement`. Returns the new
    /// pane's id, or None when `target` is not in the layout.
    pub fn place_pane(&mut self, target: PaneId, placement: PanePlacement) -> Option<PaneId> {
        match placement {
            PanePlacement::Split { direction, ratio } => {
                self.split_pane(target, direction, ratio.unwrap_or(0.5))
            }
            PanePlacement::Stacked => self.stack_pane(target),
        }
    }

    /// Stack a new pane onto `target`, which becomes a two-member stack when it is
    /// a plain pane and gains a member when it is already stacked. Returns the new
    /// pane's id, or None when `target` is not in the layout. Focus is left alone;
    /// callers move it once the runtime has spawned.
    pub fn stack_pane(&mut self, target: PaneId) -> Option<PaneId> {
        if !self.pane_ids().contains(&target) {
            return None;
        }
        let new_id = PaneId::alloc();
        let placeholder = PaneId::from_raw(0);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        self.root = stack_at(old, target, new_id);
        Some(new_id)
    }

    /// Insert an existing pane id next to a target pane without allocating a new
    /// pane or spawning a terminal runtime. When `focus` is false, focus and its
    /// history are left untouched.
    pub fn insert_pane_near(
        &mut self,
        target: PaneId,
        moved: PaneId,
        direction: Direction,
        ratio: f32,
        focus: bool,
    ) -> bool {
        if target == moved {
            return false;
        }
        let ids = self.pane_ids();
        if ids.contains(&moved) {
            return false;
        }
        let Some(node) = find_pane_mut(&mut self.root, target) else {
            return false;
        };
        wrap_in_split(node, direction, moved, valid_split_ratio(ratio));
        if focus {
            self.set_focus(moved);
        }
        true
    }

    /// Close the focused pane, returning focus to the pane it came from when
    /// that pane is still open. Returns false if it's the last pane.
    pub fn close_focused(&mut self) -> bool {
        if self.pane_count() <= 1 {
            return false;
        }
        let target = self.focus;
        let ids = self.pane_ids();
        let pos = ids.iter().position(|id| *id == target).unwrap();
        let ordered = if pos + 1 < ids.len() {
            ids[pos + 1]
        } else {
            ids[pos - 1]
        };
        let new_focus = match self.prev_focus {
            Some(prev) if prev != target && ids.contains(&prev) => prev,
            _ => ordered,
        };
        let placeholder = PaneId::from_raw(0);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        if let Some(new_root) = remove_pane(old, target) {
            self.root = new_root;
            self.focus = new_focus;
            self.prev_focus = None;
            self.sync_stack_active();
            true
        } else {
            false
        }
    }

    /// Close any pane. Focus and its history are left alone unless the closed
    /// pane is the focused one.
    pub fn close_pane(&mut self, id: PaneId) -> bool {
        if self.focus == id {
            return self.close_focused();
        }
        if self.pane_count() <= 1 || !self.pane_ids().contains(&id) {
            return false;
        }
        let placeholder = PaneId::from_raw(0);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        let Some(new_root) = remove_pane(old, id) else {
            return false;
        };
        self.root = new_root;
        if self.prev_focus == Some(id) {
            self.prev_focus = None;
        }
        true
    }

    pub fn focus_pane(&mut self, id: PaneId) {
        if self.pane_ids().contains(&id) {
            self.set_focus(id);
        }
    }

    /// Swap two pane ids in the layout tree while preserving split shape and
    /// ratios. Returns true only when both panes exist and are different.
    pub fn swap_panes(&mut self, first: PaneId, second: PaneId) -> bool {
        if first == second {
            return false;
        }
        let ids = self.pane_ids();
        if !ids.contains(&first) || !ids.contains(&second) {
            return false;
        }
        swap_pane_ids(&mut self.root, first, second);
        // Swapping ids can move the focused pane into or out of a stack, so the visible
        // member has to be recomputed rather than left pointing at the old occupant.
        self.sync_stack_active();
        true
    }

    /// Make `pane` the visible member of its stack without moving focus.
    pub fn show_in_stack(&mut self, pane: PaneId) {
        activate_in_stack(&mut self.root, pane);
    }

    /// Whether any pane in this layout is stacked.
    pub fn has_stack(&self) -> bool {
        fn walk(node: &Node) -> bool {
            match node {
                Node::Pane(_) => false,
                Node::Split { first, second, .. } => walk(first) || walk(second),
                Node::Stack { .. } => true,
            }
        }
        walk(&self.root)
    }

    /// The member `delta` positions from `pane` in the stack that holds it, or `None`
    /// when `pane` is not stacked or the position falls outside the stack. Reordering
    /// clamps at the ends instead of wrapping, so callers treat `None` as a no-op.
    pub fn stack_neighbor(&self, pane: PaneId, delta: isize) -> Option<PaneId> {
        if delta == 0 {
            return None;
        }
        stack_neighbor_in(&self.root, pane, delta)
    }

    /// Set the ratio of a split node at the given path.
    pub fn set_ratio_at(&mut self, path: &[bool], ratio: f32) -> bool {
        set_ratio_at(&mut self.root, path, ratio.clamp(0.1, 0.9))
    }

    /// Adjust the nearest split in the given direction for the focused pane.
    /// `delta` is positive to grow, negative to shrink.
    pub fn resize_focused(&mut self, nav: NavDirection, delta: f32, area: Rect) {
        let panes = self.panes(area);
        let Some(focused) = panes.iter().find(|p| p.is_focused) else {
            return;
        };
        // A stacked pane's own band sits inside its stack's region; the splits
        // around it border the region, so that is the edge to measure from.
        let focused_rect = focused.stack.map_or(focused.rect, |slot| slot.region_rect);
        let splits = self.splits(area);

        let target_dir = match nav {
            NavDirection::Left | NavDirection::Right => Direction::Horizontal,
            NavDirection::Up | NavDirection::Down => Direction::Vertical,
        };
        let grows = matches!(nav, NavDirection::Right | NavDirection::Down);

        let best = nearest_resize_split(&splits, target_dir, focused_rect, nav).or_else(|| {
            nearest_resize_split(&splits, target_dir, focused_rect, opposite_direction(nav))
        });

        if let Some(split) = best {
            let path = split.path.clone();
            let current_ratio = get_ratio_at(&self.root, &path).unwrap_or(0.5);
            let adj = if grows { delta } else { -delta };
            self.set_ratio_at(&path, current_ratio + adj);
        }
    }

    pub fn resize_pane(
        &mut self,
        pane_id: PaneId,
        nav: NavDirection,
        delta: f32,
        area: Rect,
    ) -> bool {
        if !self.pane_ids().contains(&pane_id) {
            return false;
        }
        let before = split_ratios(&self.root);
        let previous_focus = self.focus;
        self.focus = pane_id;
        self.resize_focused(nav, delta, area);
        self.focus = previous_focus;
        split_ratios(&self.root) != before
    }

    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        collect_ids(&self.root, &mut ids);
        ids
    }

    /// Access the tree root for serialization.
    pub fn root(&self) -> &Node {
        &self.root
    }

    /// Reconstruct a layout from a saved tree.
    pub fn from_saved(root: Node, focus: PaneId) -> Self {
        let mut layout = Self {
            root,
            focus,
            prev_focus: None,
        };
        layout.sync_stack_active();
        layout
    }
}

// --- Directional pane navigation ---

/// Find the nearest pane in the given direction from `focused`.
pub fn find_in_direction(
    focused: &PaneInfo,
    direction: NavDirection,
    panes: &[PaneInfo],
) -> Option<PaneId> {
    let fr = focused.rect;

    panes
        .iter()
        .enumerate()
        .filter(|(_, p)| p.id != focused.id)
        .filter(|(_, p)| {
            let r = p.rect;
            match direction {
                NavDirection::Left => {
                    r.x + r.width <= fr.x && ranges_overlap(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Right => {
                    r.x >= fr.x + fr.width && ranges_overlap(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Up => {
                    r.y + r.height <= fr.y && ranges_overlap(r.x, r.width, fr.x, fr.width)
                }
                NavDirection::Down => {
                    r.y >= fr.y + fr.height && ranges_overlap(r.x, r.width, fr.x, fr.width)
                }
            }
        })
        .min_by_key(|(index, p)| {
            let r = p.rect;
            let edge_distance = match direction {
                NavDirection::Left => fr.x.saturating_sub(r.x + r.width),
                NavDirection::Right => r.x.saturating_sub(fr.x + fr.width),
                NavDirection::Up => fr.y.saturating_sub(r.y + r.height),
                NavDirection::Down => r.y.saturating_sub(fr.y + fr.height),
            };
            let overlap = match direction {
                NavDirection::Left | NavDirection::Right => {
                    range_overlap_amount(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Up | NavDirection::Down => {
                    range_overlap_amount(r.x, r.width, fr.x, fr.width)
                }
            };
            let center_distance = match direction {
                NavDirection::Left | NavDirection::Right => {
                    range_center_distance(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Up | NavDirection::Down => {
                    range_center_distance(r.x, r.width, fr.x, fr.width)
                }
            };
            (edge_distance, Reverse(overlap), center_distance, *index)
        })
        .map(|(_, p)| p.id)
}

fn ranges_overlap(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> bool {
    a_start < b_start + b_len && a_start + a_len > b_start
}

fn split_on_requested_edge(split: &SplitBorder, focused: Rect, nav: NavDirection) -> bool {
    split_edge_distance(split, focused, nav) <= 1
}

fn split_area_overlaps_focused_pane(split: &SplitBorder, focused: Rect, nav: NavDirection) -> bool {
    match nav {
        NavDirection::Left | NavDirection::Right => {
            ranges_overlap(split.area.y, split.area.height, focused.y, focused.height)
        }
        NavDirection::Up | NavDirection::Down => {
            ranges_overlap(split.area.x, split.area.width, focused.x, focused.width)
        }
    }
}

fn nearest_resize_split(
    splits: &[SplitBorder],
    target_dir: Direction,
    focused: Rect,
    nav: NavDirection,
) -> Option<&SplitBorder> {
    splits
        .iter()
        .filter(|s| s.direction == target_dir)
        .filter(|s| split_area_overlaps_focused_pane(s, focused, nav))
        .filter(|s| split_on_requested_edge(s, focused, nav))
        .min_by_key(|s| split_edge_distance(s, focused, nav))
}

fn opposite_direction(nav: NavDirection) -> NavDirection {
    match nav {
        NavDirection::Left => NavDirection::Right,
        NavDirection::Right => NavDirection::Left,
        NavDirection::Up => NavDirection::Down,
        NavDirection::Down => NavDirection::Up,
    }
}

fn split_edge_distance(split: &SplitBorder, focused: Rect, nav: NavDirection) -> u32 {
    match nav {
        NavDirection::Left => (split.pos as i32 - focused.x as i32).unsigned_abs(),
        NavDirection::Right => {
            (split.pos as i32 - (focused.x + focused.width) as i32).unsigned_abs()
        }
        NavDirection::Up => (split.pos as i32 - focused.y as i32).unsigned_abs(),
        NavDirection::Down => {
            (split.pos as i32 - (focused.y + focused.height) as i32).unsigned_abs()
        }
    }
}

fn range_overlap_amount(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_end.min(b_end).saturating_sub(a_start.max(b_start))
}

fn range_center_distance(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_center = a_start.saturating_mul(2).saturating_add(a_len);
    let b_center = b_start.saturating_mul(2).saturating_add(b_len);
    a_center.abs_diff(b_center)
}

// --- Tree operations ---

fn count_panes(node: &Node) -> usize {
    match node {
        Node::Pane(_) => 1,
        Node::Split { first, second, .. } => count_panes(first) + count_panes(second),
        Node::Stack { panes, .. } => panes.len(),
    }
}

/// Rows the visible member keeps for terminal content, on top of its own title row.
///
/// A stack must not shrink its members' terminals to nothing as it grows. Once the
/// region cannot seat every title row and still leave this much content, title rows
/// are dropped instead, starting with the members furthest from the visible one.
pub(crate) const MIN_STACK_CONTENT_ROWS: u16 = 3;

/// Row heights for the members of a stack, in list order.
///
/// Every member gets one title row. A collapsed member is nothing but that row; the
/// visible member's share is its own title row plus all the remaining content rows.
/// When the region is too short, members furthest from the visible one lose their row
/// first; the visible member always keeps at least one.
pub(crate) fn stack_heights(height: u16, len: usize, active: usize) -> Vec<u16> {
    let mut heights = vec![0u16; len];
    if len == 0 || height == 0 || active >= len {
        return heights;
    }

    // Seat as many title rows as fit while leaving the visible member its own row plus
    // a usable content area. Below that the region simply cannot show every member.
    let reserved = MIN_STACK_CONTENT_ROWS.saturating_add(1);
    let affordable = height.saturating_sub(reserved) as usize;
    let bars = (height.saturating_sub(1) as usize)
        .min(len - 1)
        .min(affordable);
    let mut remaining = bars;
    // Members adjacent to the visible one keep their row when space runs out.
    for offset in 1..len {
        for index in [active.wrapping_sub(offset), active + offset] {
            if remaining == 0 {
                break;
            }
            if index < len && index != active && heights[index] == 0 {
                heights[index] = 1;
                remaining -= 1;
            }
        }
    }

    heights[active] = height - bars as u16;
    heights
}

/// The active member's header row and the terminal rect directly beneath it.
pub(crate) fn stack_content_rect(area: Rect, heights: &[u16], active: usize) -> (Rect, Rect) {
    let above: u16 = heights[..active.min(heights.len())].iter().sum();
    let header_y = area.y.saturating_add(above);
    let share = heights.get(active).copied().unwrap_or(area.height);
    let header = Rect::new(area.x, header_y, area.width, share.min(1));
    let content = Rect::new(
        area.x,
        header_y.saturating_add(1),
        area.width,
        share.saturating_sub(1),
    );
    (content, header)
}

fn collect_stack_panes(
    panes: &[PaneId],
    active: usize,
    area: Rect,
    focus: PaneId,
    result: &mut Vec<PaneInfo>,
) {
    let heights = stack_heights(area.height, panes.len(), active);
    let (content_rect, _) = stack_content_rect(area, &heights, active);

    let mut y = area.y;
    for (index, id) in panes.iter().enumerate() {
        let height = heights[index];
        let rect = Rect::new(area.x, y, area.width, height);
        let header_rect = Rect::new(area.x, y, area.width, height.min(1));
        y = y.saturating_add(height);
        result.push(PaneInfo {
            id: *id,
            rect,
            inner_rect: rect,
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: *id == focus,
            stack: Some(StackSlot {
                index,
                len: panes.len(),
                collapsed: index != active,
                header_rect,
                content_rect,
                region_rect: area,
                region_borders: Borders::NONE,
            }),
        });
    }
}

fn collect_panes(node: &Node, area: Rect, focus: PaneId, result: &mut Vec<PaneInfo>) {
    match node {
        Node::Pane(id) => {
            result.push(PaneInfo {
                id: *id,
                rect: area,
                // inner_rect is set during render when we know if borders are shown
                inner_rect: area,
                scrollbar_rect: None,
                borders: Borders::NONE,
                is_focused: *id == focus,
                stack: None,
            });
        }
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let (a, b) = split_rect(area, *direction, *ratio);
            collect_panes(first, a, focus, result);
            collect_panes(second, b, focus, result);
        }
        Node::Stack { panes, active } => {
            collect_stack_panes(panes, *active, area, focus, result);
        }
    }
}

fn collect_splits(node: &Node, area: Rect, path: &mut Vec<bool>, result: &mut Vec<SplitBorder>) {
    if let Node::Split {
        direction,
        ratio,
        first,
        second,
    } = node
    {
        let (a, b) = split_rect(area, *direction, *ratio);
        let pos = match direction {
            Direction::Horizontal => a.x + a.width,
            Direction::Vertical => a.y + a.height,
        };
        result.push(SplitBorder {
            pos,
            direction: *direction,
            ratio: *ratio,
            area,
            path: path.clone(),
        });
        path.push(false);
        collect_splits(first, a, path, result);
        path.pop();
        path.push(true);
        collect_splits(second, b, path, result);
        path.pop();
    }
}

fn collect_ids(node: &Node, ids: &mut Vec<PaneId>) {
    match node {
        Node::Pane(id) => ids.push(*id),
        Node::Split { first, second, .. } => {
            collect_ids(first, ids);
            collect_ids(second, ids);
        }
        Node::Stack { panes, .. } => ids.extend(panes.iter().copied()),
    }
}

fn split_ratios(node: &Node) -> Vec<(Vec<bool>, f32)> {
    fn collect(node: &Node, path: &mut Vec<bool>, out: &mut Vec<(Vec<bool>, f32)>) {
        match node {
            Node::Pane(_) | Node::Stack { .. } => {}
            Node::Split {
                ratio,
                first,
                second,
                ..
            } => {
                out.push((path.clone(), *ratio));
                path.push(false);
                collect(first, path, out);
                path.pop();
                path.push(true);
                collect(second, path, out);
                path.pop();
            }
        }
    }

    let mut out = Vec::new();
    collect(node, &mut Vec::new(), &mut out);
    out
}

fn swap_pane_ids(node: &mut Node, first: PaneId, second: PaneId) {
    match node {
        Node::Pane(id) if *id == first => *id = second,
        Node::Pane(id) if *id == second => *id = first,
        Node::Pane(_) => {}
        Node::Split {
            first: first_child,
            second: second_child,
            ..
        } => {
            swap_pane_ids(first_child, first, second);
            swap_pane_ids(second_child, first, second);
        }
        Node::Stack { panes, .. } => {
            for id in panes.iter_mut() {
                if *id == first {
                    *id = second;
                } else if *id == second {
                    *id = first;
                }
            }
        }
    }
}

fn find_pane_mut(node: &mut Node, target: PaneId) -> Option<&mut Node> {
    match node {
        Node::Pane(id) if *id == target => Some(node),
        Node::Pane(_) => None,
        Node::Split { first, second, .. } => {
            find_pane_mut(first, target).or_else(|| find_pane_mut(second, target))
        }
        // A stacked pane stands for the region of its whole stack.
        Node::Stack { panes, .. } if panes.contains(&target) => Some(node),
        Node::Stack { .. } => None,
    }
}

/// Replace `node` with a split holding `node` first and `new_id` second.
fn wrap_in_split(node: &mut Node, direction: Direction, new_id: PaneId, split_ratio: f32) {
    let old = std::mem::replace(node, Node::Pane(new_id));
    *node = Node::Split {
        direction,
        ratio: split_ratio,
        first: Box::new(old),
        second: Box::new(Node::Pane(new_id)),
    };
}

/// Add `new_id` to the stack holding `target`, or turn `target` into a two-member
/// stack when it is a plain pane.
fn stack_at(node: Node, target: PaneId, new_id: PaneId) -> Node {
    match node {
        // The new pane joins as a collapsed member. Callers that want it in front
        // focus it, and focusing is what makes a member visible.
        Node::Pane(id) if id == target => Node::Stack {
            panes: vec![id, new_id],
            active: 0,
        },
        Node::Pane(_) => node,
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => Node::Split {
            direction,
            ratio,
            first: Box::new(stack_at(*first, target, new_id)),
            second: Box::new(stack_at(*second, target, new_id)),
        },
        Node::Stack { mut panes, active } => {
            let Some(pos) = panes.iter().position(|id| *id == target) else {
                return Node::Stack { panes, active };
            };
            panes.insert(pos + 1, new_id);
            // Whichever member was visible stays visible; the insertion only shifts
            // its index.
            let active = if active > pos { active + 1 } else { active };
            Node::Stack { panes, active }
        }
    }
}

/// Make `target` the active member of the stack that holds it.
fn activate_in_stack(node: &mut Node, target: PaneId) -> bool {
    match node {
        Node::Pane(_) => false,
        Node::Split { first, second, .. } => {
            activate_in_stack(first, target) || activate_in_stack(second, target)
        }
        Node::Stack { panes, active } => match panes.iter().position(|id| *id == target) {
            Some(pos) => {
                *active = pos;
                true
            }
            None => false,
        },
    }
}

/// Look up `target`'s stack neighbour `delta` positions away, in list order. A
/// `delta` of zero answers whether `target` is stacked at all.
fn stack_neighbor_in(node: &Node, target: PaneId, delta: isize) -> Option<PaneId> {
    match node {
        Node::Pane(_) => None,
        Node::Split { first, second, .. } => stack_neighbor_in(first, target, delta)
            .or_else(|| stack_neighbor_in(second, target, delta)),
        Node::Stack { panes, .. } => {
            let index = panes.iter().position(|id| *id == target)?;
            panes.get(index.checked_add_signed(delta)?).copied()
        }
    }
}

#[cfg(test)]
fn split_node(target: PaneId, direction: Direction, new_id: PaneId, split_ratio: f32) -> Node {
    Node::Split {
        direction,
        ratio: split_ratio,
        first: Box::new(Node::Pane(target)),
        second: Box::new(Node::Pane(new_id)),
    }
}

pub(crate) fn valid_split_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(0.1, 0.9)
    } else {
        0.5
    }
}

fn remove_pane(node: Node, target: PaneId) -> Option<Node> {
    match node {
        Node::Pane(id) if id == target => None,
        Node::Pane(_) => Some(node),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => match (remove_pane(*first, target), remove_pane(*second, target)) {
            (None, Some(s)) => Some(s),
            (Some(f), None) => Some(f),
            (Some(f), Some(s)) => Some(Node::Split {
                direction,
                ratio,
                first: Box::new(f),
                second: Box::new(s),
            }),
            (None, None) => None,
        },
        Node::Stack { mut panes, active } => {
            let Some(pos) = panes.iter().position(|id| *id == target) else {
                return Some(Node::Stack { panes, active });
            };
            panes.remove(pos);
            match panes.len() {
                0 => None,
                1 => Some(Node::Pane(panes[0])),
                len => {
                    let active = if active > pos {
                        active - 1
                    } else {
                        active.min(len - 1)
                    };
                    Some(Node::Stack { panes, active })
                }
            }
        }
    }
}

fn set_ratio_at(node: &mut Node, path: &[bool], new_ratio: f32) -> bool {
    if let Node::Split {
        ratio,
        first,
        second,
        ..
    } = node
    {
        if path.is_empty() {
            *ratio = new_ratio;
            true
        } else if path[0] {
            set_ratio_at(second, &path[1..], new_ratio)
        } else {
            set_ratio_at(first, &path[1..], new_ratio)
        }
    } else {
        false
    }
}

fn get_ratio_at(node: &Node, path: &[bool]) -> Option<f32> {
    if let Node::Split {
        ratio,
        first,
        second,
        ..
    } = node
    {
        if path.is_empty() {
            Some(*ratio)
        } else if path[0] {
            get_ratio_at(second, &path[1..])
        } else {
            get_ratio_at(first, &path[1..])
        }
    } else {
        None
    }
}

fn split_rect(area: Rect, direction: Direction, ratio: f32) -> (Rect, Rect) {
    match direction {
        Direction::Horizontal => {
            let first_w = ((area.width as f32) * ratio).round() as u16;
            let second_w = area.width.saturating_sub(first_w);
            (
                Rect::new(area.x, area.y, first_w, area.height),
                Rect::new(area.x + first_w, area.y, second_w, area.height),
            )
        }
        Direction::Vertical => {
            let first_h = ((area.height as f32) * ratio).round() as u16;
            let second_h = area.height.saturating_sub(first_h);
            (
                Rect::new(area.x, area.y, area.width, first_h),
                Rect::new(area.x, area.y + first_h, area.width, second_h),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_paths_preserve_preorder_geometry_and_resize_targets() {
        let ids = [
            PaneId::alloc(),
            PaneId::alloc(),
            PaneId::alloc(),
            PaneId::alloc(),
        ];
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.5,
                first: Box::new(split_node(ids[0], Direction::Vertical, ids[1], 0.5)),
                second: Box::new(split_node(ids[2], Direction::Vertical, ids[3], 0.25)),
            },
            ids[0],
        );
        let area = Rect::new(3, 7, 120, 80);
        let splits = layout.splits(area);
        assert_eq!(splits.len(), 3);
        for (split, (path, direction, pos, rect)) in splits.iter().zip([
            (vec![], Direction::Horizontal, 63, area),
            (
                vec![false],
                Direction::Vertical,
                47,
                Rect::new(3, 7, 60, 80),
            ),
            (
                vec![true],
                Direction::Vertical,
                27,
                Rect::new(63, 7, 60, 80),
            ),
        ]) {
            assert_eq!(
                (split.path.clone(), split.direction, split.pos, split.area),
                (path, direction, pos, rect)
            );
        }
        assert!(layout.set_ratio_at(&splits[2].path, 0.5));
        let resized = layout.splits(area);
        assert_eq!(resized[1].pos, splits[1].pos);
        assert_eq!(resized[2].pos, 47);
    }

    /// Builds a stack the way the TUI does: each new pane is stacked on the last one
    /// and focused, so the newest member is the visible one.
    fn stacked_layout(members: usize) -> (TileLayout, Vec<PaneId>) {
        let (mut layout, root) = TileLayout::new();
        let mut ids = vec![root];
        let mut target = root;
        for _ in 1..members {
            let new_id = layout.stack_pane(target).expect("target is in the layout");
            layout.focus_pane(new_id);
            ids.push(new_id);
            target = new_id;
        }
        (layout, ids)
    }

    #[test]
    fn a_stacked_pane_created_without_focus_does_not_steal_the_visible_slot() {
        let (mut layout, root) = TileLayout::new();

        let added = layout.stack_pane(root).expect("root is in the layout");

        assert_eq!(layout.focused(), root);
        let visible: Vec<PaneId> = layout
            .panes(Rect::new(0, 0, 100, 40))
            .into_iter()
            .filter(|info| !info.stack.expect("stacked").collapsed)
            .map(|info| info.id)
            .collect();
        assert_eq!(
            visible,
            vec![root],
            "the focused pane stays visible until the new one is focused"
        );

        layout.focus_pane(added);
        let visible: Vec<PaneId> = layout
            .panes(Rect::new(0, 0, 100, 40))
            .into_iter()
            .filter(|info| !info.stack.expect("stacked").collapsed)
            .map(|info| info.id)
            .collect();
        assert_eq!(visible, vec![added]);
    }

    fn stack_slots(layout: &TileLayout) -> Vec<(PaneId, Rect, bool)> {
        layout
            .panes(Rect::new(0, 0, 100, 40))
            .into_iter()
            .map(|info| {
                let slot = info.stack.expect("pane should be stacked");
                (info.id, info.rect, slot.collapsed)
            })
            .collect()
    }

    #[test]
    fn stacking_a_plain_pane_makes_a_two_member_stack() {
        let (layout, ids) = stacked_layout(2);

        assert_eq!(layout.pane_count(), 2);
        assert_eq!(layout.pane_ids(), ids);
        assert_eq!(
            stack_slots(&layout),
            vec![
                (ids[0], Rect::new(0, 0, 100, 1), true),
                (ids[1], Rect::new(0, 1, 100, 39), false),
            ]
        );
    }

    #[test]
    fn every_collapsed_member_gets_one_row_and_the_active_member_takes_the_rest() {
        let (mut layout, ids) = stacked_layout(5);
        layout.focus_pane(ids[2]);

        let slots = stack_slots(&layout);
        assert_eq!(
            slots,
            vec![
                (ids[0], Rect::new(0, 0, 100, 1), true),
                (ids[1], Rect::new(0, 1, 100, 1), true),
                (ids[2], Rect::new(0, 2, 100, 36), false),
                (ids[3], Rect::new(0, 38, 100, 1), true),
                (ids[4], Rect::new(0, 39, 100, 1), true),
            ]
        );
        let total: u16 = slots.iter().map(|(_, rect, _)| rect.height).sum();
        assert_eq!(total, 40);
    }

    #[test]
    fn every_member_reports_the_same_content_rect_so_no_pty_is_resized_by_cycling() {
        let (mut layout, ids) = stacked_layout(4);

        let content_rects = |layout: &TileLayout| {
            layout
                .panes(Rect::new(0, 0, 100, 40))
                .into_iter()
                .map(|info| info.stack.expect("stacked").content_rect)
                .collect::<Vec<_>>()
        };

        // Four members in 40 rows: four header rows, 36 content rows. The newest
        // member is active and last, so its header is row 3 and content starts at 4.
        let before = content_rects(&layout);
        assert_eq!(before, vec![Rect::new(0, 4, 100, 36); 4]);

        layout.focus_pane(ids[0]);
        assert_eq!(content_rects(&layout), vec![Rect::new(0, 1, 100, 36); 4]);

        // The region each member is sized to keeps its dimensions as focus moves;
        // only its origin follows the header rows above it.
        for rect in content_rects(&layout) {
            assert_eq!((rect.width, rect.height), (100, 36));
        }
    }

    #[test]
    fn focusing_a_stacked_pane_makes_it_the_visible_member() {
        let (mut layout, ids) = stacked_layout(3);
        assert_eq!(layout.focused(), ids[2], "the newest member is in front");

        layout.focus_pane(ids[1]);

        let visible: Vec<PaneId> = stack_slots(&layout)
            .into_iter()
            .filter(|(_, _, collapsed)| !collapsed)
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(visible, vec![ids[1]]);
    }

    #[test]
    fn closing_members_demotes_a_stack_back_to_a_plain_pane() {
        let (mut layout, ids) = stacked_layout(3);

        assert!(layout.close_pane(ids[1]));
        assert_eq!(layout.pane_count(), 2);
        assert!(layout.panes(Rect::new(0, 0, 100, 40))[0].stack.is_some());

        assert!(layout.close_pane(ids[2]));
        assert_eq!(layout.pane_ids(), vec![ids[0]]);
        let remaining = layout.panes(Rect::new(0, 0, 100, 40));
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].stack.is_none());
        assert_eq!(remaining[0].rect, Rect::new(0, 0, 100, 40));
    }

    #[test]
    fn splitting_a_stacked_pane_splits_the_region_the_whole_stack_occupies() {
        let (mut layout, ids) = stacked_layout(3);

        let new_id = layout
            .split_pane(ids[1], Direction::Horizontal, 0.5)
            .expect("stacked pane can be split");

        assert_eq!(layout.pane_count(), 4);
        let panes = layout.panes(Rect::new(0, 0, 100, 40));
        for info in panes.iter().filter(|info| info.id != new_id) {
            assert!(info.stack.is_some());
            assert_eq!(info.rect.width, 50);
        }
        let split_off = panes
            .iter()
            .find(|info| info.id == new_id)
            .expect("new pane exists");
        assert!(split_off.stack.is_none());
        assert_eq!(split_off.rect, Rect::new(50, 0, 50, 40));
    }

    #[test]
    fn a_stack_taller_than_its_region_drops_title_rows_rather_than_the_terminal() {
        let (mut layout, ids) = stacked_layout(6);
        layout.focus_pane(ids[3]);

        let panes = layout.panes(Rect::new(0, 0, 100, 8));
        let total: u16 = panes.iter().map(|info| info.rect.height).sum();
        assert_eq!(total, 8, "the members exactly fill the region");

        let active = panes
            .iter()
            .find(|info| info.id == ids[3])
            .expect("active member is present");
        let slot = active.stack.expect("stacked");
        assert!(
            slot.content_rect.height >= MIN_STACK_CONTENT_ROWS,
            "the visible terminal keeps a usable height, got {}",
            slot.content_rect.height
        );
        assert!(
            panes.iter().any(|info| info.rect.height == 0),
            "a member that cannot be seated loses its title row"
        );
    }

    #[test]
    fn growing_a_stack_never_starves_the_visible_terminal() {
        // Twenty members in twelve rows: title rows must give way, not the terminal.
        let (mut layout, ids) = stacked_layout(20);
        layout.focus_pane(ids[19]);

        for height in [6u16, 8, 12, 40] {
            let panes = layout.panes(Rect::new(0, 0, 100, height));
            let slot = panes
                .iter()
                .find(|info| info.id == ids[19])
                .expect("visible member is present")
                .stack
                .expect("stacked");
            assert!(
                slot.content_rect.height >= MIN_STACK_CONTENT_ROWS,
                "height {height} left only {} content rows",
                slot.content_rect.height
            );
            let total: u16 = panes.iter().map(|info| info.rect.height).sum();
            assert_eq!(total, height, "members must tile the region exactly");
        }
    }

    #[test]
    fn stack_heights_tolerates_an_out_of_range_active_index() {
        assert_eq!(stack_heights(10, 3, 9), vec![0, 0, 0]);
    }

    #[test]
    fn swapping_a_pane_into_a_stack_keeps_the_focused_member_visible() {
        let (mut layout, ids) = stacked_layout(3);
        let outside = layout
            .split_pane(ids[0], Direction::Horizontal, 0.5)
            .expect("stacked pane can be split");
        layout.focus_pane(outside);

        assert!(layout.swap_panes(outside, ids[1]));

        // `outside` now sits inside the stack and still holds the focus, so it must be
        // the member on show.
        let visible: Vec<PaneId> = layout
            .panes(Rect::new(0, 0, 100, 40))
            .into_iter()
            .filter(|info| info.stack.is_some_and(|slot| !slot.collapsed))
            .map(|info| info.id)
            .collect();
        assert_eq!(visible, vec![outside]);
    }

    #[test]
    fn stack_neighbor_walks_the_member_list_in_order() {
        let (layout, ids) = stacked_layout(3);

        assert_eq!(layout.stack_neighbor(ids[1], -1), Some(ids[0]));
        assert_eq!(layout.stack_neighbor(ids[1], 1), Some(ids[2]));
    }

    #[test]
    fn stack_neighbor_clamps_at_both_ends_of_the_stack() {
        let (layout, ids) = stacked_layout(3);

        assert_eq!(layout.stack_neighbor(ids[0], -1), None);
        assert_eq!(layout.stack_neighbor(ids[2], 1), None);
    }

    #[test]
    fn stack_neighbor_ignores_panes_outside_a_stack() {
        let (mut layout, ids) = stacked_layout(2);
        let outside = layout
            .split_pane(ids[0], Direction::Horizontal, 0.5)
            .expect("stacked pane can be split");

        // A plain pane beside the stack has no member list to step through, and neither
        // does an id that is not in the layout at all.
        assert_eq!(layout.stack_neighbor(outside, -1), None);
        assert_eq!(layout.stack_neighbor(outside, 1), None);
        assert_eq!(layout.stack_neighbor(pane(99), 1), None);
    }

    #[test]
    fn stack_neighbor_pairs_the_two_members_of_a_minimal_stack() {
        let (layout, ids) = stacked_layout(2);

        assert_eq!(layout.stack_neighbor(ids[0], 1), Some(ids[1]));
        assert_eq!(layout.stack_neighbor(ids[1], -1), Some(ids[0]));
    }

    #[test]
    fn swapping_with_a_stack_neighbor_reorders_the_stack_and_keeps_focus_visible() {
        let (mut layout, ids) = stacked_layout(3);
        layout.focus_pane(ids[2]);

        let neighbor = layout
            .stack_neighbor(ids[2], -1)
            .expect("the last member has a neighbour above it");
        assert!(layout.swap_panes(ids[2], neighbor));

        let slots = stack_slots(&layout);
        assert_eq!(
            slots.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
            vec![ids[0], ids[2], ids[1]]
        );
        // The moved pane still holds the focus, so it must still be the member on show.
        let visible: Vec<PaneId> = slots
            .iter()
            .filter(|(_, _, collapsed)| !*collapsed)
            .map(|(id, _, _)| *id)
            .collect();
        assert_eq!(visible, vec![ids[2]]);
    }

    #[test]
    fn panes_after_stack_predicts_the_real_stack_geometry() {
        let area = Rect::new(2, 1, 100, 40);
        let (mut layout, root) = TileLayout::new();
        let right = layout.split_pane(root, Direction::Horizontal, 0.5).unwrap();
        // The second round stacks onto a pane that is already in a stack.
        for _ in 0..2 {
            let (predicted, index) = layout.panes_after_stack(area, right).unwrap();
            let new_id = layout.stack_pane(right).unwrap();
            let actual = layout.panes(area);
            assert_eq!(
                predicted.iter().map(|info| info.rect).collect::<Vec<_>>(),
                actual.iter().map(|info| info.rect).collect::<Vec<_>>()
            );
            assert_eq!(actual[index].id, new_id);
        }
        assert!(layout.panes_after_stack(area, PaneId::alloc()).is_none());
    }

    #[test]
    fn panes_after_split_of_a_stacked_pane_predicts_splitting_the_whole_stack() {
        let area = Rect::new(0, 0, 100, 40);
        let (mut layout, ids) = stacked_layout(3);

        let (predicted, index) = layout
            .panes_after_split(area, ids[1], Direction::Vertical, 0.5)
            .unwrap();
        let new_id = layout.split_pane(ids[1], Direction::Vertical, 0.5).unwrap();
        let actual = layout.panes(area);

        assert_eq!(
            predicted.iter().map(|info| info.rect).collect::<Vec<_>>(),
            actual.iter().map(|info| info.rect).collect::<Vec<_>>()
        );
        assert_eq!(actual[index].id, new_id);
        assert_eq!(actual[index].rect, Rect::new(0, 20, 100, 20));
    }

    #[test]
    fn inserting_a_moved_pane_next_to_a_stacked_pane_keeps_the_stack_whole() {
        let (mut layout, ids) = stacked_layout(2);
        let moved = PaneId::alloc();

        assert!(layout.insert_pane_near(ids[0], moved, Direction::Horizontal, 0.5, false));

        let panes = layout.panes(Rect::new(0, 0, 100, 40));
        assert_eq!(layout.pane_ids(), vec![ids[0], ids[1], moved]);
        assert!(panes[0].stack.is_some() && panes[1].stack.is_some());
        assert!(panes[2].stack.is_none());
        assert_eq!(panes[2].rect, Rect::new(50, 0, 50, 40));
    }

    #[test]
    fn resizing_the_visible_member_moves_the_split_bordering_its_stack() {
        let area = Rect::new(0, 0, 100, 40);
        let (mut layout, root) = TileLayout::new();
        let lower = layout.split_pane(root, Direction::Vertical, 0.5).unwrap();
        let second = layout.stack_pane(lower).unwrap();
        let third = layout.stack_pane(second).unwrap();
        // The newest member is visible, two title rows below the region's top edge.
        layout.focus_pane(third);

        layout.resize_focused(NavDirection::Up, 0.1, area);

        assert_eq!(
            split_snapshot(&layout),
            vec![(Direction::Vertical, 0.4)],
            "growing the stack upward moves the split above it"
        );
    }

    #[test]
    fn rejected_splits_and_insertions_preserve_layout_and_focus_history() {
        let (mut layout, root) = TileLayout::new();
        let second = layout.split_focused(Direction::Horizontal);
        let absent = PaneId::alloc();
        let before = pane_rects(&layout);
        let splits = split_snapshot(&layout);
        assert!(layout
            .split_pane(absent, Direction::Vertical, 0.5)
            .is_none());
        for (target, moved) in [(absent, PaneId::alloc()), (root, second), (root, root)] {
            assert!(!layout.insert_pane_near(target, moved, Direction::Vertical, 0.3, true));
        }
        assert_eq!(pane_rects(&layout), before);
        assert_eq!(split_snapshot(&layout), splits);
        assert_eq!(layout.focused(), second);
        assert_eq!(layout.prev_focus, Some(root));
        assert!(layout.close_focused());
        assert_eq!(layout.focused(), root);
    }

    #[test]
    fn splitting_a_deep_leaf_preserves_other_branches_and_focus() {
        let (mut layout, root) = TileLayout::new();
        let right = layout.split_pane(root, Direction::Horizontal, 0.6).unwrap();
        let bottom_left = layout.split_pane(root, Direction::Vertical, 0.4).unwrap();
        let right_rect = pane_rect(&layout, right);
        let focus = layout.focused();
        let new = layout
            .split_pane(bottom_left, Direction::Horizontal, f32::NAN)
            .unwrap();
        assert_eq!(layout.pane_ids(), [root, bottom_left, new, right]);
        assert_eq!(pane_rect(&layout, right), right_rect);
        assert_eq!(layout.focused(), focus);
        assert_eq!(
            split_snapshot(&layout),
            [
                (Direction::Horizontal, 0.6),
                (Direction::Vertical, 0.4),
                (Direction::Horizontal, 0.5)
            ]
        );
    }

    #[test]
    fn panes_after_split_predicts_the_real_split_geometry() {
        let area = Rect::new(3, 1, 121, 37);
        let (mut layout, root) = TileLayout::new();
        let right = layout.split_pane(root, Direction::Horizontal, 0.6).unwrap();
        let bottom_left = layout.split_pane(root, Direction::Vertical, 0.4).unwrap();
        for (target, direction, ratio) in [
            (bottom_left, Direction::Horizontal, 0.5),
            (right, Direction::Vertical, 0.3),
            (root, Direction::Horizontal, f32::NAN),
        ] {
            let (predicted, new_index) = layout
                .panes_after_split(area, target, direction, ratio)
                .unwrap();
            let new_id = layout.split_pane(target, direction, ratio).unwrap();
            let actual: Vec<_> = layout
                .panes(area)
                .into_iter()
                .map(|info| info.rect)
                .collect();
            assert_eq!(
                predicted.iter().map(|info| info.rect).collect::<Vec<_>>(),
                actual
            );
            assert_eq!(layout.pane_ids()[new_index], new_id);
        }
        assert!(layout
            .panes_after_split(area, PaneId::alloc(), Direction::Vertical, 0.5)
            .is_none());
    }

    #[test]
    #[ignore = "manual BSP allocation and traversal scaling profile"]
    fn bsp_layout_profile() {
        use std::hint::black_box;
        use std::time::{Duration, Instant};
        fn build(count: usize, balanced: bool) -> TileLayout {
            let (mut layout, root) = TileLayout::new();
            let mut leaves = std::collections::VecDeque::from([root]);
            for _ in 1..count {
                let target = leaves.pop_front().unwrap();
                let new = layout
                    .split_pane(target, Direction::Horizontal, 0.5)
                    .unwrap();
                if balanced {
                    leaves.push_back(target);
                }
                leaves.push_back(new);
            }
            layout
        }
        fn measure(mut operation: impl FnMut()) -> f64 {
            for _ in 0..32 {
                operation();
            }
            let mut samples = Vec::new();
            for _ in 0..7 {
                let start = Instant::now();
                let mut iterations = 0;
                while start.elapsed() < Duration::from_millis(20) {
                    operation();
                    iterations += 1;
                }
                samples.push(start.elapsed().as_secs_f64() * 1e6 / f64::from(iterations));
            }
            samples.sort_by(f64::total_cmp);
            samples[3]
        }
        for count in [1, 15, 128, 512] {
            for balanced in [true, false] {
                let layout = build(count, balanced);
                let splits = measure(|| {
                    black_box(layout.splits(Rect::new(0, 0, 120, 40)));
                });
                let build = measure(|| {
                    black_box(build(count, balanced));
                });
                println!("bsp panes={count} balanced={balanced} splits_us={splits:.3} build_us={build:.3}");
            }
        }
    }

    fn pane(id: u32) -> PaneId {
        PaneId::from_raw(id)
    }

    fn sample_layout() -> TileLayout {
        TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.3,
                first: Box::new(Node::Pane(pane(1))),
                second: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.6,
                    first: Box::new(Node::Pane(pane(2))),
                    second: Box::new(Node::Split {
                        direction: Direction::Horizontal,
                        ratio: 0.4,
                        first: Box::new(Node::Pane(pane(3))),
                        second: Box::new(Node::Pane(pane(4))),
                    }),
                }),
            },
            pane(2),
        )
    }

    fn pane_rects(layout: &TileLayout) -> Vec<(PaneId, Rect)> {
        layout
            .panes(Rect::new(0, 0, 100, 40))
            .into_iter()
            .map(|info| (info.id, info.rect))
            .collect()
    }

    fn pane_rect(layout: &TileLayout, pane_id: PaneId) -> Rect {
        pane_rects(layout)
            .into_iter()
            .find_map(|(id, rect)| (id == pane_id).then_some(rect))
            .expect("pane should exist")
    }

    fn split_snapshot(layout: &TileLayout) -> Vec<(Direction, f32)> {
        fn collect(node: &Node, out: &mut Vec<(Direction, f32)>) {
            match node {
                Node::Pane(_) | Node::Stack { .. } => {}
                Node::Split {
                    direction,
                    ratio,
                    first,
                    second,
                } => {
                    out.push((*direction, *ratio));
                    collect(first, out);
                    collect(second, out);
                }
            }
        }

        let mut out = Vec::new();
        collect(layout.root(), &mut out);
        out
    }

    #[test]
    fn swap_panes_exchanges_leaf_ids_without_changing_cells() {
        let mut layout = sample_layout();
        let before_rects = pane_rects(&layout);
        let before_splits = split_snapshot(&layout);

        assert!(layout.swap_panes(pane(2), pane(4)));

        assert_eq!(layout.pane_count(), 4);
        assert_eq!(split_snapshot(&layout), before_splits);
        assert_eq!(layout.focused(), pane(2));

        let after_rects = pane_rects(&layout);
        assert_eq!(after_rects[0], before_rects[0]);
        assert_eq!(after_rects[1], (pane(4), before_rects[1].1));
        assert_eq!(after_rects[2], before_rects[2]);
        assert_eq!(after_rects[3], (pane(2), before_rects[3].1));
    }

    #[test]
    fn swap_panes_is_noop_for_same_or_missing_pane() {
        let mut layout = sample_layout();
        let before_rects = pane_rects(&layout);
        let before_splits = split_snapshot(&layout);
        let before_focus = layout.focused();

        assert!(!layout.swap_panes(pane(2), pane(2)));
        assert!(!layout.swap_panes(pane(2), pane(99)));
        assert!(!layout.swap_panes(pane(99), pane(2)));

        assert_eq!(pane_rects(&layout), before_rects);
        assert_eq!(split_snapshot(&layout), before_splits);
        assert_eq!(layout.focused(), before_focus);
    }

    #[test]
    fn insert_existing_pane_near_target_preserves_existing_ids_and_focuses_moved_pane() {
        let (mut layout, root) = TileLayout::new();
        let moved = pane(99);

        assert!(layout.insert_pane_near(root, moved, Direction::Horizontal, 0.25, true));

        assert_eq!(layout.pane_count(), 2);
        assert_eq!(layout.pane_ids(), vec![root, moved]);
        assert_eq!(layout.focused(), moved);
        let splits = split_snapshot(&layout);
        assert_eq!(splits, vec![(Direction::Horizontal, 0.25)]);
        assert_eq!(pane_rect(&layout, root), Rect::new(0, 0, 25, 40));
        assert_eq!(pane_rect(&layout, moved), Rect::new(25, 0, 75, 40));
    }

    #[test]
    fn split_focused_with_ratio_sets_new_split_ratio() {
        let (mut layout, root) = TileLayout::new();
        layout.focus_pane(root);

        layout.split_focused_with_ratio(Direction::Horizontal, 0.333);

        let splits = split_snapshot(&layout);
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].0, Direction::Horizontal);
        assert!((splits[0].1 - 0.333).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_pane_preserves_focus_and_reports_change() {
        let mut layout = sample_layout();
        let original_focus = layout.focused();

        assert!(layout.resize_pane(pane(1), NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40),));

        assert_eq!(layout.focused(), original_focus);
        let split = split_snapshot(&layout)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.35).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_second_child_toward_split_decreases_ratio() {
        let (mut layout, root) = TileLayout::new();
        let right = layout.split_focused(Direction::Horizontal);
        layout.focus_pane(root);

        assert!(layout.resize_pane(right, NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40),));

        let split = split_snapshot(&layout)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);
        assert_eq!(layout.focused(), root);
    }

    #[test]
    fn resize_outer_edges_shrink_focused_pane() {
        let (mut horizontal, left) = TileLayout::new();
        horizontal.split_focused(Direction::Horizontal);

        assert!(horizontal.resize_pane(left, NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&horizontal)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);

        let (mut horizontal, _left) = TileLayout::new();
        let right = horizontal.split_focused(Direction::Horizontal);

        assert!(horizontal.resize_pane(right, NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&horizontal)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.55).abs() < f32::EPSILON);

        let (mut vertical, top) = TileLayout::new();
        vertical.split_focused(Direction::Vertical);

        assert!(vertical.resize_pane(top, NavDirection::Up, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&vertical)[0];
        assert_eq!(split.0, Direction::Vertical);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);

        let (mut vertical, _top) = TileLayout::new();
        let bottom = vertical.split_focused(Direction::Vertical);

        assert!(vertical.resize_pane(bottom, NavDirection::Down, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&vertical)[0];
        assert_eq!(split.0, Direction::Vertical);
        assert!((split.1 - 0.55).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_outer_edge_falls_back_to_horizontal_ancestor_split() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.6,
                first: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Pane(pane(3))),
            },
            pane(1),
        );
        let before = pane_rect(&layout, pane(1));

        assert!(layout.resize_pane(pane(1), NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40),));

        let after = pane_rect(&layout, pane(1));
        assert_eq!(after.height, before.height);
        assert!(after.width < before.width);
        let splits = split_snapshot(&layout);
        assert_eq!(splits[0].0, Direction::Horizontal);
        assert!((splits[0].1 - 0.55).abs() < f32::EPSILON);
        assert_eq!(splits[1], (Direction::Vertical, 0.5));
    }

    #[test]
    fn resize_outer_edge_falls_back_to_vertical_ancestor_split() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Vertical,
                ratio: 0.6,
                first: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Pane(pane(3))),
            },
            pane(1),
        );
        let before = pane_rect(&layout, pane(1));

        assert!(layout.resize_pane(pane(1), NavDirection::Up, 0.05, Rect::new(0, 0, 100, 40),));

        let after = pane_rect(&layout, pane(1));
        assert_eq!(after.width, before.width);
        assert!(after.height < before.height);
        let splits = split_snapshot(&layout);
        assert_eq!(splits[0].0, Direction::Vertical);
        assert!((splits[0].1 - 0.55).abs() < f32::EPSILON);
        assert_eq!(splits[1], (Direction::Horizontal, 0.5));
    }

    #[test]
    fn resize_uses_split_in_same_branch_when_borders_share_coordinate() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Vertical,
                ratio: 0.5,
                first: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(3))),
                    second: Box::new(Node::Pane(pane(4))),
                }),
            },
            pane(3),
        );

        assert!(layout.resize_pane(pane(3), NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40),));

        let splits = split_snapshot(&layout);
        assert_eq!(splits[0], (Direction::Vertical, 0.5));
        assert_eq!(splits[1], (Direction::Horizontal, 0.5));
        assert_eq!(splits[2].0, Direction::Horizontal);
        assert!((splits[2].1 - 0.55).abs() < f32::EPSILON);
    }

    #[test]
    fn find_in_direction_tiebreaks_by_larger_overlap_before_layout_order() {
        let focused = PaneInfo {
            id: pane(1),
            rect: Rect::new(10, 10, 10, 10),
            inner_rect: Rect::new(10, 10, 10, 10),
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: true,
            stack: None,
        };
        let small_overlap_first = PaneInfo {
            id: pane(2),
            rect: Rect::new(0, 10, 10, 2),
            inner_rect: Rect::new(0, 10, 10, 2),
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: false,
            stack: None,
        };
        let larger_overlap_second = PaneInfo {
            id: pane(3),
            rect: Rect::new(0, 10, 10, 8),
            inner_rect: Rect::new(0, 10, 10, 8),
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: false,
            stack: None,
        };
        let panes = vec![focused.clone(), small_overlap_first, larger_overlap_second];

        assert_eq!(
            find_in_direction(&focused, NavDirection::Left, &panes),
            Some(pane(3))
        );
    }

    #[test]
    fn close_focused_returns_to_the_pane_focus_came_from() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.close_focused());

        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn close_focused_returns_to_the_pane_that_opened_a_split() {
        // Allocated ids only: sample_layout() uses from_raw and shares the id
        // space with the allocator.
        let (mut layout, first) = TileLayout::new();
        let second = layout.split_focused(Direction::Horizontal);
        let third = layout.split_focused(Direction::Vertical);
        assert_eq!(layout.pane_ids().len(), 3);

        layout.focus_pane(first);
        let opened = layout.split_focused(Direction::Horizontal);
        assert_eq!(layout.focused(), opened);

        assert!(layout.close_focused());

        assert_eq!(layout.focused(), first);
        assert!(layout.pane_ids().contains(&second));
        assert!(layout.pane_ids().contains(&third));
    }

    #[test]
    fn closing_a_background_pane_keeps_the_focused_pane_history() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.close_pane(pane(1)));
        assert_eq!(layout.focused(), pane(4));

        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn closing_the_remembered_pane_drops_the_focus_history() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.close_pane(pane(2)));

        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(3));
    }

    #[test]
    fn close_focused_uses_tree_order_without_focus_history() {
        let mut layout = sample_layout();

        assert!(layout.close_focused());

        assert_eq!(layout.focused(), pane(3));
    }

    #[test]
    fn close_focused_does_not_reuse_history_after_it_is_consumed() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2));

        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(3));
    }

    #[test]
    fn resize_does_not_disturb_the_close_focus_target() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));
        layout.resize_pane(pane(1), NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40));

        assert!(layout.close_focused());

        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn split_pane_leaves_focus_and_history_untouched() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        let new_id = layout
            .split_pane(pane(1), Direction::Horizontal, 0.5)
            .expect("target exists");

        assert!(layout.pane_ids().contains(&new_id));
        assert_eq!(layout.focused(), pane(4));
        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn split_pane_missing_target_changes_nothing() {
        let mut layout = sample_layout();
        let ids = layout.pane_ids();

        assert_eq!(
            layout.split_pane(pane(99), Direction::Horizontal, 0.5),
            None
        );

        assert_eq!(layout.pane_ids(), ids);
    }

    #[test]
    fn insert_pane_near_unfocused_keeps_focus_and_history() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        assert!(layout.insert_pane_near(pane(1), pane(9), Direction::Horizontal, 0.5, false));

        assert_eq!(layout.focused(), pane(4));
        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2));
    }

    #[test]
    fn failed_split_rollback_preserves_focus_history() {
        let mut layout = sample_layout();
        layout.focus_pane(pane(4));

        let new_id = layout
            .split_pane(layout.focused(), Direction::Horizontal, 0.5)
            .expect("target exists");
        assert!(layout.close_pane(new_id));

        assert_eq!(layout.focused(), pane(4));
        assert!(layout.close_focused());
        assert_eq!(layout.focused(), pane(2));
    }
}
