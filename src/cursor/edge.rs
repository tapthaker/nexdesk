use crate::net::protocol::Direction;

/// Margin in pixels from screen edge to trigger switching.
const EDGE_MARGIN: i32 = 2;

/// Detect if the cursor is at a screen edge.
pub fn detect_edge(x: i32, y: i32, screen_width: u32, screen_height: u32) -> Option<Direction> {
    let w = screen_width as i32;
    let h = screen_height as i32;

    if x <= EDGE_MARGIN {
        Some(Direction::Left)
    } else if x >= w - EDGE_MARGIN - 1 {
        Some(Direction::Right)
    } else if y <= EDGE_MARGIN {
        Some(Direction::Up)
    } else if y >= h - EDGE_MARGIN - 1 {
        Some(Direction::Down)
    } else {
        None
    }
}
