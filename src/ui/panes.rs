use ratatui::{
    buffer::Buffer,
    layout::{Direction, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::scrollbar::{render_pane_scrollbar, should_show_scrollbar};
#[cfg(test)]
use super::text::display_width;
use super::text::truncate_end;
use super::widgets::panel_contrast_fg;
use crate::app::state::Palette;
use crate::app::AppState;
use crate::layout::{PaneId, PaneInfo};
use crate::popup_size::resolve_popup_geometry;
use crate::terminal::{TerminalRuntime, TerminalRuntimeRegistry};

pub(crate) fn pane_is_scrolled_back(rt: &TerminalRuntime) -> bool {
    rt.scroll_metrics()
        .is_some_and(|metrics| metrics.offset_from_bottom > 0)
}

fn pane_border_title(label: &str, pane_width: u16, _focused: bool) -> Option<String> {
    let label = label.trim();
    if label.is_empty() || pane_width <= 4 {
        return None;
    }
    let max_label_width = pane_width.saturating_sub(4) as usize;
    Some(format!(" {} ", truncate_end(label, max_label_width)))
}

// Full view computation reaches this helper for active and background panes.
// Keep terminal queries narrow, allocation-free, and short under the core lock.
fn terminal_inner_rect(rt: &TerminalRuntime, pane_inner: Rect, pane_scrollbars: bool) -> Rect {
    terminal_inner_rect_for(pane_inner, pane_scrollbars && !rt.alternate_screen_active())
}

fn terminal_inner_rect_for(pane_inner: Rect, scrollbar_gutter: bool) -> Rect {
    if !scrollbar_gutter || pane_inner.width <= 4 {
        return pane_inner;
    }

    Rect::new(
        pane_inner.x,
        pane_inner.y,
        pane_inner.width.saturating_sub(1),
        pane_inner.height,
    )
}

fn zoomed_pane_borders(app: &AppState, multi_pane: bool) -> Borders {
    if app.pane_borders.shows_borders(multi_pane) && app.pane_outer_borders {
        Borders::ALL
    } else {
        Borders::NONE
    }
}

/// Where a pane that is about to be created will sit in its tab.
#[derive(Debug, Clone, Copy)]
pub(crate) enum NewPanePlacement {
    /// The only pane of a new tab or workspace.
    Alone,
    /// The pane created by splitting `target` in workspace `ws_idx`.
    Split {
        ws_idx: usize,
        target: PaneId,
        direction: Direction,
        ratio: f32,
    },
    /// The pane created by stacking onto `target` in workspace `ws_idx`.
    Stacked { ws_idx: usize, target: PaneId },
    /// A pane split off and immediately zoomed over its tab.
    ZoomedOverlay,
    /// An existing pane in workspace `ws_idx` getting a new terminal.
    Existing { ws_idx: usize, pane: PaneId },
}

/// Terminal rows and columns a new pane gets once its tab is laid out in
/// `area`, so its program starts at that size instead of being resized.
pub(crate) fn new_pane_terminal_size(
    app: &AppState,
    area: Rect,
    placement: NewPanePlacement,
) -> (u16, u16) {
    let laid_out = |panes: Vec<PaneInfo>, index: usize| {
        let infos = apply_pane_chrome(
            panes,
            app.pane_borders,
            app.pane_gaps,
            app.pane_outer_borders,
        );
        pane_sizing_rect(&infos[index])
    };
    // A lone pane has no neighbors, so its chrome matches a zoomed single pane.
    let alone = || pane_inner_rect(area, zoomed_pane_borders(app, false));
    let tab_for = |ws_idx: usize, pane: PaneId| {
        let ws = app.workspaces.get(ws_idx)?;
        ws.tabs.get(ws.find_tab_index_for_pane(pane)?)
    };
    let pane_inner = match placement {
        NewPanePlacement::Alone => alone(),
        NewPanePlacement::Existing { ws_idx, pane } => tab_for(ws_idx, pane)
            .and_then(|tab| {
                if tab.zoomed && tab.layout.focused() == pane {
                    let multi_pane = tab.layout.pane_count() > 1;
                    return Some(pane_inner_rect(area, zoomed_pane_borders(app, multi_pane)));
                }
                let panes = tab.layout.panes(area);
                let index = panes.iter().position(|info| info.id == pane)?;
                Some(laid_out(panes, index))
            })
            .unwrap_or_else(alone),
        NewPanePlacement::ZoomedOverlay => pane_inner_rect(area, zoomed_pane_borders(app, true)),
        NewPanePlacement::Split {
            ws_idx,
            target,
            direction,
            ratio,
        } => tab_for(ws_idx, target)
            .and_then(|tab| tab.layout.panes_after_split(area, target, direction, ratio))
            .map(|(panes, new_index)| laid_out(panes, new_index))
            .unwrap_or_else(alone),
        NewPanePlacement::Stacked { ws_idx, target } => tab_for(ws_idx, target)
            .and_then(|tab| tab.layout.panes_after_stack(area, target))
            .map(|(panes, new_index)| laid_out(panes, new_index))
            .unwrap_or_else(alone),
    };
    new_terminal_size(app, pane_inner)
}

/// Terminal rows and columns for every pane of `layout` laid out in `area`, in
/// pane order, so a multi-pane layout can start each program at its final size.
pub(crate) fn new_layout_terminal_sizes(
    app: &AppState,
    area: Rect,
    layout: &crate::layout::TileLayout,
) -> Vec<(u16, u16)> {
    apply_pane_chrome(
        layout.panes(area),
        app.pane_borders,
        app.pane_gaps,
        app.pane_outer_borders,
    )
    .into_iter()
    .map(|info| new_terminal_size(app, pane_sizing_rect(&info)))
    .collect()
}

fn new_terminal_size(app: &AppState, pane_inner: Rect) -> (u16, u16) {
    // A new program starts on the primary screen, which reserves the gutter.
    let inner = terminal_inner_rect_for(pane_inner, app.pane_scrollbars);
    (
        inner.height.max(crate::pane::MIN_PANE_ROWS),
        inner.width.max(crate::pane::MIN_PANE_COLS),
    )
}

pub(crate) fn pane_inner_rect(area: Rect, borders: Borders) -> Rect {
    if borders.is_empty() {
        area
    } else {
        Block::default().borders(borders).inner(area)
    }
}

fn ranges_overlap(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> bool {
    a_start < b_start.saturating_add(b_len) && b_start < a_start.saturating_add(a_len)
}

/// The rect chrome is decided for: a stacked pane is placed by the region its stack
/// occupies rather than by its own row.
fn chrome_rect(info: &PaneInfo) -> Rect {
    info.stack.map_or(info.rect, |slot| slot.region_rect)
}

fn pane_to_right<'a>(info: &PaneInfo, panes: &'a [PaneInfo]) -> Option<&'a PaneInfo> {
    let rect = chrome_rect(info);
    let right = rect.x.saturating_add(rect.width);
    panes.iter().find(|other| {
        let other_rect = chrome_rect(other);
        other.id != info.id
            && other_rect.x == right
            && ranges_overlap(rect.y, rect.height, other_rect.y, other_rect.height)
    })
}

fn pane_below<'a>(info: &PaneInfo, panes: &'a [PaneInfo]) -> Option<&'a PaneInfo> {
    let rect = chrome_rect(info);
    let bottom = rect.y.saturating_add(rect.height);
    panes.iter().find(|other| {
        let other_rect = chrome_rect(other);
        other.id != info.id
            && other_rect.y == bottom
            && ranges_overlap(rect.x, rect.width, other_rect.x, other_rect.width)
    })
}

fn shrink_for_one_cell_gap(size: u16) -> u16 {
    if size > 1 {
        size - 1
    } else {
        size
    }
}

