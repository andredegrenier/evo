//! The horizontal ribbon: grouped controls either side of an "evo" button that
//! goes back to the document library.
//!
//! Replaces a flat toolbar that ran everything together in one row, had no
//! zoom controls (the percentage was read-only in the status bar), and offered
//! no way back to the library short of closing the document from a menu.
//!
//! Groups and the items inside them can be rearranged in customize mode. It is
//! a mode rather than always-on dragging because the ribbon is full of live
//! widgets -- colour pickers, drag values -- which consume drags themselves;
//! trying to drag one would fight the control instead of moving it.

use eframe::egui::{self, Sense, StrokeKind};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};

use crate::keymap::{Action, Keymap};
use crate::state::DocState;
use crate::tools::ActiveTool;
use crate::ui::canvas;
use crate::ui::theme::Tokens;

const ICON_SIZE: f32 = 16.0;
const BUTTON_SIZE: egui::Vec2 = egui::Vec2::new(28.0, 28.0);
const EVO_WIDTH: f32 = 74.0;
/// Breathing room either side of the evo button before a group may start.
const EVO_GUTTER: f32 = 12.0;

/// One control on the ribbon.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum RibbonItem {
    Undo,
    Redo,
    Tool(ActiveTool),
    StrokeColor,
    FillColor,
    StrokeWidth,
    FontSize,
    ZoomOut,
    ZoomLevel,
    ZoomIn,
    FitWidth,
}

