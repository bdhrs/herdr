use super::*;

/// A pane above a two-member stack: the root pane on top, the stack below it with
/// the newer member visible. Returns (server, render receiver, top, collapsed, visible).
fn stacked_test_server() -> (
    HeadlessServer,
    std::sync::mpsc::Receiver<Vec<u8>>,
    crate::layout::PaneId,
    crate::layout::PaneId,
    crate::layout::PaneId,
) {
    let (mut server, render_rx, top) = retained_test_server(b"TOP-PANE");
    let workspace = &mut server.app.state.workspaces[0];
    let collapsed = workspace.test_split(ratatui::layout::Direction::Vertical);
    let visible = workspace.test_stack();
    workspace.insert_test_runtime(
        collapsed,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"HIDDEN-PANE"),
    );
    workspace.insert_test_runtime(
        visible,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"VISIBLE-PANE"),
    );
    server.app.state.ensure_test_terminals();
    (server, render_rx, top, collapsed, visible)
}

fn last_surface(server: &HeadlessServer) -> crate::protocol::PaneSurfaceFrame {
    server.clients[&1]
        .render_state
        .last_pane_surface()
        .expect("rendered pane surface")
        .clone()
}

fn surface_pane<'a>(
    server: &HeadlessServer,
    surface: &'a crate::protocol::PaneSurfaceFrame,
    pane: crate::layout::PaneId,
) -> &'a crate::protocol::PaneSurfacePane {
    let public_id = server.app.public_pane_id(0, pane).expect("public pane id");
    surface
        .panes
        .iter()
        .find(|candidate| candidate.pane_id == public_id)
        .expect("pane is in the surface")
}

fn contains(rect: crate::protocol::SurfaceRect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

#[tokio::test]
async fn a_click_on_a_collapsed_title_row_hits_that_member_first() {
    let (mut server, _render_rx, _top, collapsed, visible) = stacked_test_server();
    server.render_and_stream();
    let surface = last_surface(&server);

    let hidden = surface_pane(&server, &surface, collapsed);
    let shown = surface_pane(&server, &surface, visible);
    assert_eq!(
        hidden.inner_rect.height, 0,
        "a collapsed member has no content rows for clicks or patches to land in"
    );
    assert!(
        contains(shown.rect, hidden.rect.x + 2, hidden.rect.y),
        "the visible member owns the whole region, title rows included"
    );
    // Clients take the first pane whose rect contains the click.
    let first_hit = surface
        .panes
        .iter()
        .find(|pane| contains(pane.rect, hidden.rect.x + 2, hidden.rect.y))
        .expect("the title row is clickable");
    assert_eq!(first_hit.pane_id, hidden.pane_id);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn the_split_handle_on_a_stacks_top_title_row_leaves_the_title_clickable() {
    let (mut server, _render_rx, _top, collapsed, _visible) = stacked_test_server();
    server.render_and_stream();
    let surface = last_surface(&server);
    let hidden = surface_pane(&server, &surface, collapsed);
    let title_row = hidden.rect.y;

    let handle = surface
        .splits
        .iter()
        .find(|split| split.direction == crate::protocol::PaneSurfaceSplitDirection::Vertical)
        .expect("the split above the stack has a handle");
    assert!(
        contains(handle.hit_rect, handle.hit_rect.x, title_row),
        "this test is only meaningful while the handle shares the title row"
    );
    assert!(
        !contains(handle.hit_rect, hidden.rect.x + 2, title_row),
        "the start of the title row must focus the member, not start a resize"
    );
    assert!(
        contains(
            handle.hit_rect,
            hidden.rect.x + hidden.rect.width - 3,
            title_row
        ),
        "the rest of the line still resizes the split"
    );
    shutdown_test_runtimes(&mut server);
}

/// Every cell the server sent to the client since the last drain, as text.
fn drain_sent_text(render_rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> String {
    let mut text = String::new();
    while let Ok(bytes) = render_rx.recv_timeout(Duration::from_millis(50)) {
        match read_server_message(bytes) {
            ServerMessage::PaneSurfacePatch(patch) => {
                for row in patch.rows {
                    text.extend(row.cells.iter().map(|cell| cell.symbol.as_str()));
                    text.push('\n');
                }
            }
            ServerMessage::PaneSurface(surface) => text.push_str(&frame_text(&surface.frame)),
            _ => {}
        }
    }
    text
}

#[tokio::test]
async fn retained_update_never_paints_a_collapsed_members_output() {
    let (mut server, render_rx, _top, collapsed, visible) = stacked_test_server();
    // Settle every terminal out of its initial full-dirty state.
    for _ in 0..3 {
        server.render_and_stream();
    }
    drain_sent_text(&render_rx);

    // A hidden pane only refreshes its render state when a patch is collected, so
    // the first attempt can report the whole screen dirty. The second is the one
    // that would paint row 0 of the hidden terminal onto its title row.
    write_shared_test_pane(&mut server, collapsed, b"\x1b[Hsettling");
    server.render_retained_pane_surface_and_stream(&HashSet::from([collapsed]));
    drain_sent_text(&render_rx);
    write_shared_test_pane(&mut server, collapsed, b"\x1b[HLEAKED-FROM-BACKGROUND");
    assert!(
        server.render_retained_pane_surface_and_stream(&HashSet::from([collapsed])),
        "the retained path must actually run, or this test proves nothing"
    );
    let sent = drain_sent_text(&render_rx);
    assert!(
        !sent.contains("LEAKED-FROM-BACKGROUND"),
        "a collapsed stack member's output must never reach the screen: {sent}"
    );

    // Control: the same path does deliver the visible member's output.
    write_shared_test_pane(&mut server, visible, b"\x1b[HSHOWN-ON-SCREEN");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([visible])));
    let sent = drain_sent_text(&render_rx);
    assert!(sent.contains("SHOWN-ON-SCREEN"), "{sent}");
    shutdown_test_runtimes(&mut server);
}