pub(crate) fn apply_pane_chrome(
    panes: Vec<PaneInfo>,
    pane_borders: crate::config::PaneBordersConfig,
    pane_gaps: bool,
    pane_outer_borders: bool,
) -> Vec<PaneInfo> {
    let multi_pane = panes.len() > 1;
    let bordered = pane_borders.shows_borders(multi_pane);
    let outer_left = panes.iter().map(|info| info.rect.x).min().unwrap_or(0);
    let outer_top = panes.iter().map(|info| info.rect.y).min().unwrap_or(0);
    let outer_right = panes
        .iter()
        .map(|info| info.rect.x.saturating_add(info.rect.width))
        .max()
        .unwrap_or(0);
    let outer_bottom = panes
        .iter()
        .map(|info| info.rect.y.saturating_add(info.rect.height))
        .max()
        .unwrap_or(0);
    let mut chromed: Vec<PaneInfo> = panes
        .iter()
        .cloned()
        .map(|mut info| {
            let right_neighbor = multi_pane.then(|| pane_to_right(&info, &panes)).flatten();
            let below_neighbor = multi_pane.then(|| pane_below(&info, &panes)).flatten();
            // Borders are decided for the region of a whole stack, so every member ends
            // up with the same chrome.
            let rect = chrome_rect(&info);

            if multi_pane && pane_gaps && !pane_borders.draws_borders() {
                // A stack takes no gap between its own members; the gap belongs to the
                // region, which the members are laid out inside afterwards.
                let gapped = match info.stack.as_mut() {
                    Some(slot) => &mut slot.region_rect,
                    None => &mut info.rect,
                };
                if right_neighbor.is_some() {
                    gapped.width = shrink_for_one_cell_gap(gapped.width);
                }
                if below_neighbor.is_some() {
                    gapped.height = shrink_for_one_cell_gap(gapped.height);
                }
            }

            info.borders = if !bordered {
                Borders::NONE
            } else {
                let mut borders = Borders::ALL;
                if !pane_gaps {
                    if right_neighbor.is_some() {
                        borders.remove(Borders::RIGHT);
                    }
                    if below_neighbor.is_some() {
                        borders.remove(Borders::BOTTOM);
                    }
                }
                if !pane_outer_borders {
                    if rect.x == outer_left {
                        borders.remove(Borders::LEFT);
                    }
                    if rect.y == outer_top {
                        borders.remove(Borders::TOP);
                    }
                    if rect.x.saturating_add(rect.width) == outer_right {
                        borders.remove(Borders::RIGHT);
                    }
                    if rect.y.saturating_add(rect.height) == outer_bottom {
                        borders.remove(Borders::BOTTOM);
                    }
                }
                borders
            };
            info
        })
        .collect();

    relayout_stacks_inside_chrome(&mut chromed);
    chromed
}

/// Re-fit every stack's members inside the chrome its region was given.
///
/// Chrome is derived from neighbour geometry, so applying it per member would give
/// the top, middle and bottom of a stack different borders — and therefore different
/// terminal heights as the active member moves. Instead the stack gets one box around
/// the whole region, carried by the active member so the existing border-joining code
/// draws and joins it exactly like any other pane's. Collapsed members are plain rows
/// inside that box with no chrome of their own, and every member's terminal is sized
/// to the same content rect.
fn relayout_stacks_inside_chrome(panes: &mut [PaneInfo]) {
    // This runs for every view computation on every attached client, so a layout
    // without stacks must cost one scan and no allocation.
    if panes.iter().all(|info| info.stack.is_none()) {
        return;
    }

    let mut regions: Vec<Rect> = Vec::new();
    for info in panes.iter() {
        if let Some(slot) = info.stack {
            if !regions.contains(&slot.region_rect) {
                regions.push(slot.region_rect);
            }
        }
    }

    for region in regions {
        let members: Vec<usize> = panes
            .iter()
            .enumerate()
            .filter(|(_, info)| info.stack.is_some_and(|slot| slot.region_rect == region))
            .map(|(index, _)| index)
            .collect();
        let Some(&first) = members.first() else {
            continue;
        };
        // A region with no rows has no top border to reclaim; widening the band anyway
        // would hand the first member a title row belonging to a neighbouring pane.
        if region.height == 0 {
            continue;
        }
        // Every member got the same chrome because it was decided for the region.
        let borders = panes[first].borders;
        let active = members
            .iter()
            .position(|index| panes[*index].stack.is_some_and(|slot| !slot.collapsed))
            .unwrap_or(0);
        let inner = pane_inner_rect(region, borders);
        // The first member's title row is the region's own top border, so a stack costs
        // one row less than a box plus a row per member and the title sits on the frame.
        let band = if borders.contains(Borders::TOP) {
            Rect::new(
                inner.x,
                region.y,
                inner.width,
                inner.height.saturating_add(1),
            )
        } else {
            inner
        };
        let heights = crate::layout::stack_heights(band.height, members.len(), active);
        let content_rect = crate::layout::stack_content_rect(band, &heights, active).0;

        let mut y = band.y;
        for (position, &index) in members.iter().enumerate() {
            let height = heights[position];
            let rect = Rect::new(band.x, y, band.width, height);
            y = y.saturating_add(height);
            let is_active = position == active;
            let info = &mut panes[index];
            // The active member owns the region's box; collapsed rows sit inside it.
            info.rect = if is_active { region } else { rect };
            info.borders = if is_active { borders } else { Borders::NONE };
            if let Some(slot) = info.stack.as_mut() {
                slot.collapsed = !is_active;
                slot.index = position;
                slot.header_rect = Rect::new(band.x, rect.y, band.width, height.min(1));
                slot.content_rect = content_rect;
                slot.region_borders = borders;
            }
        }
    }
}

/// The rect a pane's terminal is sized to.
///
/// For a stacked pane this is the region the active member occupies, already inside
/// the stack's chrome — so every member of a stack reports the same size and cycling
/// the stack resizes nothing.
fn pane_sizing_rect(info: &PaneInfo) -> Rect {
    match info.stack {
        Some(slot) => slot.content_rect,
        None => pane_inner_rect(info.rect, info.borders),
    }
}

fn runtime_for_tab_pane<'a>(
    _app: &'a AppState,
    terminal_runtimes: &'a TerminalRuntimeRegistry,
    _workspace_index: usize,
    tab: &'a crate::workspace::Tab,
    pane_id: crate::layout::PaneId,
) -> Option<(&'a crate::terminal::TerminalId, &'a TerminalRuntime)> {
    let terminal_id = tab.terminal_id(pane_id)?;
    #[cfg(test)]
    if let Some(runtime) = _app
        .workspaces
        .get(_workspace_index)?
        .test_runtimes
        .get(&pane_id)
        .or_else(|| tab.runtimes.get(&pane_id))
    {
        return Some((terminal_id, runtime));
    }
    terminal_runtimes
        .get(terminal_id)
        .map(|runtime| (terminal_id, runtime))
}

fn stable_scrollbar_gutter(
    rt: &TerminalRuntime,
    pane_inner: Rect,
    pane_scrollbars: bool,
) -> (Rect, Option<Rect>) {
    let inner_rect = terminal_inner_rect(rt, pane_inner, pane_scrollbars);
    if inner_rect == pane_inner {
        return (inner_rect, None);
    }
    let gutter = Rect::new(
        pane_inner.x + pane_inner.width.saturating_sub(1),
        pane_inner.y,
        1,
        pane_inner.height,
    );
    let scrollbar_rect = rt
        .scroll_metrics()
        .filter(|metrics| should_show_scrollbar(*metrics))
        .map(|_| gutter);

    (inner_rect, scrollbar_rect)
}

/// Resize every visible runtime in a tab to the geometry it would receive if the tab were selected.
pub(super) fn resize_tab_panes(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    workspace_index: usize,
    tab: &crate::workspace::Tab,
    area: Rect,
    cell_size: crate::kitty_graphics::HostCellSize,
) {
    let multi_pane = tab.layout.pane_count() > 1;

    if tab.zoomed {
        let focused_id = tab.layout.focused();
        if let Some((terminal_id, rt)) =
            runtime_for_tab_pane(app, terminal_runtimes, workspace_index, tab, focused_id)
        {
            let pane_inner = pane_inner_rect(area, zoomed_pane_borders(app, multi_pane));
            let inner_rect = terminal_inner_rect(rt, pane_inner, app.pane_scrollbars);
            if !app.direct_attach_resize_locks.contains(terminal_id) {
                rt.resize(
                    inner_rect.height,
                    inner_rect.width,
                    cell_size.width_px,
                    cell_size.height_px,
                );
            }
        }
        return;
    }

    for info in apply_pane_chrome(
        tab.layout.panes(area),
        app.pane_borders,
        app.pane_gaps,
        app.pane_outer_borders,
    ) {
        let pane_inner = pane_sizing_rect(&info);

        if let Some((terminal_id, rt)) =
            runtime_for_tab_pane(app, terminal_runtimes, workspace_index, tab, info.id)
        {
            let inner_rect = terminal_inner_rect(rt, pane_inner, app.pane_scrollbars);
            if !app.direct_attach_resize_locks.contains(terminal_id) {
                rt.resize(
                    inner_rect.height,
                    inner_rect.width,
                    cell_size.width_px,
                    cell_size.height_px,
                );
            }
        }
    }
}