impl RibbonItem {
    /// Every item that exists, in default order. `sanitize` uses this to
    /// restore anything a stored layout is missing.
    pub const ALL: [RibbonItem; 22] = [
        Self::Undo,
        Self::Redo,
        Self::Tool(ActiveTool::Select),
        Self::Tool(ActiveTool::Pan),
        Self::Tool(ActiveTool::Highlight),
        Self::Tool(ActiveTool::Text),
        Self::Tool(ActiveTool::Rect),
        Self::Tool(ActiveTool::Ellipse),
        Self::Tool(ActiveTool::Line),
        Self::Tool(ActiveTool::Arrow),
        Self::Tool(ActiveTool::Pen),
        Self::Tool(ActiveTool::Polygon),
        Self::Tool(ActiveTool::PolyLine),
        Self::Tool(ActiveTool::Cloud),
        Self::StrokeColor,
        Self::FillColor,
        Self::StrokeWidth,
        Self::FontSize,
        Self::ZoomOut,
        Self::ZoomLevel,
        Self::ZoomIn,
        Self::FitWidth,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Undo => "Undo",
            Self::Redo => "Redo",
            Self::Tool(tool) => tool.label(),
            Self::StrokeColor => "Stroke",
            Self::FillColor => "Fill",
            Self::StrokeWidth => "Width",
            Self::FontSize => "Font",
            Self::ZoomOut => "Zoom Out",
            Self::ZoomLevel => "Zoom Level",
            Self::ZoomIn => "Zoom In",
            Self::FitWidth => "Fit Width",
        }
    }

    fn home(self) -> RibbonGroup {
        match self {
            Self::Undo | Self::Redo => RibbonGroup::History,
            Self::Tool(_) => RibbonGroup::Tools,
            Self::StrokeColor | Self::FillColor | Self::StrokeWidth | Self::FontSize => {
                RibbonGroup::Style
            }
            Self::ZoomOut | Self::ZoomLevel | Self::ZoomIn | Self::FitWidth => RibbonGroup::Zoom,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum RibbonGroup {
    History,
    Tools,
    Style,
    Zoom,
}

impl RibbonGroup {
    pub const ALL: [RibbonGroup; 4] = [Self::History, Self::Tools, Self::Style, Self::Zoom];

    pub fn label(self) -> &'static str {
        match self {
            Self::History => "History",
            Self::Tools => "Tools",
            Self::Style => "Style",
            Self::Zoom => "Zoom",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GroupConfig {
    pub group: RibbonGroup,
    pub items: Vec<RibbonItem>,
    #[serde(default = "yes")]
    pub visible: bool,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RibbonConfig {
    pub groups: Vec<GroupConfig>,
    #[serde(skip)]
    pub customizing: bool,
}

impl Default for RibbonConfig {
    fn default() -> Self {
        let group = |group: RibbonGroup| GroupConfig {
            group,
            items: RibbonItem::ALL
                .iter()
                .copied()
                .filter(|i| i.home() == group)
                .collect(),
            visible: true,
        };
        Self {
            groups: RibbonGroup::ALL.iter().copied().map(group).collect(),
            customizing: false,
        }
    }
}

impl RibbonConfig {
    /// Reconcile a stored layout with the items this build actually has.
    ///
    /// Without this, upgrading either loses a new control (it is in no stored
    /// group, so it never renders) or panics on one that was removed. Both are
    /// silent from the user's side -- the button is simply gone -- so repair
    /// rather than reject.
    pub fn sanitize(&mut self) {
        self.groups.retain(|g| RibbonGroup::ALL.contains(&g.group));
        for group in RibbonGroup::ALL {
            if !self.groups.iter().any(|g| g.group == group) {
                self.groups.push(GroupConfig {
                    group,
                    items: Vec::new(),
                    visible: true,
                });
            }
        }

        let mut seen = std::collections::HashSet::new();
        for g in &mut self.groups {
            g.items
                .retain(|i| RibbonItem::ALL.contains(i) && seen.insert(*i));
        }
        for item in RibbonItem::ALL {
            if !seen.contains(&item) {
                let home = item.home();
                if let Some(g) = self.groups.iter_mut().find(|g| g.group == home) {
                    g.items.push(item);
                }
            }
        }
    }

    fn visible_groups(&self) -> Vec<usize> {
        (0..self.groups.len())
            .filter(|&i| self.groups[i].visible && !self.groups[i].items.is_empty())
            .collect()
    }

    pub fn move_group(&mut self, from: usize, to: usize) {
        if from == to || from >= self.groups.len() || to >= self.groups.len() {
            return;
        }
        let g = self.groups.remove(from);
        self.groups.insert(to, g);
    }

    /// Move an item to the end of another group (or to a new slot in its own).
    pub fn move_item(&mut self, from: ItemAddr, to_group: usize) {
        if from.group >= self.groups.len() || to_group >= self.groups.len() {
            return;
        }
        if from.index >= self.groups[from.group].items.len() {
            return;
        }
        let item = self.groups[from.group].items.remove(from.index);
        self.groups[to_group].items.push(item);
    }
}

/// Where an item lives. A distinct payload type from `DragGroup` so egui's
/// drag-and-drop, which matches payloads by type, can tell a group drag from
/// an item drag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ItemAddr {
    pub group: usize,
    pub index: usize,
}

#[derive(Clone, Copy)]
struct DragGroup(usize);

pub enum RibbonAction {
    /// The evo button: back to the document library.
    GoToLibrary,
}

pub fn show(
    ui: &mut egui::Ui,
    dc: &mut DocState,
    cfg: &mut RibbonConfig,
    keymap: &Keymap,
    t: &Tokens,
) -> Option<RibbonAction> {
    let mut action = None;
    let full = ui.max_rect();
    let centre = full.center().x;
    let evo_rect = egui::Rect::from_center_size(
        egui::Pos2::new(centre, full.center().y),
        egui::Vec2::new(EVO_WIDTH, (t.ribbon_height - 12.0).max(24.0)),
    );

    // Split the visible groups either side of the centre. The two sides are
    // laid out into fixed rects rather than one flowing row, which is what
    // keeps the evo button exactly centred however wide the groups get.
    let visible = cfg.visible_groups();
    let split = visible.len().div_ceil(2);
    let (left, right) = visible.split_at(split);

    let left_rect = egui::Rect::from_min_max(
        full.min,
        egui::Pos2::new(evo_rect.left() - EVO_GUTTER, full.max.y),
    );
    let right_rect = egui::Rect::from_min_max(
        egui::Pos2::new(evo_rect.right() + EVO_GUTTER, full.min.y),
        full.max,
    );

    let mut edit: Option<Edit> = None;

    if left_rect.width() > 0.0 {
        ui.scope_builder(egui::UiBuilder::new().max_rect(left_rect), |ui| {
            ui.horizontal_centered(|ui| {
                for &index in left {
                    draw_group(ui, dc, cfg, keymap, t, index, &mut edit);
                }
            });
        });
    }
    if right_rect.width() > 0.0 {
        ui.scope_builder(egui::UiBuilder::new().max_rect(right_rect), |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for &index in right.iter().rev() {
                    draw_group(ui, dc, cfg, keymap, t, index, &mut edit);
                }
            });
        });
    }

    if evo_button(ui, evo_rect, t).clicked() {
        action = Some(RibbonAction::GoToLibrary);
    }

    // Right-clicking anywhere on the ribbon: visibility and customize mode.
    let bg = ui.interact(full, ui.id().with("ribbon-bg"), Sense::click());
    bg.context_menu(|ui| ribbon_menu(ui, cfg));

    if cfg.customizing {
        customize_banner(ui, full, cfg, t);
    }

    apply(cfg, edit);
    action
}

/// A rearrangement, applied after the ribbon is drawn so the borrow ends first.
enum Edit {
    Group { from: usize, to: usize },
    Item { from: ItemAddr, to: usize },
}

fn apply(cfg: &mut RibbonConfig, edit: Option<Edit>) {
    match edit {
        Some(Edit::Group { from, to }) => cfg.move_group(from, to),
        Some(Edit::Item { from, to }) => cfg.move_item(from, to),
        None => {}
    }
}

fn evo_button(ui: &mut egui::Ui, rect: egui::Rect, t: &Tokens) -> egui::Response {
    let resp = ui.interact(rect, ui.id().with("evo-home"), Sense::click());
    let hovered = resp.hovered();
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        t.radius(t.radius_l),
        if hovered { t.accent } else { t.bg_raised },
    );
    if !hovered {
        painter.rect_stroke(rect, t.radius(t.radius_l), t.hairline(), StrokeKind::Inside);
    }
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "evo",
        egui::FontId::proportional(17.0),
        if hovered { t.on_accent } else { t.ink },
    );
    resp.on_hover_text("Document Library")
}

