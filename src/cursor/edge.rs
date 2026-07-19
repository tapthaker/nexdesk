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

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 1920;
    const H: u32 = 1080;

    #[test]
    fn left_edge_at_zero() {
        assert_eq!(detect_edge(0, 500, W, H), Some(Direction::Left));
    }

    #[test]
    fn left_edge_at_margin() {
        assert_eq!(detect_edge(EDGE_MARGIN, 500, W, H), Some(Direction::Left));
    }

    #[test]
    fn just_inside_left_margin() {
        assert_eq!(detect_edge(EDGE_MARGIN + 1, 500, W, H), None);
    }

    #[test]
    fn right_edge_at_max() {
        assert_eq!(detect_edge(W as i32 - 1, 500, W, H), Some(Direction::Right));
    }

    #[test]
    fn right_edge_at_margin() {
        assert_eq!(
            detect_edge(W as i32 - EDGE_MARGIN - 1, 500, W, H),
            Some(Direction::Right)
        );
    }

    #[test]
    fn just_inside_right_margin() {
        assert_eq!(detect_edge(W as i32 - EDGE_MARGIN - 2, 500, W, H), None);
    }

    #[test]
    fn top_edge_at_zero() {
        assert_eq!(detect_edge(500, 0, W, H), Some(Direction::Up));
    }

    #[test]
    fn top_edge_at_margin() {
        assert_eq!(detect_edge(500, EDGE_MARGIN, W, H), Some(Direction::Up));
    }

    #[test]
    fn bottom_edge_at_max() {
        assert_eq!(detect_edge(500, H as i32 - 1, W, H), Some(Direction::Down));
    }

    #[test]
    fn bottom_edge_at_margin() {
        assert_eq!(
            detect_edge(500, H as i32 - EDGE_MARGIN - 1, W, H),
            Some(Direction::Down)
        );
    }

    #[test]
    fn center_returns_none() {
        assert_eq!(detect_edge(960, 540, W, H), None);
    }

    #[test]
    fn corner_priority_left_over_up() {
        // At (0, 0), left is checked before up
        assert_eq!(detect_edge(0, 0, W, H), Some(Direction::Left));
    }

    #[test]
    fn corner_priority_right_over_down() {
        assert_eq!(
            detect_edge(W as i32 - 1, H as i32 - 1, W, H),
            Some(Direction::Right)
        );
    }

    #[test]
    fn origin_returns_left() {
        assert_eq!(detect_edge(0, 0, W, H), Some(Direction::Left));
    }

    #[test]
    fn max_corner_returns_right() {
        assert_eq!(
            detect_edge(W as i32 - 1, H as i32 - 1, W, H),
            Some(Direction::Right)
        );
    }
}