/// Compute pane layout info and optionally resize pane runtimes to match.
pub(super) fn compute_pane_infos_for_tab(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    ws_idx: usize,
    tab_idx: usize,
    area: Rect,
    resize_panes: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
) -> Vec<PaneInfo> {
    let Some(tab) = app
        .workspaces
        .get(ws_idx)
        .and_then(|workspace| workspace.tabs.get(tab_idx))
    else {
        return Vec::new();
    };

    let multi_pane = tab.layout.pane_count() > 1;

    if tab.zoomed {
        let focused_id = tab.layout.focused();
        let borders = zoomed_pane_borders(app, multi_pane);
        let pane_inner = pane_inner_rect(area, borders);
        let mut inner_rect = pane_inner;
        let mut scrollbar_rect = None;
        if let Some(rt) = app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, focused_id) {
            (inner_rect, scrollbar_rect) =
                stable_scrollbar_gutter(rt, pane_inner, app.pane_scrollbars);
            if resize_panes
                && tab.terminal_id(focused_id).is_some_and(|terminal_id| {
                    !app.direct_attach_resize_locks.contains(terminal_id)
                })
            {
                rt.resize(
                    inner_rect.height,
                    inner_rect.width,
                    cell_size.width_px,
                    cell_size.height_px,
                );
            }
        }
        return vec![PaneInfo {
            id: focused_id,
            rect: area,
            inner_rect,
            scrollbar_rect,
            borders,
            is_focused: true,
            stack: None,
        }];
    }

    let mut pane_infos = apply_pane_chrome(
        tab.layout.panes(area),
        app.pane_borders,
        app.pane_gaps,
        app.pane_outer_borders,
    );

    // Every member of a stack is sized to the region the active member occupies, so
    // cycling a stack never resizes a PTY and collapsed agents keep their screen.
    for info in &mut pane_infos {
        let pane_inner = pane_sizing_rect(info);

        let mut inner_rect = pane_inner;
        let mut scrollbar_rect = None;
        if let Some(rt) = app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id) {
            (inner_rect, scrollbar_rect) =
                stable_scrollbar_gutter(rt, pane_inner, app.pane_scrollbars);
            if resize_panes
                && tab.terminal_id(info.id).is_some_and(|terminal_id| {
                    !app.direct_attach_resize_locks.contains(terminal_id)
                })
            {
                rt.resize(
                    inner_rect.height,
                    inner_rect.width,
                    cell_size.width_px,
                    cell_size.height_px,
                );
            }
        }

        if let Some(slot) = info.stack.filter(|slot| slot.collapsed) {
            // A collapsed member shows only its title row and has no content rows.
            // Clients forward clicks inside `inner_rect` to the pane's program and the
            // retained renderer patches it, so it must be empty, not the title row.
            info.inner_rect = Rect::new(
                slot.header_rect.x,
                slot.header_rect.y,
                slot.header_rect.width,
                0,
            );
            info.scrollbar_rect = None;
        } else {
            info.inner_rect = inner_rect;
            info.scrollbar_rect = scrollbar_rect;
        }
    }

    pane_infos
}

#[cfg(test)]
fn compute_pane_infos(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    area: Rect,
    resize_panes: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
) -> Vec<PaneInfo> {
    let Some(workspace_index) = app.active else {
        return Vec::new();
    };
    let Some(tab_index) = app
        .workspaces
        .get(workspace_index)
        .map(crate::workspace::Workspace::active_tab_index)
    else {
        return Vec::new();
    };
    compute_pane_infos_for_tab(
        app,
        terminal_runtimes,
        workspace_index,
        tab_index,
        area,
        resize_panes,
        cell_size,
    )
}

pub(super) fn render_panes(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    target: Option<super::tab_surface::TabSurfaceTarget>,
    pane_infos: &[PaneInfo],
    split_borders: &[crate::layout::SplitBorder],
) {
    let Some(target) = target else {
        return;
    };
    let ws_idx = target.workspace_index;
    let Some(ws) = app.workspaces.get(ws_idx) else {
        return;
    };

    for info in pane_infos {
        // A collapsed stack member shows a title row instead of its terminal, even
        // though its PTY stays full size behind the row.
        if info.stack.is_some_and(|slot| slot.collapsed) {
            continue;
        }
        if let Some(rt) = app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id) {
            let show_cursor = info.is_focused
                && !pane_is_scrolled_back(rt)
                && app.pane_exposes_host_cursor(ws_idx, info.id);
            rt.render(frame, info.inner_rect, show_cursor);
            render_pane_scrollbar(app, frame, info, rt);
        } else if let Some(reason) = ws
            .tabs
            .get(target.tab_index)
            .and_then(|tab| tab.terminal_id(info.id))
            .and_then(|id| app.terminals.get(id))
            .and_then(|terminal| terminal.restore_error.as_deref())
        {
            frame.render_widget(
                Paragraph::new(reason).wrap(Wrap { trim: false }),
                info.inner_rect,
            );
        }
    }

    render_pane_borders(app, ws, pane_infos, split_borders, frame);
    render_stack_bars(app, ws, pane_infos, frame);
}

/// The name drawn on a stack member's title row.
fn stack_bar_label(
    app: &AppState,
    ws: &crate::workspace::Workspace,
    info: &PaneInfo,
    slot: crate::layout::StackSlot,
) -> String {
    // A title row is the only name a collapsed pane has, so fall back past the pane
    // name to the agent type before resorting to a positional name.
    let terminal = ws
        .pane_state(info.id)
        .and_then(|pane| app.terminals.get(&pane.attached_terminal_id));
    terminal
        .and_then(|terminal| terminal.display_name())
        .or_else(|| {
            terminal
                .and_then(|terminal| terminal.border_label(app.show_agent_labels_on_pane_borders))
        })
        .unwrap_or_else(|| match ws.public_pane_number(info.id) {
            Some(number) => format!("pane {number}"),
            None => format!("pane {}", slot.index + 1),
        })
}

/// The text of a stack member's title row and the cells it may use, starting one cell
/// in from the row's left edge. `None` when the row has no room for a name.
pub(crate) fn stack_bar_title(
    app: &AppState,
    ws: &crate::workspace::Workspace,
    info: &PaneInfo,
    available: usize,
) -> Option<String> {
    let slot = info.stack?;
    // A member's title row is the only place its name appears, so a narrow stack
    // drops the spacing rather than the name.
    let padding = if available > STACK_BAR_TITLE_PADDING {
        STACK_BAR_TITLE_PADDING
    } else {
        STACK_BAR_MARKER_WIDTH
    };
    if available <= padding {
        return None;
    }
    let label = stack_bar_label(app, ws, info, slot);
    let marker = if slot.collapsed { '▸' } else { '▾' };
    let name = truncate_end(label.trim(), available.saturating_sub(padding));
    Some(if padding == STACK_BAR_TITLE_PADDING {
        format!(" {marker} {name} ")
    } else {
        format!("{marker} {name}")
    })
}

/// The cells a stack member's title text covers on its title row, when that row is
/// the region's top border and so shares its cells with a split resize handle.
pub(crate) fn stack_border_title_span(
    app: &AppState,
    ws: &crate::workspace::Workspace,
    info: &PaneInfo,
) -> Option<(u16, u16)> {
    let slot = info.stack?;
    let row = slot.header_rect;
    if row.height == 0 || row.y != slot.region_rect.y || !slot.region_borders.contains(Borders::TOP)
    {
        return None;
    }
    let start_x = row.x.saturating_add(1);
    let available = row.x.saturating_add(row.width).saturating_sub(start_x) as usize;
    let title = stack_bar_title(app, ws, info, available)?;
    let width = super::text::display_width(&title).min(available) as u16;
    Some((start_x, start_x.saturating_add(width)))
}

/// Draw the title row every stack member carries, the visible one included.
fn render_stack_bars(
    app: &AppState,
    ws: &crate::workspace::Workspace,
    pane_infos: &[PaneInfo],
    frame: &mut Frame,
) {
    let buf = frame.buffer_mut();
    let buf_area = buf.area;
    for info in pane_infos {
        let Some(slot) = info.stack else {
            continue;
        };
        let row = slot.header_rect;
        if row.width == 0 || row.height == 0 {
            continue;
        }
        if row.y < buf_area.y || row.y >= buf_area.y.saturating_add(buf_area.height) {
            continue;
        }
        if row.x < buf_area.x {
            continue;
        }
        let end_x = row
            .x
            .saturating_add(row.width)
            .min(buf_area.x.saturating_add(buf_area.width));
        if row.x >= end_x {
            continue;
        }

        let style = stack_bar_style(&app.palette, slot.collapsed);
        let region = slot.region_rect;
        // The first member's row is the region's own top border, already drawn by the
        // border pass — as a rule, and at junctions with a neighbouring split as `┬`
        // or `┴`. That row is shared with the pane above, so redrawing or recolouring
        // it would erase a junction and repaint another pane's edge. Only the title
        // text goes there. Every other row belongs to the stack alone: it is ruled and
        // opens its own corner, which is what makes a stack read as a deck of cards
        // rather than as one pane chopped into strips.
        if !slot.region_borders.contains(Borders::TOP) || row.y != region.y {
            for x in row.x..end_x {
                let cell = &mut buf[(x, row.y)];
                cell.reset();
                cell.set_symbol("─");
                cell.set_style(style);
            }

            let right_x = region.x.saturating_add(region.width).saturating_sub(1);
            for (x, side, joint) in [
                (region.x, Borders::LEFT, "┌"),
                (right_x, Borders::RIGHT, "┐"),
            ] {
                if !slot.region_borders.contains(side)
                    || x < buf_area.x
                    || x >= buf_area.x.saturating_add(buf_area.width)
                {
                    continue;
                }
                let cell = &mut buf[(x, row.y)];
                cell.set_symbol(joint);
                cell.set_style(style);
            }
        }

        // The rule runs edge to edge with the name inset by one, so a title row reads
        // as a frame line rather than a row of text: `┌─ ▸ name ─────┐`.
        let start_x = row.x.saturating_add(1);
        let available = end_x.saturating_sub(start_x) as usize;
        let Some(title) = stack_bar_title(app, ws, info, available) else {
            continue;
        };
        buf.set_stringn(start_x, row.y, title, available, style);
    }
}