fn ribbon_menu(ui: &mut egui::Ui, cfg: &mut RibbonConfig) {
    ui.label(egui::RichText::new("Ribbon").strong());
    for i in 0..cfg.groups.len() {
        let label = cfg.groups[i].group.label();
        let mut visible = cfg.groups[i].visible;
        if ui.checkbox(&mut visible, label).changed() {
            cfg.groups[i].visible = visible;
        }
    }
    ui.separator();
    let label = if cfg.customizing {
        "Done Customizing"
    } else {
        "Customize Ribbon…"
    };
    if ui.button(label).clicked() {
        cfg.customizing = !cfg.customizing;
        ui.close();
    }
    if ui.button("Reset Layout").clicked() {
        let customizing = cfg.customizing;
        *cfg = RibbonConfig::default();
        cfg.customizing = customizing;
        ui.close();
    }
}

fn customize_banner(ui: &mut egui::Ui, full: egui::Rect, cfg: &mut RibbonConfig, t: &Tokens) {
    let painter = ui.painter();
    painter.rect_stroke(
        full.shrink(1.0),
        t.radius(t.radius_s),
        egui::Stroke::new(1.0, t.accent),
        StrokeKind::Inside,
    );
    // Esc is the usual way out of a mode, and it costs nothing here: the
    // ribbon has no gesture of its own for Esc to cancel.
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        cfg.customizing = false;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_group(
    ui: &mut egui::Ui,
    dc: &mut DocState,
    cfg: &RibbonConfig,
    keymap: &Keymap,
    t: &Tokens,
    index: usize,
    edit: &mut Option<Edit>,
) {
    let id = ui.id().with("ribbon-group").with(index);
    let frame = t
        .card()
        .inner_margin(egui::Margin::symmetric(t.space_s as i8, 2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = t.space_xs;
            ui.horizontal_centered(|ui| {
                for slot in 0..cfg.groups[index].items.len() {
                    let addr = ItemAddr {
                        group: index,
                        index: slot,
                    };
                    draw_item(ui, dc, cfg, keymap, t, addr);
                }
            });
        });

    if !cfg.customizing {
        return;
    }

    // In customize mode the whole card becomes a drag source and a drop
    // target: for other groups (reorder) and for items (move between groups).
    let resp = ui.interact(frame.response.rect, id, Sense::click_and_drag());
    resp.dnd_set_drag_payload(DragGroup(index));

    if let Some(from) = resp.dnd_release_payload::<DragGroup>() {
        if from.0 != index {
            *edit = Some(Edit::Group {
                from: from.0,
                to: index,
            });
        }
    } else if let Some(from) = resp.dnd_release_payload::<ItemAddr>()
        && from.group != index
    {
        *edit = Some(Edit::Item {
            from: *from,
            to: index,
        });
    }

    let hovering = resp.dnd_hover_payload::<DragGroup>().is_some()
        || resp.dnd_hover_payload::<ItemAddr>().is_some();
    ui.painter().rect_stroke(
        frame.response.rect,
        t.radius(t.radius_m),
        egui::Stroke::new(if hovering { 2.0 } else { 1.0 }, t.accent),
        StrokeKind::Outside,
    );
}

fn icon_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).size(ICON_SIZE)).min_size(BUTTON_SIZE)
}

