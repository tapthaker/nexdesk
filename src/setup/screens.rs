use ratatui::prelude::*;

use super::app::SetupState;
use super::edge_picker;

pub fn render(frame: &mut Frame, area: Rect, state: &SetupState) {
    edge_picker::render_edge_layout(frame, area, state.edge_selection);
}