/// Cells `▸ name` spends on everything but the name itself.
const STACK_BAR_MARKER_WIDTH: usize = 2;

/// Cells ` ▸ name ` spends on everything but the name itself.
const STACK_BAR_TITLE_PADDING: usize = 4;

/// How much of the panel background is mixed into the accent for a collapsed member's
/// title row. Tuned by eye against Catppuccin Mocha; one number to change.
const COLLAPSED_STACK_BAR_MUTE: f32 = 0.55;

/// Colour of a stack member's title row.
///
/// The visible member takes the accent; a collapsed member takes a muted version of
/// that same accent rather than the generic border grey, so a stack reads as one group
/// of related panes instead of one live pane beside some dead ones.
fn stack_bar_style(p: &Palette, collapsed: bool) -> Style {
    if !collapsed {
        return Style::default().fg(p.accent).add_modifier(Modifier::BOLD);
    }
    let muted = color_to_rgb(p.accent)
        .zip(color_to_rgb(selection_palette_background(p)))
        .map(|(accent, background)| {
            let (r, g, b) = mix_rgb(accent, background, COLLAPSED_STACK_BAR_MUTE);
            Color::Rgb(r, g, b)
        });
    match muted {
        // A named or indexed accent has no channels to mix, so dim it instead.
        Some(color) => Style::default().fg(color),
        None => Style::default().fg(p.accent).add_modifier(Modifier::DIM),
    }
}

pub(crate) fn popup_pane_rects(app: &AppState, area: Rect) -> Option<(Rect, Rect)> {
    let popup = app.popup_pane.as_ref()?;
    resolve_popup_geometry(popup.width, popup.height, area)
        .map(|geometry| (geometry.outer, geometry.inner))
}

pub(super) fn resize_popup_pane(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    area: Rect,
    cell_size: crate::kitty_graphics::HostCellSize,
) {
    let Some(popup) = app.popup_pane.as_ref() else {
        return;
    };
    let Some((_outer, inner)) = popup_pane_rects(app, area) else {
        return;
    };
    if app.direct_attach_resize_locks.contains(&popup.terminal_id) {
        return;
    }
    if let Some(rt) = terminal_runtimes.get(&popup.terminal_id) {
        rt.resize(
            inner.height,
            inner.width,
            cell_size.width_px,
            cell_size.height_px,
        );
    }
}

#[derive(Clone, Copy, Default)]
struct LineCell {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
}

fn render_pane_borders(
    app: &AppState,
    ws: &crate::workspace::Workspace,
    pane_infos: &[PaneInfo],
    split_borders: &[crate::layout::SplitBorder],
    frame: &mut Frame,
) {
    if !app.pane_borders.draws_borders() || pane_infos.iter().all(|info| info.borders.is_empty()) {
        return;
    }

    let mut cells = std::collections::HashMap::<(u16, u16), LineCell>::new();
    for info in pane_infos {
        add_pane_border_cells(&mut cells, info);
    }
    add_split_border_cells(app.pane_gaps, split_borders, &mut cells);

    let buf = frame.buffer_mut();
    let area = buf.area;
    for ((x, y), line) in cells {
        if x < area.x
            || x >= area.x.saturating_add(area.width)
            || y < area.y
            || y >= area.y.saturating_add(area.height)
        {
            continue;
        }
        let focused = pane_infos
            .iter()
            .any(|info| info.is_focused && line_touches_pane(x, y, info, app.pane_gaps));
        let symbol = line_cell_symbol(line);
        if symbol.is_empty() {
            continue;
        }
        let cell = &mut buf[(x, y)];
        cell.set_symbol(symbol);
        let color = if focused {
            app.palette.accent
        } else {
            app.palette.overlay0
        };
        cell.set_style(Style::default().fg(color));
    }

    render_pane_border_titles(app, ws, pane_infos, frame);
}

fn add_split_border_cells(
    pane_gaps: bool,
    split_borders: &[crate::layout::SplitBorder],
    cells: &mut std::collections::HashMap<(u16, u16), LineCell>,
) {
    if pane_gaps {
        return;
    }

    for split in split_borders {
        match split.direction {
            ratatui::layout::Direction::Horizontal => {
                let x = split.pos;
                let end = split.area.y.saturating_add(split.area.height);
                for y in split.area.y..=end {
                    if !cells.contains_key(&(x, y)) {
                        continue;
                    }
                    let left = x
                        .checked_sub(1)
                        .and_then(|left_x| cells.get(&(left_x, y)))
                        .is_some_and(|cell| cell.left || cell.right);
                    let right = cells
                        .get(&(x.saturating_add(1), y))
                        .is_some_and(|cell| cell.left || cell.right);
                    let cell = cells.entry((x, y)).or_default();
                    cell.up |= y > split.area.y;
                    cell.down |= y + 1 < end;
                    cell.left |= left;
                    cell.right |= right;
                }
            }
            ratatui::layout::Direction::Vertical => {
                let y = split.pos;
                let end = split.area.x.saturating_add(split.area.width);
                for x in split.area.x..=end {
                    if !cells.contains_key(&(x, y)) {
                        continue;
                    }
                    let up = y
                        .checked_sub(1)
                        .and_then(|up_y| cells.get(&(x, up_y)))
                        .is_some_and(|cell| cell.up || cell.down);
                    let down = cells
                        .get(&(x, y.saturating_add(1)))
                        .is_some_and(|cell| cell.up || cell.down);
                    let cell = cells.entry((x, y)).or_default();
                    cell.left |= x > split.area.x;
                    cell.right |= x + 1 < end;
                    cell.up |= up;
                    cell.down |= down;
                }
            }
        }
    }
}

fn add_pane_border_cells(
    cells: &mut std::collections::HashMap<(u16, u16), LineCell>,
    info: &PaneInfo,
) {
    let rect = info.rect;
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let right = rect.x.saturating_add(rect.width).saturating_sub(1);
    let bottom = rect.y.saturating_add(rect.height).saturating_sub(1);

    if info.borders.contains(Borders::TOP) {
        for x in rect.x..=right {
            let cell = cells.entry((x, rect.y)).or_default();
            cell.left |= x > rect.x;
            cell.right |= x < right;
        }
    }
    if info.borders.contains(Borders::BOTTOM) {
        for x in rect.x..=right {
            let cell = cells.entry((x, bottom)).or_default();
            cell.left |= x > rect.x;
            cell.right |= x < right;
        }
    }
    if info.borders.contains(Borders::LEFT) {
        for y in rect.y..=bottom {
            let cell = cells.entry((rect.x, y)).or_default();
            cell.up |= y > rect.y;
            cell.down |= y < bottom;
        }
    }
    if info.borders.contains(Borders::RIGHT) {
        for y in rect.y..=bottom {
            let cell = cells.entry((right, y)).or_default();
            cell.up |= y > rect.y;
            cell.down |= y < bottom;
        }
    }
}

fn line_touches_pane(x: u16, y: u16, info: &PaneInfo, pane_gaps: bool) -> bool {
    let rect = info.rect;
    if rect.width == 0 || rect.height == 0 {
        return false;
    }
    let right = rect.x.saturating_add(rect.width).saturating_sub(1);
    let bottom = rect.y.saturating_add(rect.height).saturating_sub(1);
    let in_rows = y >= rect.y && y <= bottom;
    let in_cols = x >= rect.x && x <= right;
    let own_border =
        (in_rows && (x == rect.x || x == right)) || (in_cols && (y == rect.y || y == bottom));

    if pane_gaps {
        return own_border;
    }

    let shared_right = rect.x.saturating_add(rect.width);
    let shared_bottom = rect.y.saturating_add(rect.height);
    own_border
        || (in_rows && x == shared_right)
        || (in_cols && y == shared_bottom)
        || (x == shared_right && y == shared_bottom)
}