fn draw_item(
    ui: &mut egui::Ui,
    dc: &mut DocState,
    cfg: &RibbonConfig,
    keymap: &Keymap,
    t: &Tokens,
    addr: ItemAddr,
) {
    let item = cfg.groups[addr.group].items[addr.index];

    if cfg.customizing {
        // Inert stand-ins: a live colour picker or drag value would swallow
        // the drag that is meant to move it.
        let resp = ui
            .scope(|ui| {
                ui.disable();
                item_widget(ui, dc, keymap, t, item);
            })
            .response;
        let hit = ui.interact(
            resp.rect,
            ui.id().with("ribbon-item").with((addr.group, addr.index)),
            Sense::click_and_drag(),
        );
        hit.dnd_set_drag_payload(addr);
        if hit.hovered() {
            ui.painter().rect_stroke(
                resp.rect,
                t.radius(t.radius_s),
                egui::Stroke::new(1.0, t.accent),
                StrokeKind::Outside,
            );
        }
        hit.on_hover_text(item.label());
        return;
    }

    item_widget(ui, dc, keymap, t, item);
}

fn item_widget(
    ui: &mut egui::Ui,
    dc: &mut DocState,
    keymap: &Keymap,
    t: &Tokens,
    item: RibbonItem,
) {
    let ctx = ui.ctx().clone();
    match item {
        RibbonItem::Undo => {
            if ui
                .add_enabled(
                    dc.history.can_undo(),
                    icon_button(icon::ARROW_COUNTER_CLOCKWISE),
                )
                .on_hover_text(keymap.tooltip(&ctx, Action::Undo))
                .clicked()
            {
                if dc.editing_text.is_some() {
                    canvas::commit_text_edit(dc);
                }
                dc.history.undo(&mut dc.store, &mut dc.pages);
            }
        }
        RibbonItem::Redo => {
            if ui
                .add_enabled(dc.history.can_redo(), icon_button(icon::ARROW_CLOCKWISE))
                .on_hover_text(keymap.tooltip(&ctx, Action::Redo))
                .clicked()
            {
                dc.history.redo(&mut dc.store, &mut dc.pages);
            }
        }
        RibbonItem::Tool(tool) => {
            let (glyph, action) = tool_icon(tool);
            let selected = dc.tool == tool;
            let mut tip = keymap.tooltip(&ctx, action);
            if tool == ActiveTool::Pan {
                tip.push_str(" — or hold Space");
            }
            if ui
                .add(icon_button(glyph).selected(selected))
                .on_hover_text(tip)
                .clicked()
            {
                if dc.editing_text.is_some() {
                    canvas::commit_text_edit(dc);
                }
                dc.tool = tool;
            }
        }
        RibbonItem::StrokeColor => {
            let mut stroke = to_egui(dc.current_style.stroke);
            if ui
                .color_edit_button_srgba(&mut stroke)
                .on_hover_text("Stroke colour")
                .changed()
            {
                dc.current_style.stroke = from_egui(stroke);
            }
        }
        RibbonItem::FillColor => {
            let mut fill = to_egui(dc.current_style.fill);
            if ui
                .color_edit_button_srgba(&mut fill)
                .on_hover_text("Fill colour")
                .changed()
            {
                dc.current_style.fill = from_egui(fill);
            }
        }
        RibbonItem::StrokeWidth => {
            ui.add(
                egui::DragValue::new(&mut dc.current_style.stroke_width)
                    .range(0.5..=24.0)
                    .speed(0.1)
                    .prefix("W "),
            )
            .on_hover_text("Stroke width");
        }
        RibbonItem::FontSize => {
            ui.add(
                egui::DragValue::new(&mut dc.current_font_size)
                    .range(6.0..=96.0)
                    .speed(0.5)
                    .prefix("A "),
            )
            .on_hover_text("Font size");
        }
        RibbonItem::ZoomOut => {
            if ui
                .add(icon_button(icon::MINUS))
                .on_hover_text(keymap.tooltip(&ctx, Action::ZoomOut))
                .clicked()
            {
                let z = dc.viewport.zoom / crate::app::ZOOM_STEP;
                dc.viewport.set_zoom(z);
            }
        }
        RibbonItem::ZoomIn => {
            if ui
                .add(icon_button(icon::PLUS))
                .on_hover_text(keymap.tooltip(&ctx, Action::ZoomIn))
                .clicked()
            {
                let z = dc.viewport.zoom * crate::app::ZOOM_STEP;
                dc.viewport.set_zoom(z);
            }
        }
        RibbonItem::ZoomLevel => {
            // Click to go back to 100%: the percentage used to be a read-only
            // status-bar label with no way to act on it.
            let percent = (dc.viewport.zoom * 100.0).round() as i32;
            let resp = ui.add(
                egui::Button::new(format!("{percent}%"))
                    .min_size(egui::Vec2::new(52.0, BUTTON_SIZE.y))
                    .fill(t.bg_sunken),
            );
            if resp.on_hover_text("Actual size").clicked() {
                dc.viewport.set_zoom(1.0);
            }
        }
        RibbonItem::FitWidth => {
            if ui
                .add(icon_button(icon::ARROWS_OUT_LINE_HORIZONTAL).selected(dc.viewport.fit_width))
                .on_hover_text(keymap.tooltip(&ctx, Action::ZoomFitWidth))
                .clicked()
            {
                dc.viewport.fit_width = true;
            }
        }
    }
}