fn render_pane_border_titles(
    app: &AppState,
    ws: &crate::workspace::Workspace,
    pane_infos: &[PaneInfo],
    frame: &mut Frame,
) {
    let buf = frame.buffer_mut();
    let area = buf.area;
    for info in pane_infos {
        // A stack member is named by its own title row, which is drawn on the frame
        // line itself; a border title here would repeat it one row above.
        if info.stack.is_some() || !info.borders.contains(Borders::TOP) || info.rect.width <= 4 {
            continue;
        }
        let Some(title) = ws
            .pane_state(info.id)
            .and_then(|pane| app.terminals.get(&pane.attached_terminal_id))
            .and_then(|terminal| terminal.border_label(app.show_agent_labels_on_pane_borders))
            .and_then(|label| pane_border_title(&label, info.rect.width, info.is_focused))
        else {
            continue;
        };
        let y = info.rect.y;
        if y < area.y || y >= area.y.saturating_add(area.height) {
            continue;
        }
        let start_x = info.rect.x.saturating_add(1);
        let end_x = info
            .rect
            .x
            .saturating_add(info.rect.width)
            .saturating_sub(1)
            .min(area.x.saturating_add(area.width));
        if start_x >= end_x {
            continue;
        }
        let color = if info.is_focused {
            app.palette.accent
        } else {
            app.palette.overlay0
        };
        let mut style = Style::default().fg(color);
        if info.is_focused {
            style = style.add_modifier(Modifier::BOLD);
        }
        buf.set_stringn(
            start_x,
            y,
            title,
            end_x.saturating_sub(start_x) as usize,
            style,
        );
    }
}

fn line_cell_symbol(line: LineCell) -> &'static str {
    match (line.up, line.down, line.left, line.right) {
        (true, true, true, true) => "┼",
        (true, true, true, false) => "┤",
        (true, true, false, true) => "├",
        (true, false, true, true) => "┴",
        (false, true, true, true) => "┬",
        (true, true, false, false) | (true, false, false, false) | (false, true, false, false) => {
            "│"
        }
        (false, false, true, true) | (false, false, true, false) | (false, false, false, true) => {
            "─"
        }
        (false, true, false, true) => "┌",
        (false, true, true, false) => "┐",
        (true, false, false, true) => "└",
        (true, false, true, false) => "┘",
        _ => "",
    }
}

pub(crate) fn render_selection_highlight<P: PartialEq>(
    selection: Option<&crate::selection::Selection<P>>,
    buffer: &mut Buffer,
    pane_id: &P,
    inner: Rect,
    scroll_metrics: Option<crate::pane::ScrollMetrics>,
    p: &Palette,
    host_theme: crate::terminal_theme::TerminalTheme,
) {
    let Some(selection) =
        selection.filter(|selection| selection.is_visible() && &selection.pane_id == pane_id)
    else {
        return;
    };
    let style = automatic_selection_style(p, host_theme);
    for y in 0..inner.height {
        for x in 0..inner.width {
            if selection.contains(y, x, scroll_metrics) {
                buffer[(inner.x + x, inner.y + y)].set_style(style);
            }
        }
    }
}

type Rgb = (u8, u8, u8);

fn automatic_selection_style(
    p: &Palette,
    host_theme: crate::terminal_theme::TerminalTheme,
) -> Style {
    let bg = automatic_selection_bg(p, host_theme);
    Style::reset().fg(selection_fg_for_bg(bg, p)).bg(bg)
}

fn automatic_selection_bg(p: &Palette, host_theme: crate::terminal_theme::TerminalTheme) -> Color {
    let fallback = selection_palette_background(p);
    let Some(background) = host_theme
        .background
        .map(|color| (color.r, color.g, color.b))
        .or(match fallback {
            Color::Rgb(r, g, b) => Some((r, g, b)),
            _ => None,
        })
    else {
        return fallback;
    };

    let target = if relative_luminance(background) < 0.5 {
        (255, 255, 255)
    } else {
        (0, 0, 0)
    };
    let selected = mix_rgb(background, target, 0.28);
    Color::Rgb(selected.0, selected.1, selected.2)
}

fn selection_palette_background(p: &Palette) -> Color {
    if p.panel_bg == Color::Reset {
        p.surface_dim
    } else {
        p.panel_bg
    }
}

fn selection_fg_for_bg(bg: Color, p: &Palette) -> Color {
    if let Color::Rgb(r, g, b) = bg {
        let luminance = relative_luminance((r, g, b));
        let black_contrast = (luminance + 0.05) / 0.05;
        let white_contrast = 1.05 / (luminance + 0.05);
        return if black_contrast > white_contrast {
            Color::Rgb(0, 0, 0)
        } else {
            Color::Rgb(255, 255, 255)
        };
    }

    color_to_rgb(bg)
        .map(|bg| {
            if relative_luminance(bg) < 0.5 {
                Color::White
            } else {
                Color::Black
            }
        })
        .unwrap_or_else(|| panel_contrast_fg(p))
}

fn mix_rgb(base: Rgb, target: Rgb, amount: f32) -> Rgb {
    fn channel(base: u8, target: u8, amount: f32) -> u8 {
        (f32::from(base) + (f32::from(target) - f32::from(base)) * amount).round() as u8
    }
    (
        channel(base.0, target.0, amount),
        channel(base.1, target.1, amount),
        channel(base.2, target.2, amount),
    )
}

fn relative_luminance(color: Rgb) -> f32 {
    fn channel(value: u8) -> f32 {
        let value = f32::from(value) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(color.0) + 0.7152 * channel(color.1) + 0.0722 * channel(color.2)
}

fn color_to_rgb(color: Color) -> Option<Rgb> {
    match color {
        Color::Reset => None,
        Color::Black => Some((0, 0, 0)),
        Color::Red => Some((128, 0, 0)),
        Color::Green => Some((0, 128, 0)),
        Color::Yellow => Some((128, 128, 0)),
        Color::Blue => Some((0, 0, 128)),
        Color::Magenta => Some((128, 0, 128)),
        Color::Cyan => Some((0, 128, 128)),
        Color::Gray => Some((192, 192, 192)),
        Color::DarkGray => Some((128, 128, 128)),
        Color::LightRed => Some((255, 0, 0)),
        Color::LightGreen => Some((0, 255, 0)),
        Color::LightYellow => Some((255, 255, 0)),
        Color::LightBlue => Some((0, 0, 255)),
        Color::LightMagenta => Some((255, 0, 255)),
        Color::LightCyan => Some((0, 255, 255)),
        Color::White => Some((255, 255, 255)),
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PaneBordersConfig;
    use crate::layout::PaneId;
    use crate::selection::Selection;
    use crate::terminal::TerminalRuntime;
    use crate::terminal::TerminalState;
    use crate::workspace::Workspace;

    fn render_view_pane_borders(
        app: &AppState,
        ws: &Workspace,
        split_borders: &[crate::layout::SplitBorder],
        frame: &mut Frame,
    ) {
        render_pane_borders(app, ws, &app.view.pane_infos, split_borders, frame);
    }

    #[test]
    fn unavailable_pane_renders_restore_failure_without_a_runtime() {
        let mut app = AppState::test_new();
        app.workspaces = vec![Workspace::test_new("unavailable")];
        app.active = Some(0);
        app.ensure_test_terminals();
        let pane_id = app.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.workspaces[0].terminal_id(pane_id).unwrap().clone();
        app.terminals.get_mut(&terminal_id).unwrap().restore_error =
            Some("Saved directory is unavailable. Restart to retry.".into());
        let runtimes = TerminalRuntimeRegistry::new();
        let area = Rect::new(0, 0, 80, 24);
        let layout = crate::ui::compute_tab_surface_for(
            &app,
            &runtimes,
            Some(crate::ui::TabSurfaceTarget {
                workspace_index: 0,
                tab_index: 0,
            }),
            area,
            false,
            Default::default(),
        );
        let (buffer, cursor, _, _) =
            crate::server::render_stream::render_tab_surface_virtual(&app, &runtimes, layout, area);
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("Saved directory is unavailable."));
        assert!(cursor.is_none_or(|cursor| !cursor.visible));
    }

    #[test]
    fn pane_border_title_trims_and_truncates() {
        assert_eq!(
            pane_border_title(" claude ", 20, false).as_deref(),
            Some(" claude ")
        );
        assert_eq!(
            pane_border_title(" claude ", 20, true).as_deref(),
            Some(" claude ")
        );
        assert_eq!(pane_border_title("", 20, false), None);
        assert_eq!(
            pane_border_title("abcdef", 8, false).as_deref(),
            Some(" abc… ")
        );
        assert_eq!(
            pane_border_title("abcdef", 8, true).as_deref(),
            Some(" abc… ")
        );
        assert_eq!(pane_border_title("abcdef", 4, false), None);
    }

    #[test]
    fn pane_border_title_truncates_cjk_by_display_width() {
        let title = pane_border_title("1 模块组织（已定）", 12, false).unwrap();

        assert_eq!(title, " 1 模块… ");
        assert!(display_width(title.as_str()) <= 10);
    }

    #[test]
    fn pane_border_renderer_places_adjacent_cjk_by_display_width() {
        let mut app = AppState::test_new();
        app.view.terminal_area = Rect::new(0, 0, 12, 3);
        let ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        app.view.pane_infos = vec![PaneInfo {
            id: pane_id,
            rect: Rect::new(0, 0, 12, 3),
            inner_rect: Rect::default(),
            scrollbar_rect: None,
            borders: Borders::ALL,
            is_focused: false,
            stack: None,
        }];

        let terminal_id = ws.tabs[0].panes[&pane_id].attached_terminal_id.clone();
        let mut terminal_state = TerminalState::new(terminal_id.clone(), "/tmp".into());
        terminal_state.set_manual_label("1 模块组织（已定）".into());
        app.terminals.insert(terminal_id, terminal_state);

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(12, 3)).unwrap();
        terminal
            .draw(|frame| render_view_pane_borders(&app, &ws, &[], frame))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(4, 0)].symbol(), "模");
        assert_eq!(buffer[(5, 0)].symbol(), " ");
        assert_eq!(buffer[(6, 0)].symbol(), "块");
    }

    #[test]
    fn default_horizontal_split_uses_one_shared_divider_column() {
        let mut workspace = Workspace::test_new("test");
        let root = workspace.tabs[0].root_pane;
        let right = workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.tabs[0].layout.focus_pane(root);

        let infos = apply_pane_chrome(
            workspace.tabs[0].layout.panes(Rect::new(0, 0, 100, 20)),
            PaneBordersConfig::Auto,
            false,
            true,
        );
        let left = infos.iter().find(|info| info.id == root).unwrap();
        let right = infos.iter().find(|info| info.id == right).unwrap();

        assert_eq!(left.rect.x + left.rect.width, right.rect.x);
        assert!(!left.borders.contains(Borders::RIGHT));
        assert!(right.borders.contains(Borders::LEFT));
    }

    #[test]
    fn default_vertical_split_uses_one_shared_divider_row() {
        let mut workspace = Workspace::test_new("test");
        let root = workspace.tabs[0].root_pane;
        let bottom = workspace.test_split(ratatui::layout::Direction::Vertical);
        workspace.tabs[0].layout.focus_pane(root);

        let infos = apply_pane_chrome(
            workspace.tabs[0].layout.panes(Rect::new(0, 0, 100, 20)),
            PaneBordersConfig::Auto,
            false,
            true,
        );
        let top = infos.iter().find(|info| info.id == root).unwrap();
        let bottom = infos.iter().find(|info| info.id == bottom).unwrap();

        assert_eq!(top.rect.y + top.rect.height, bottom.rect.y);
        assert!(!top.borders.contains(Borders::BOTTOM));
        assert!(bottom.borders.contains(Borders::TOP));
    }

    #[test]
    fn disabled_outer_borders_keep_only_shared_pane_dividers() {
        let mut workspace = Workspace::test_new("test");
        let root = workspace.tabs[0].root_pane;
        let right = workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.tabs[0].layout.focus_pane(root);

        let infos = apply_pane_chrome(
            workspace.tabs[0].layout.panes(Rect::new(0, 0, 100, 20)),
            PaneBordersConfig::Auto,
            false,
            false,
        );
        let left = infos.iter().find(|info| info.id == root).unwrap();
        let right = infos.iter().find(|info| info.id == right).unwrap();

        assert_eq!(left.borders, Borders::NONE);
        assert_eq!(right.borders, Borders::LEFT);
    }

    #[test]
    fn pane_gaps_keep_independent_bordered_panes() {
        let mut workspace = Workspace::test_new("test");
        let root = workspace.tabs[0].root_pane;
        let right = workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.tabs[0].layout.focus_pane(root);

        let infos = apply_pane_chrome(
            workspace.tabs[0].layout.panes(Rect::new(0, 0, 100, 20)),
            PaneBordersConfig::Auto,
            true,
            true,
        );
        let left = infos.iter().find(|info| info.id == root).unwrap();
        let right = infos.iter().find(|info| info.id == right).unwrap();

        assert_eq!(left.rect.x + left.rect.width, right.rect.x);
        assert_eq!(left.borders, Borders::ALL);
        assert_eq!(right.borders, Borders::ALL);
    }

    #[test]
    fn borderless_pane_gaps_add_one_empty_cell_between_panes() {
        let mut workspace = Workspace::test_new("test");
        let root = workspace.tabs[0].root_pane;
        let right = workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.tabs[0].layout.focus_pane(root);

        let infos = apply_pane_chrome(
            workspace.tabs[0].layout.panes(Rect::new(0, 0, 100, 20)),
            PaneBordersConfig::Off,
            true,
            true,
        );
        let left = infos.iter().find(|info| info.id == root).unwrap();
        let right = infos.iter().find(|info| info.id == right).unwrap();

        assert_eq!(left.rect, Rect::new(0, 0, 49, 20));
        assert_eq!(right.rect, Rect::new(50, 0, 50, 20));
        assert!(left.borders.is_empty());
        assert!(right.borders.is_empty());
    }

    #[test]
    fn disabled_pane_borders_make_inner_rect_equal_visual_rect() {
        let mut workspace = Workspace::test_new("test");
        workspace.test_split(ratatui::layout::Direction::Horizontal);

        let infos = apply_pane_chrome(
            workspace.tabs[0].layout.panes(Rect::new(0, 0, 100, 20)),
            PaneBordersConfig::Off,
            false,
            true,
        );

        for info in infos {
            assert!(info.borders.is_empty());
            assert_eq!(pane_inner_rect(info.rect, info.borders), info.rect);
        }
    }

    #[test]
    fn always_pane_borders_frame_lone_pane() {
        let workspace = Workspace::test_new("test");
        let area = Rect::new(0, 0, 100, 20);

        let default_infos = apply_pane_chrome(
            workspace.tabs[0].layout.panes(area),
            PaneBordersConfig::Auto,
            false,
            true,
        );
        assert_eq!(default_infos[0].borders, Borders::NONE);

        let framed_infos = apply_pane_chrome(
            workspace.tabs[0].layout.panes(area),
            PaneBordersConfig::Always,
            false,
            true,
        );
        assert_eq!(framed_infos[0].borders, Borders::ALL);

        let no_outer_infos = apply_pane_chrome(
            workspace.tabs[0].layout.panes(area),
            PaneBordersConfig::Always,
            false,
            false,
        );
        assert_eq!(no_outer_infos[0].borders, Borders::NONE);
    }

    #[test]
    fn global_pane_border_renderer_composes_junctions_and_focus_style() {
        let mut app = AppState::test_new();
        app.view.terminal_area = Rect::new(0, 0, 4, 4);
        app.view.pane_infos = vec![
            PaneInfo {
                id: PaneId::from_raw(1),
                rect: Rect::new(0, 0, 2, 2),
                inner_rect: Rect::default(),
                scrollbar_rect: None,
                borders: Borders::TOP | Borders::LEFT,
                is_focused: true,
                stack: None,
            },
            PaneInfo {
                id: PaneId::from_raw(2),
                rect: Rect::new(2, 0, 2, 2),
                inner_rect: Rect::default(),
                scrollbar_rect: None,
                borders: Borders::TOP | Borders::LEFT | Borders::RIGHT,
                is_focused: false,
                stack: None,
            },
            PaneInfo {
                id: PaneId::from_raw(3),
                rect: Rect::new(0, 2, 2, 2),
                inner_rect: Rect::default(),
                scrollbar_rect: None,
                borders: Borders::TOP | Borders::LEFT | Borders::BOTTOM,
                is_focused: false,
                stack: None,
            },
            PaneInfo {
                id: PaneId::from_raw(4),
                rect: Rect::new(2, 2, 2, 2),
                inner_rect: Rect::default(),
                scrollbar_rect: None,
                borders: Borders::ALL,
                is_focused: false,
                stack: None,
            },
        ];
        let split_borders = vec![
            crate::layout::SplitBorder {
                pos: 2,
                direction: ratatui::layout::Direction::Horizontal,
                ratio: 0.5,
                area: Rect::new(0, 0, 4, 4),
                path: vec![],
            },
            crate::layout::SplitBorder {
                pos: 2,
                direction: ratatui::layout::Direction::Vertical,
                ratio: 0.5,
                area: Rect::new(0, 0, 4, 4),
                path: vec![false],
            },
        ];
        let ws = Workspace::test_new("test");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(4, 4)).unwrap();

        terminal
            .draw(|frame| render_view_pane_borders(&app, &ws, &split_borders, frame))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 2)].symbol(), "┼");
        assert_eq!(buffer[(2, 2)].style().fg, Some(app.palette.accent));
        assert_eq!(buffer[(2, 1)].symbol(), "│");
        assert_eq!(buffer[(2, 1)].style().fg, Some(app.palette.accent));
    }

    #[test]
    fn gapped_pane_focus_does_not_color_neighbor_border() {
        let mut app = AppState::test_new();
        app.pane_gaps = true;
        app.view.terminal_area = Rect::new(0, 0, 4, 3);
        app.view.pane_infos = vec![
            PaneInfo {
                id: PaneId::from_raw(1),
                rect: Rect::new(0, 0, 2, 3),
                inner_rect: Rect::default(),
                scrollbar_rect: None,
                borders: Borders::ALL,
                is_focused: true,
                stack: None,
            },
            PaneInfo {
                id: PaneId::from_raw(2),
                rect: Rect::new(2, 0, 2, 3),
                inner_rect: Rect::default(),
                scrollbar_rect: None,
                borders: Borders::ALL,
                is_focused: false,
                stack: None,
            },
        ];
        let ws = Workspace::test_new("test");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(4, 3)).unwrap();

        terminal
            .draw(|frame| render_view_pane_borders(&app, &ws, &[], frame))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 1)].style().fg, Some(app.palette.accent));
        assert_eq!(buffer[(2, 1)].style().fg, Some(app.palette.overlay0));
    }

    #[tokio::test]
    async fn pane_scrollbar_gutter_is_reserved_before_scrollback_exists() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        let root_pane = workspace.tabs[0].root_pane;
        workspace.tabs[0].runtimes.insert(
            root_pane,
            TerminalRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
        );
        app.workspaces = vec![workspace];
        app.active = Some(0);

        let area = Rect::new(10, 3, 40, 8);
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        let infos = compute_pane_infos(
            &app,
            &terminal_runtimes,
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
        );
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, Rect::new(10, 3, 39, 8));
    }

    #[tokio::test]
    async fn alternate_screen_reclaims_scrollbar_gutter_and_restores_it_on_exit() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        let root_pane = workspace.tabs[0].root_pane;
        workspace.tabs[0].runtimes.insert(
            root_pane,
            TerminalRuntime::test_with_scrollback_bytes(
                40,
                8,
                1024,
                b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
            ),
        );
        app.workspaces = vec![workspace];
        app.active = Some(0);

        let area = Rect::new(10, 3, 40, 8);
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        let assert_geometry = |expected_width, has_scrollbar| {
            let infos = compute_pane_infos(
                &app,
                &terminal_runtimes,
                area,
                true,
                crate::kitty_graphics::HostCellSize::default(),
            );
            assert_eq!(
                infos[0].inner_rect,
                Rect::new(area.x, area.y, expected_width, area.height)
            );
            assert_eq!(infos[0].scrollbar_rect.is_some(), has_scrollbar);
            assert_eq!(
                app.workspaces[0].tabs[0].runtimes[&root_pane].current_size(),
                (area.height, expected_width)
            );
        };

        assert_geometry(39, true);
        app.workspaces[0].tabs[0].runtimes[&root_pane].test_process_pty_bytes(b"\x1b[?1049h");
        assert_geometry(40, false);
        app.workspaces[0].tabs[0].runtimes[&root_pane].test_process_pty_bytes(b"\x1b[?1049l");
        assert_geometry(39, true);
    }

    #[tokio::test]
    async fn zoomed_pane_scrollbar_gutter_is_reserved_before_scrollback_exists() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        workspace.zoomed = true;
        let root_pane = workspace.tabs[0].root_pane;
        workspace.tabs[0].runtimes.insert(
            root_pane,
            TerminalRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
        );
        app.workspaces = vec![workspace];
        app.active = Some(0);

        let area = Rect::new(10, 3, 40, 8);
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        let infos = compute_pane_infos(
            &app,
            &terminal_runtimes,
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
        );
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, Rect::new(10, 3, 39, 8));
    }

    #[tokio::test]
    async fn zoomed_multi_pane_keeps_border_space() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        let focused_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.zoomed = true;
        workspace.tabs[0].runtimes.insert(
            focused_pane,
            TerminalRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
        );
        app.workspaces = vec![workspace];
        app.active = Some(0);

        let area = Rect::new(10, 3, 40, 8);
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        let infos = compute_pane_infos(
            &app,
            &terminal_runtimes,
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
        );
        let info = &infos[0];

        assert_eq!(info.id, focused_pane);
        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, Rect::new(11, 4, 37, 6));
    }

    #[tokio::test]
    async fn cycling_a_stack_never_resizes_a_member_pty() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        let first = workspace.tabs[0].root_pane;
        let second = workspace.test_stack();
        let third = workspace.test_stack();
        for pane in [first, second, third] {
            workspace.tabs[0].runtimes.insert(
                pane,
                TerminalRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
            );
        }
        app.workspaces = vec![workspace];
        app.active = Some(0);

        let area = Rect::new(0, 0, 40, 12);
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        let sizes_after_focusing = |app: &mut AppState, focus: crate::layout::PaneId| {
            app.workspaces[0].tabs[0].layout.focus_pane(focus);
            compute_pane_infos(
                app,
                &terminal_runtimes,
                area,
                true,
                crate::kitty_graphics::HostCellSize::default(),
            );
            [first, second, third]
                .map(|pane| app.workspaces[0].tabs[0].runtimes[&pane].current_size())
        };

        let baseline = sizes_after_focusing(&mut app, first);
        assert_eq!(
            baseline, [baseline[0]; 3],
            "every member is sized to the same region"
        );
        assert_eq!(sizes_after_focusing(&mut app, second), baseline);
        assert_eq!(sizes_after_focusing(&mut app, third), baseline);
        assert_eq!(sizes_after_focusing(&mut app, first), baseline);
    }

    #[tokio::test]
    async fn collapsed_stack_members_get_one_row_and_the_active_member_gets_the_rest() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        let first = workspace.tabs[0].root_pane;
        let second = workspace.test_stack();
        for pane in [first, second] {
            workspace.tabs[0].runtimes.insert(
                pane,
                TerminalRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
            );
        }
        app.workspaces = vec![workspace];
        app.active = Some(0);

        let infos = compute_pane_infos(
            &app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 40, 12),
            false,
            crate::kitty_graphics::HostCellSize::default(),
        );

        let collapsed = infos
            .iter()
            .find(|info| info.id == first)
            .expect("first member present");
        let active = infos
            .iter()
            .find(|info| info.id == second)
            .expect("second member present");
        assert_eq!(collapsed.rect.height, 1);
        assert!(collapsed.stack.expect("stacked").collapsed);
        // No content rows: a click or a retained patch must never reach the hidden
        // terminal through its title row.
        assert_eq!(collapsed.inner_rect.height, 0);
        assert_eq!(collapsed.inner_rect.y, collapsed.rect.y);
        assert_eq!(collapsed.scrollbar_rect, None);
        assert_eq!(
            collapsed.borders,
            Borders::NONE,
            "a title row carries no chrome of its own"
        );

        assert_eq!(
            collapsed.stack.expect("stacked").header_rect.y,
            collapsed.stack.expect("stacked").region_rect.y,
            "the first member's title row is the region's own top border"
        );

        // The active member owns the box drawn around the whole stack, so its rect is
        // the full region while its terminal occupies only the content rows: 12 rows,
        // less the region's bottom border, less both members' title rows. The top
        // border is not subtracted — it is the first member's title row.
        let slot = active.stack.expect("stacked");
        assert!(!slot.collapsed);
        assert_eq!(active.rect, slot.region_rect);
        assert_eq!(slot.content_rect.height, 9);
        assert_eq!(active.inner_rect.height, 9);
        assert_eq!(
            slot.content_rect.y,
            slot.header_rect.y + 1,
            "the terminal starts on the row directly below its own title row"
        );
        assert!(
            !slot
                .header_rect
                .intersects(collapsed.stack.expect("stacked").header_rect),
            "each member has its own header row"
        );
        assert!(
            !slot.content_rect.intersects(collapsed.rect),
            "the terminal must not be drawn under a title row"
        );
    }

    #[tokio::test]
    async fn a_stack_without_a_top_border_keeps_every_title_row_inside_the_region() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        let first = workspace.tabs[0].root_pane;
        let second = workspace.test_stack();
        for pane in [first, second] {
            workspace.tabs[0].runtimes.insert(
                pane,
                TerminalRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
            );
        }
        app.workspaces = vec![workspace];
        app.active = Some(0);
        // Without outer borders the region has no top border row to spend on a title,
        // so the stack simply keeps its rows inside and reclaims nothing.
        app.pane_outer_borders = false;

        let region = Rect::new(0, 0, 40, 12);
        let infos = compute_pane_infos(
            &app,
            &TerminalRuntimeRegistry::new(),
            region,
            false,
            crate::kitty_graphics::HostCellSize::default(),
        );

        let collapsed = infos
            .iter()
            .find(|info| info.id == first)
            .expect("first member present")
            .stack
            .expect("stacked");
        let active = infos
            .iter()
            .find(|info| info.id == second)
            .expect("second member present")
            .stack
            .expect("stacked");

        assert!(!active.region_borders.contains(Borders::TOP));
        assert_eq!(
            collapsed.header_rect.y, region.y,
            "with no top border the first title row is the region's first row"
        );
        assert_eq!(
            active.content_rect.height, 10,
            "12 rows, less both title rows, and no border row to reclaim"
        );
    }

    #[test]
    fn a_collapsed_title_row_is_a_muted_version_of_the_accent() {
        let mut palette = Palette::catppuccin();
        palette.accent = Color::Rgb(250, 179, 135);

        let visible = stack_bar_style(&palette, false);
        let collapsed = stack_bar_style(&palette, true);

        assert_eq!(visible.fg, Some(palette.accent));
        let Some(Color::Rgb(r, g, b)) = collapsed.fg else {
            panic!("an rgb accent must mute to an rgb colour: {collapsed:?}");
        };
        assert_ne!(
            collapsed.fg,
            Some(palette.accent),
            "a collapsed row must be distinguishable from the visible one"
        );
        assert!(
            r < 250 && g < 179 && b > 37,
            "the mute moves the accent toward the panel background, not to grey: \
             {r},{g},{b}"
        );
        assert!(
            !collapsed
                .add_modifier
                .contains(ratatui::style::Modifier::DIM),
            "an rgb accent is mixed, never dimmed"
        );
    }

    #[test]
    fn an_unmixable_accent_falls_back_to_dimming() {
        let mut palette = Palette::catppuccin();
        palette.accent = Color::Indexed(208);

        let collapsed = stack_bar_style(&palette, true);

        assert_eq!(collapsed.fg, Some(Color::Indexed(208)));
        assert!(collapsed
            .add_modifier
            .contains(ratatui::style::Modifier::DIM));
    }

    #[test]
    fn a_stack_keeps_its_gap_when_borders_are_off() {
        let (mut layout, root) = crate::layout::TileLayout::new();
        let right = layout
            .split_pane(root, Direction::Horizontal, 0.5)
            .expect("root splits");
        let stacked = layout.stack_pane(root).expect("root stacks");
        layout.focus_pane(stacked);

        let infos = apply_pane_chrome(
            layout.panes(Rect::new(0, 0, 100, 40)),
            PaneBordersConfig::Off,
            true,
            true,
        );

        let visible = infos
            .iter()
            .find(|info| info.id == stacked)
            .expect("visible member");
        let neighbour = infos
            .iter()
            .find(|info| info.id == right)
            .expect("right pane");
        assert_eq!(
            visible.rect.right() + 1,
            neighbour.rect.x,
            "a one-cell gap separates the stack from its neighbour, as it does plain panes"
        );
    }

    #[tokio::test]
    async fn tiny_pane_does_not_reserve_scrollbar_gutter() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        let root_pane = workspace.tabs[0].root_pane;
        workspace.tabs[0].runtimes.insert(
            root_pane,
            TerminalRuntime::test_with_scrollback_bytes(4, 8, 1024, b"ready\n"),
        );
        app.workspaces = vec![workspace];
        app.active = Some(0);

        let area = Rect::new(10, 3, 4, 8);
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        let infos = compute_pane_infos(
            &app,
            &terminal_runtimes,
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
        );
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, area);
    }

    #[tokio::test]
    async fn pane_scrollbar_setting_controls_reserved_column() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        let root_pane = workspace.tabs[0].root_pane;
        workspace.tabs[0].runtimes.insert(
            root_pane,
            TerminalRuntime::test_with_scrollback_bytes(
                40,
                8,
                1024,
                b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
            ),
        );
        app.workspaces = vec![workspace];
        app.active = Some(0);

        let area = Rect::new(10, 3, 40, 8);
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        let infos = compute_pane_infos(
            &app,
            &terminal_runtimes,
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
        );
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, Some(Rect::new(49, 3, 1, 8)));
        assert_eq!(info.inner_rect, Rect::new(10, 3, 39, 8));

        app.pane_scrollbars = false;
        let infos = compute_pane_infos(
            &app,
            &terminal_runtimes,
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
        );
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, area);
    }

    #[test]
    fn selection_highlight_uses_one_uniform_style() {
        let palette = Palette::catppuccin();
        let host_theme = crate::terminal_theme::TerminalTheme {
            foreground: None,
            background: Some(crate::terminal_theme::RgbColor {
                r: 12,
                g: 14,
                b: 16,
            }),
            ..Default::default()
        };
        let expected_style = automatic_selection_style(&palette, host_theme);
        let selection = Some(Selection::absolute_range(
            PaneId::from_raw(1),
            (0, 0),
            (0, 2),
        ));
        let backend = ratatui::backend::TestBackend::new(4, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                buf[(0, 0)].set_style(
                    Style::default()
                        .fg(Color::Rgb(10, 220, 120))
                        .bg(Color::Black),
                );
                buf[(1, 0)].set_style(
                    Style::default()
                        .fg(Color::Rgb(220, 180, 40))
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
                buf[(2, 0)].set_style(Style::default().fg(Color::Blue).bg(Color::Reset));
                render_selection_highlight(
                    selection.as_ref(),
                    frame.buffer_mut(),
                    &PaneId::from_raw(1),
                    Rect::new(0, 0, 4, 1),
                    None,
                    &palette,
                    host_theme,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let first = buffer[(0, 0)].style();
        let second = buffer[(1, 0)].style();
        let third = buffer[(2, 0)].style();

        assert_eq!(first.fg, expected_style.fg);
        assert_eq!(second.fg, expected_style.fg);
        assert_eq!(third.fg, expected_style.fg);
        assert_eq!(first.bg, expected_style.bg);
        assert_eq!(second.bg, expected_style.bg);
        assert_eq!(third.bg, expected_style.bg);
        assert_eq!(first.add_modifier, expected_style.add_modifier);
        assert_eq!(second.add_modifier, expected_style.add_modifier);
        assert_eq!(third.add_modifier, expected_style.add_modifier);
        assert!(!second.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn automatic_selection_background_uses_host_background() {
        let bg = automatic_selection_bg(
            &Palette::terminal(),
            crate::terminal_theme::TerminalTheme {
                foreground: Some(crate::terminal_theme::RgbColor {
                    r: 230,
                    g: 230,
                    b: 230,
                }),
                background: Some(crate::terminal_theme::RgbColor {
                    r: 12,
                    g: 14,
                    b: 16,
                }),
                ..Default::default()
            },
        );

        let Color::Rgb(r, g, b) = bg else {
            panic!("selection background should resolve to rgb");
        };
        assert!(relative_luminance((r, g, b)) > relative_luminance((12, 14, 16)));
    }

    #[test]
    fn automatic_selection_rgb_style_is_readable_with_or_without_host_background() {
        for (background, selected_bg, selected_fg) in [
            ((239, 241, 245), (172, 174, 176), (0, 0, 0)),
            ((26, 27, 38), (90, 91, 99), (255, 255, 255)),
            ((45, 53, 59), (104, 110, 114), (255, 255, 255)),
        ] {
            let mut palette = Palette::catppuccin();
            let (r, g, b) = background;
            palette.panel_bg = Color::Rgb(r, g, b);
            let expected = Style::reset()
                .bg(Color::Rgb(selected_bg.0, selected_bg.1, selected_bg.2))
                .fg(Color::Rgb(selected_fg.0, selected_fg.1, selected_fg.2));

            assert_eq!(
                automatic_selection_style(&palette, Default::default()),
                expected
            );
            assert_eq!(
                automatic_selection_style(
                    &Palette::terminal(),
                    crate::terminal_theme::TerminalTheme {
                        background: Some(crate::terminal_theme::RgbColor { r, g, b }),
                        ..Default::default()
                    },
                ),
                expected
            );
        }
    }

    #[test]
    fn automatic_selection_preserves_symbolic_palette_fallbacks() {
        let mut palette = Palette::terminal();
        assert_eq!(
            automatic_selection_style(&palette, Default::default()),
            Style::reset().fg(Color::White).bg(Color::DarkGray)
        );
        for fallback in [Color::Blue, Color::White, Color::Indexed(42), Color::Reset] {
            palette.surface_dim = fallback;
            assert_eq!(
                automatic_selection_bg(&palette, Default::default()),
                fallback
            );
        }
    }
}