fn tool_icon(tool: ActiveTool) -> (&'static str, Action) {
    match tool {
        ActiveTool::Select => (icon::CURSOR, Action::ToolSelect),
        ActiveTool::Pan => (icon::HAND, Action::ToolPan),
        ActiveTool::Highlight => (icon::HIGHLIGHTER, Action::ToolHighlight),
        ActiveTool::Text => (icon::TEXT_T, Action::ToolText),
        ActiveTool::Rect => (icon::RECTANGLE, Action::ToolRect),
        ActiveTool::Ellipse => (icon::CIRCLE, Action::ToolEllipse),
        ActiveTool::Line => (icon::LINE_SEGMENT, Action::ToolLine),
        ActiveTool::Arrow => (icon::ARROW_UP_RIGHT, Action::ToolArrow),
        ActiveTool::Pen => (icon::SCRIBBLE, Action::ToolPen),
        ActiveTool::Cloud => (icon::CLOUD, Action::ToolCloud),
        ActiveTool::Polygon => (icon::POLYGON, Action::ToolPolygon),
        ActiveTool::PolyLine => (icon::LINE_SEGMENTS, Action::ToolPolyLine),
    }
}

pub fn to_egui(c: crate::doc::annotation::Color) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

pub fn from_egui(c: egui::Color32) -> crate::doc::annotation::Color {
    crate::doc::annotation::Color::rgba(c.r(), c.g(), c.b(), c.a())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_layout_holds_every_item_exactly_once() {
        let cfg = RibbonConfig::default();
        let mut seen: Vec<RibbonItem> = cfg.groups.iter().flat_map(|g| g.items.clone()).collect();
        let before = seen.len();
        seen.sort_by_key(|i| format!("{i:?}"));
        seen.dedup();
        assert_eq!(before, seen.len(), "an item appears twice");
        for item in RibbonItem::ALL {
            assert!(seen.contains(&item), "{item:?} is missing");
        }
    }

    #[test]
    fn sanitize_restores_an_item_a_stored_layout_never_heard_of() {
        let mut cfg = RibbonConfig::default();
        // Simulate a layout saved before FitWidth existed.
        for g in &mut cfg.groups {
            g.items.retain(|i| *i != RibbonItem::FitWidth);
        }
        cfg.sanitize();
        let zoom = cfg
            .groups
            .iter()
            .find(|g| g.group == RibbonGroup::Zoom)
            .expect("a zoom group");
        assert!(zoom.items.contains(&RibbonItem::FitWidth));
    }

    #[test]
    fn sanitize_drops_a_duplicate_rather_than_rendering_it_twice() {
        let mut cfg = RibbonConfig::default();
        cfg.groups[0].items.push(RibbonItem::Tool(ActiveTool::Pen));
        cfg.sanitize();
        let count = cfg
            .groups
            .iter()
            .flat_map(|g| g.items.iter())
            .filter(|i| **i == RibbonItem::Tool(ActiveTool::Pen))
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn sanitize_restores_a_missing_group() {
        let mut cfg = RibbonConfig::default();
        cfg.groups.retain(|g| g.group != RibbonGroup::Zoom);
        cfg.sanitize();
        assert!(cfg.groups.iter().any(|g| g.group == RibbonGroup::Zoom));
        // ...and its items came back with it.
        assert!(
            cfg.groups
                .iter()
                .flat_map(|g| g.items.iter())
                .any(|i| *i == RibbonItem::ZoomIn)
        );
    }

    #[test]
    fn moving_a_group_reorders_it() {
        let mut cfg = RibbonConfig::default();
        let first = cfg.groups[0].group;
        cfg.move_group(0, 2);
        assert_eq!(cfg.groups[2].group, first);
    }

    #[test]
    fn moving_an_item_takes_it_to_the_other_group() {
        let mut cfg = RibbonConfig::default();
        let item = cfg.groups[1].items[0];
        cfg.move_item(ItemAddr { group: 1, index: 0 }, 0);
        assert!(cfg.groups[0].items.contains(&item));
        assert!(!cfg.groups[1].items.contains(&item));
    }

    #[test]
    fn an_out_of_range_move_is_a_no_op() {
        let mut cfg = RibbonConfig::default();
        let before: Vec<_> = cfg.groups.iter().map(|g| g.items.len()).collect();
        cfg.move_item(ItemAddr { group: 9, index: 0 }, 0);
        cfg.move_item(
            ItemAddr {
                group: 0,
                index: 99,
            },
            1,
        );
        cfg.move_group(0, 99);
        let after: Vec<_> = cfg.groups.iter().map(|g| g.items.len()).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn an_empty_or_hidden_group_is_not_laid_out() {
        let mut cfg = RibbonConfig::default();
        assert_eq!(cfg.visible_groups().len(), 4);
        cfg.groups[0].visible = false;
        cfg.groups[1].items.clear();
        assert_eq!(cfg.visible_groups().len(), 2);
    }

    #[test]
    fn a_layout_survives_a_json_round_trip() {
        let mut cfg = RibbonConfig::default();
        cfg.move_group(0, 3);
        cfg.groups[1].visible = false;
        let json = serde_json::to_string(&cfg).expect("serialize");
        let mut back: RibbonConfig = serde_json::from_str(&json).expect("deserialize");
        back.sanitize();
        assert_eq!(back.groups[3].group, cfg.groups[3].group);
        assert!(!back.groups[1].visible);
        // Customize mode is a transient thing, not part of the layout.
        assert!(!back.customizing);
    }
}
