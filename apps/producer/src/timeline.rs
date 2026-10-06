//! The scene timeline: one track per layer, clips that can be moved, trimmed
//! and dragged between tracks, cue markers and drag-and-drop of media.

use crate::{
    end_action_name, format_timecode, kind_icon, EditMode, MediaDrag, ProducerApp, LIVE,
    LOOP_COLOR, MUTED, WARN,
};
use eframe::egui::{self, Color32, RichText};
use mapforge_core::{AssetKind, Transport};
use uuid::Uuid;

const LABEL_WIDTH: f32 = 170.0;
const RULER_HEIGHT: f32 = 26.0;
/// Cues and loops each get their own lane under the ruler.
const CUE_LANE: f32 = 26.0;
const LOOP_LANE: f32 = 26.0;
const HEADER_HEIGHT: f32 = RULER_HEIGHT + CUE_LANE + LOOP_LANE;
const ROW_HEIGHT: f32 = 46.0;
const DROP_ROW_HEIGHT: f32 = 40.0;
const EDGE: f32 = 7.0;
const SNAP_PIXELS: f32 = 8.0;
const CUE_COLOR: Color32 = Color32::from_rgb(240, 170, 40);
const PLAYHEAD_COLOR: Color32 = Color32::from_rgb(255, 72, 98);

/// Where the timeline was drawn last frame, so files dropped from the OS can
/// be placed at the time and track under the pointer.
pub struct DropZone {
    rect: egui::Rect,
    origin_x: f32,
    scale: f32,
    rows_top: f32,
    tracks: usize,
}

#[derive(Clone, Copy)]
enum ClipEdit {
    Move,
    TrimStart,
    TrimEnd,
}

fn clip_color(kind: &AssetKind, selected: bool) -> Color32 {
    let base = match kind {
        AssetKind::Video => Color32::from_rgb(48, 82, 155),
        AssetKind::Image => Color32::from_rgb(36, 112, 120),
        AssetKind::Audio => Color32::from_rgb(40, 120, 70),
        AssetKind::Unknown => Color32::from_gray(70),
    };
    if selected {
        base.gamma_multiply(1.6)
    } else {
        base
    }
}

/// Snaps `value` to the nearest target within `distance` seconds.
fn snap_time(value: f64, targets: &[f64], distance: f64) -> f64 {
    targets
        .iter()
        .copied()
        .filter(|t| (t - value).abs() < distance)
        .min_by(|a, b| (a - value).abs().total_cmp(&(b - value).abs()))
        .unwrap_or(value)
}

impl ProducerApp {
    /// Time and track under `pointer` if it is over the timeline. The track
    /// is the layer index to insert above, or `None` for a new top track.
    pub(crate) fn timeline_drop_target(&self, pointer: egui::Pos2) -> Option<(f64, Option<usize>)> {
        let zone = self.timeline_zone.as_ref()?;
        if !zone.rect.contains(pointer) {
            return None;
        }
        let time = ((pointer.x - zone.origin_x) / zone.scale).max(0.0) as f64;
        let row = ((pointer.y - zone.rows_top) / ROW_HEIGHT).floor();
        let track =
            (row >= 0.0 && (row as usize) < zone.tracks).then(|| zone.tracks - 1 - row as usize);
        Some(((time * 10.0).round() / 10.0, track))
    }

    fn timeline_end(&self) -> f64 {
        let scene = self.scene();
        let cues = scene
            .cues
            .iter()
            .map(|c| c.time)
            .chain(scene.loops.iter().map(|l| l.end))
            .fold(0.0, f64::max);
        scene.duration().max(cues).max(30.0)
    }

    pub(crate) fn timeline_ui(&mut self, ui: &mut egui::Ui) {
        let playing = self.player_transport() == Some(Transport::Playing);
        let scene_duration = self.scene().duration();
        ui.horizontal(|ui| {
            ui.label(RichText::new("TIMELINE").strong().color(MUTED));
            ui.label(RichText::new(&self.scene().name).strong());
            ui.separator();
            if ui.button("⏮").on_hover_text("Go to start").clicked() {
                self.seek(0.0);
            }
            if ui
                .button(if playing { "⏸" } else { "▶" })
                .on_hover_text("Play / pause from the playhead (Space)")
                .clicked()
            {
                self.toggle_play();
            }
            if ui.button("■").on_hover_text("Stop (Esc)").clicked() {
                self.stop();
            }
            ui.monospace(format_timecode(self.playhead_seconds));
            ui.separator();
            if ui
                .button(RichText::new("◆ + Cue").color(WARN))
                .on_hover_text("Mark a start point at the playhead (M)")
                .clicked()
            {
                self.add_cue(self.playhead_seconds);
            }
            if ui
                .button(RichText::new("⟲ + Loop").color(LOOP_COLOR))
                .on_hover_text(
                    "Repeat a section until you exit it (L). Uses the selected clip's span.",
                )
                .clicked()
            {
                self.add_loop();
            }
            ui.separator();
            ui.label(RichText::new("At the end").small().color(MUTED));
            let scene = &mut self.project.scenes[self.scene];
            egui::ComboBox::from_id_salt("timeline_end_action")
                .selected_text(end_action_name(scene.end_action))
                .show_ui(ui, |ui| {
                    use mapforge_core::EndAction::*;
                    for action in [Loop, Hold, Stop, Next] {
                        ui.selectable_value(&mut scene.end_action, action, end_action_name(action));
                    }
                });
            ui.separator();
            ui.label("Zoom");
            ui.add(egui::Slider::new(&mut self.timeline_scale, 4.0..=200.0).show_value(false));
            ui.label(
                RichText::new(format!(
                    "{} tracks · {}",
                    self.scene().layers.len(),
                    format_timecode(scene_duration)
                ))
                .color(MUTED),
            );
        });
        ui.separator();

        let scale = self.timeline_scale;
        let end = self.timeline_end();
        let track_count = self.scene().layers.len();
        let visible_width = ui.available_width();
        let canvas_width = (LABEL_WIDTH + (end as f32 + 20.0) * scale).max(visible_width);
        let canvas_height = HEADER_HEIGHT + track_count as f32 * ROW_HEIGHT + DROP_ROW_HEIGHT;

        egui::ScrollArea::both()
            .id_salt("timeline_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let (canvas, response) = ui.allocate_exact_size(
                    egui::vec2(canvas_width, canvas_height),
                    egui::Sense::click_and_drag(),
                );
                let origin_x = canvas.left() + LABEL_WIDTH;
                let rows_top = canvas.top() + HEADER_HEIGHT;
                let cue_lane = egui::Rect::from_min_size(
                    egui::pos2(canvas.left(), canvas.top() + RULER_HEIGHT),
                    egui::vec2(canvas.width(), CUE_LANE),
                );
                let loop_lane = egui::Rect::from_min_size(
                    egui::pos2(canvas.left(), cue_lane.bottom()),
                    egui::vec2(canvas.width(), LOOP_LANE),
                );
                let x_for = |t: f64| origin_x + t as f32 * scale;
                let time_at = |x: f32| ((x - origin_x) / scale).max(0.0) as f64;
                self.timeline_zone = Some(DropZone {
                    rect: ui.clip_rect().intersect(canvas),
                    origin_x,
                    scale,
                    rows_top,
                    tracks: track_count,
                });

                let painter = ui.painter().clone();
                painter.rect_filled(canvas, 0.0, Color32::from_rgb(18, 21, 29));
                for (lane, label, color) in [
                    (cue_lane, "◆ Cues", CUE_COLOR),
                    (loop_lane, "⟲ Loops", LOOP_COLOR),
                ] {
                    painter.rect_filled(lane, 0.0, Color32::from_rgb(24, 27, 36));
                    painter.line_segment(
                        [lane.left_bottom(), lane.right_bottom()],
                        egui::Stroke::new(1.0_f32, Color32::from_gray(40)),
                    );
                    painter.text(
                        egui::pos2(canvas.left() + 10.0, lane.center().y),
                        egui::Align2::LEFT_CENTER,
                        label,
                        egui::FontId::proportional(12.0),
                        color,
                    );
                }

                // Ruler.
                let major = if scale >= 90.0 {
                    1
                } else if scale >= 40.0 {
                    5
                } else if scale >= 12.0 {
                    10
                } else {
                    30
                };
                let minor = if scale >= 40.0 { 1 } else { major / 5 }.max(1);
                for second in (0..=(end as usize + 20)).step_by(minor) {
                    let x = x_for(second as f64);
                    let is_major = second % major == 0;
                    painter.line_segment(
                        [
                            egui::pos2(
                                x,
                                canvas.top() + RULER_HEIGHT - if is_major { 12.0 } else { 5.0 },
                            ),
                            egui::pos2(x, canvas.top() + RULER_HEIGHT),
                        ],
                        egui::Stroke::new(
                            1.0_f32,
                            Color32::from_gray(if is_major { 130 } else { 65 }),
                        ),
                    );
                    if is_major {
                        painter.text(
                            egui::pos2(x + 4.0, canvas.top() + 3.0),
                            egui::Align2::LEFT_TOP,
                            format_timecode(second as f64),
                            egui::FontId::monospace(10.0),
                            Color32::LIGHT_GRAY,
                        );
                    }
                }

                // Snap targets: start, playhead, cues, loops and every clip edge.
                let scene = self.scene();
                let mut targets = vec![0.0, self.playhead_seconds];
                targets.extend(scene.cues.iter().map(|c| c.time));
                for region in &scene.loops {
                    targets.push(region.start);
                    targets.push(region.end);
                }
                for layer in &scene.layers {
                    targets.push(layer.timeline_start);
                    targets.push(layer.timeline_end());
                }
                let snap_distance = (SNAP_PIXELS / scale) as f64;

                // Tracks, top layer first.
                let mut edit: Option<(Uuid, ClipEdit, f64, f32)> = None;
                let mut select = None;
                let layers = self.project.scenes[self.scene].layers.clone();
                for (row, layer) in layers.iter().rev().enumerate() {
                    let top = rows_top + row as f32 * ROW_HEIGHT;
                    let row_rect = egui::Rect::from_min_size(
                        egui::pos2(canvas.left(), top),
                        egui::vec2(canvas.width(), ROW_HEIGHT),
                    );
                    painter.rect_filled(
                        row_rect,
                        0.0,
                        if row % 2 == 0 {
                            Color32::from_rgb(22, 26, 35)
                        } else {
                            Color32::from_rgb(19, 23, 31)
                        },
                    );
                    let asset = self.asset(layer.asset_id).cloned();
                    let kind = asset
                        .as_ref()
                        .map_or(AssetKind::Unknown, |a| a.kind.clone());
                    painter.text(
                        egui::pos2(canvas.left() + 10.0, top + ROW_HEIGHT * 0.5),
                        egui::Align2::LEFT_CENTER,
                        format!("{} {}", kind_icon(&kind), layer.name),
                        egui::FontId::proportional(12.0),
                        Color32::LIGHT_GRAY,
                    );

                    let clip = egui::Rect::from_min_max(
                        egui::pos2(x_for(layer.timeline_start), top + 4.0),
                        egui::pos2(
                            x_for(layer.timeline_end()).max(x_for(layer.timeline_start) + 8.0),
                            top + ROW_HEIGHT - 4.0,
                        ),
                    );
                    let selected = self.selected_layer == Some(layer.id);
                    let clip_painter = painter.with_clip_rect(clip.intersect(ui.clip_rect()));
                    clip_painter.rect_filled(clip, 5.0, clip_color(&kind, selected));

                    // Picture thumbnail or waveform inside the clip.
                    if let Some(tex) = self.thumbs.textures.get(&layer.asset_id) {
                        let [tw, th] = tex.size();
                        if kind == AssetKind::Audio {
                            let media = asset
                                .as_ref()
                                .and_then(|a| a.duration_seconds)
                                .unwrap_or(layer.timeline_duration)
                                .max(0.1);
                            let width = media as f32 * scale;
                            let mut left = clip.left() - layer.source_offset as f32 * scale;
                            while left < clip.right() {
                                clip_painter.image(
                                    tex.id(),
                                    egui::Rect::from_min_size(
                                        egui::pos2(left, clip.top() + 4.0),
                                        egui::vec2(width, clip.height() - 8.0),
                                    ),
                                    egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    ),
                                    Color32::WHITE.gamma_multiply(0.8),
                                );
                                if !layer.looping {
                                    break;
                                }
                                left += width;
                            }
                        } else {
                            let h = clip.height() - 6.0;
                            let w = h * tw as f32 / th.max(1) as f32;
                            clip_painter.image(
                                tex.id(),
                                egui::Rect::from_min_size(
                                    egui::pos2(clip.left() + 3.0, clip.top() + 3.0),
                                    egui::vec2(w, h),
                                ),
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                Color32::WHITE,
                            );
                        }
                    }
                    let mut label = layer.name.clone();
                    if layer.looping {
                        label.push_str("  ⟲");
                    }
                    let text_left = if kind == AssetKind::Audio {
                        6.0
                    } else {
                        (clip.height() - 6.0) * 16.0 / 9.0 + 10.0
                    };
                    clip_painter.text(
                        egui::pos2(clip.left() + text_left, clip.top() + 5.0),
                        egui::Align2::LEFT_TOP,
                        label,
                        egui::FontId::proportional(12.0),
                        Color32::WHITE,
                    );
                    clip_painter.rect_stroke(
                        clip,
                        5.0,
                        egui::Stroke::new(
                            if selected { 2.0_f32 } else { 1.0_f32 },
                            Color32::from_rgb(170, 200, 255),
                        ),
                        egui::StrokeKind::Inside,
                    );

                    // Body first, then edges on top so they win the pointer.
                    let body = ui
                        .interact(
                            clip,
                            egui::Id::new(("clip", layer.id)),
                            egui::Sense::click_and_drag(),
                        )
                        .on_hover_cursor(egui::CursorIcon::Grab);
                    let left_edge = ui
                        .interact(
                            egui::Rect::from_min_size(
                                clip.left_top(),
                                egui::vec2(EDGE, clip.height()),
                            ),
                            egui::Id::new(("clip_start", layer.id)),
                            egui::Sense::drag(),
                        )
                        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
                    let right_edge = ui
                        .interact(
                            egui::Rect::from_min_size(
                                clip.right_top() - egui::vec2(EDGE, 0.0),
                                egui::vec2(EDGE, clip.height()),
                            ),
                            egui::Id::new(("clip_end", layer.id)),
                            egui::Sense::drag(),
                        )
                        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
                    if body.clicked() || body.drag_started() {
                        select = Some(layer.id);
                    }
                    for (response, kind) in [
                        (&body, ClipEdit::Move),
                        (&left_edge, ClipEdit::TrimStart),
                        (&right_edge, ClipEdit::TrimEnd),
                    ] {
                        if response.dragged()
                            && response.drag_delta().x.abs() + response.drag_delta().y.abs() > 0.0
                        {
                            let pointer_y = response.interact_pointer_pos().map_or(top, |p| p.y);
                            edit = Some((
                                layer.id,
                                kind,
                                (response.drag_delta().x / scale) as f64,
                                pointer_y,
                            ));
                        }
                    }
                }
                if let Some(id) = select {
                    self.selected_layer = Some(id);
                    self.selected_cue = None;
                    if self
                        .asset(
                            layers
                                .iter()
                                .find(|l| l.id == id)
                                .map_or(Uuid::nil(), |l| l.asset_id),
                        )
                        .is_some_and(|a| a.kind != AssetKind::Audio)
                    {
                        self.mode = EditMode::Layers;
                    }
                }
                if let Some((id, kind, delta, pointer_y)) = edit {
                    let others: Vec<f64> = targets.clone();
                    let layers = &mut self.project.scenes[self.scene].layers;
                    if let Some(index) = layers.iter().position(|l| l.id == id) {
                        let layer = &mut layers[index];
                        match kind {
                            ClipEdit::Move => {
                                let start = (layer.timeline_start + delta).max(0.0);
                                // Snap either edge, whichever is closer.
                                let snapped_start = snap_time(start, &others, snap_distance);
                                let snapped_end = snap_time(
                                    start + layer.timeline_duration,
                                    &others,
                                    snap_distance,
                                );
                                layer.timeline_start = if snapped_start != start {
                                    snapped_start
                                } else if snapped_end != start + layer.timeline_duration {
                                    (snapped_end - layer.timeline_duration).max(0.0)
                                } else {
                                    start
                                };
                                // Dragging up or down moves the clip to another track.
                                let row = ((pointer_y - rows_top) / ROW_HEIGHT).floor();
                                if row >= 0.0 && (row as usize) < layers.len() {
                                    let target = layers.len() - 1 - row as usize;
                                    if target != index {
                                        let moved = layers.remove(index);
                                        layers.insert(target, moved);
                                    }
                                }
                            }
                            ClipEdit::TrimStart => {
                                let end = layer.timeline_end();
                                let start = snap_time(
                                    (layer.timeline_start + delta).clamp(0.0, end - 0.1),
                                    &others,
                                    snap_distance,
                                )
                                .min(end - 0.1);
                                let shift = start - layer.timeline_start;
                                // Trimming the front skips into the media.
                                if layer.source_offset + shift >= 0.0 {
                                    layer.source_offset += shift;
                                    layer.timeline_start = start;
                                    layer.timeline_duration = end - start;
                                }
                            }
                            ClipEdit::TrimEnd => {
                                let end =
                                    snap_time(layer.timeline_end() + delta, &others, snap_distance);
                                layer.timeline_duration = (end - layer.timeline_start).max(0.1);
                            }
                        }
                    }
                }

                // Drop row at the bottom.
                let drop_top = rows_top + track_count as f32 * ROW_HEIGHT;
                let drop_rect = egui::Rect::from_min_size(
                    egui::pos2(canvas.left(), drop_top),
                    egui::vec2(canvas.width(), DROP_ROW_HEIGHT),
                );
                painter.rect_filled(drop_rect, 0.0, Color32::from_rgb(15, 17, 24));
                painter.text(
                    egui::pos2(canvas.left() + 10.0, drop_rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    "+ Drop images, videos or music here for a new track",
                    egui::FontId::proportional(12.0),
                    MUTED,
                );

                // Media dragged from the library.
                if response.dnd_hover_payload::<MediaDrag>().is_some() {
                    if let Some(pointer) = response.hover_pos() {
                        painter.line_segment(
                            [
                                egui::pos2(pointer.x, rows_top),
                                egui::pos2(pointer.x, canvas.bottom()),
                            ],
                            egui::Stroke::new(2.0_f32, LIVE),
                        );
                    }
                }
                if let Some(payload) = response.dnd_release_payload::<MediaDrag>() {
                    let target = response
                        .hover_pos()
                        .or(response.interact_pointer_pos())
                        .and_then(|p| self.timeline_drop_target(p));
                    let (start, track) = target.unwrap_or((self.playhead_seconds, None));
                    self.add_layer_at(payload.0, start, track);
                }

                // Cue markers on the ruler and through the tracks.
                let cues = self.scene().cues.clone();
                for cue in &cues {
                    let x = x_for(cue.time);
                    let selected = self.selected_cue == Some(cue.id);
                    let marker = egui::Rect::from_center_size(
                        egui::pos2(x, cue_lane.center().y),
                        egui::vec2(14.0, 14.0),
                    );
                    painter.add(egui::Shape::convex_polygon(
                        vec![
                            egui::pos2(x, marker.top()),
                            egui::pos2(marker.right(), marker.center().y),
                            egui::pos2(x, marker.bottom()),
                            egui::pos2(marker.left(), marker.center().y),
                        ],
                        CUE_COLOR,
                        egui::Stroke::new(if selected { 2.0_f32 } else { 0.0_f32 }, Color32::WHITE),
                    ));
                    painter.extend(egui::Shape::dashed_line(
                        &[
                            egui::pos2(x, cue_lane.bottom()),
                            egui::pos2(x, canvas.bottom()),
                        ],
                        egui::Stroke::new(1.0_f32, CUE_COLOR.gamma_multiply(0.5)),
                        4.0,
                        4.0,
                    ));
                    let label = if cue.hotkey.is_empty() {
                        cue.name.clone()
                    } else {
                        format!("{} [{}]", cue.name, cue.hotkey)
                    };
                    painter.text(
                        egui::pos2(x + 10.0, cue_lane.center().y),
                        egui::Align2::LEFT_CENTER,
                        label,
                        egui::FontId::proportional(11.0),
                        CUE_COLOR,
                    );
                    let handle = ui
                        .interact(
                            marker.expand(3.0),
                            egui::Id::new(("cue", cue.id)),
                            egui::Sense::click_and_drag(),
                        )
                        .on_hover_text("Drag to move · click to select")
                        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
                    if handle.clicked() || handle.drag_started() {
                        self.selected_cue = Some(cue.id);
                        self.playhead_seconds = cue.time;
                    }
                    if handle.dragged() {
                        if let Some(pointer) = handle.interact_pointer_pos() {
                            let time = (time_at(pointer.x) * 10.0).round() / 10.0;
                            if let Some(c) =
                                self.scene_mut().cues.iter_mut().find(|c| c.id == cue.id)
                            {
                                c.time = time;
                            }
                            self.playhead_seconds = time;
                        }
                    }
                    if handle.drag_stopped() {
                        self.scene_mut()
                            .cues
                            .sort_by(|a, b| a.time.total_cmp(&b.time));
                    }
                }

                // Loop points: a ⟲ marker where the show jumps back, with a line
                // to the point it jumps back to.
                let loops = self.scene().loops.clone();
                let cue_names: Vec<(f64, String)> = self
                    .scene()
                    .cues
                    .iter()
                    .map(|c| (c.time, c.name.clone()))
                    .collect();
                let live_loop = self.player.as_ref().and_then(|s| s.loop_name.clone());
                // (loop, which part, new time or move delta)
                let mut loop_edit: Option<(uuid::Uuid, ClipEdit, f64)> = None;
                let lane_y = loop_lane.center().y;
                for region in &loops {
                    let left = x_for(region.start);
                    let right = x_for(region.end).max(left + 6.0);
                    let selected = self.selected_loop == Some(region.id);
                    let live = live_loop.as_deref() == Some(region.name.as_str());
                    let strength = if selected || live { 1.0 } else { 0.7 };

                    // Repeated section, lightly shaded through the tracks.
                    painter.rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(left, rows_top),
                            egui::pos2(right, rows_top + track_count as f32 * ROW_HEIGHT),
                        ),
                        0.0,
                        LOOP_COLOR.gamma_multiply(if live { 0.14 } else { 0.05 }),
                    );
                    painter.extend(egui::Shape::dashed_line(
                        &[
                            egui::pos2(right, loop_lane.bottom()),
                            egui::pos2(right, canvas.bottom()),
                        ],
                        egui::Stroke::new(1.0_f32, LOOP_COLOR.gamma_multiply(0.6)),
                        4.0,
                        4.0,
                    ));
                    let line = egui::Rect::from_min_max(
                        egui::pos2(left, lane_y - 5.0),
                        egui::pos2(right, lane_y + 5.0),
                    );
                    painter.line_segment(
                        [egui::pos2(left, lane_y), egui::pos2(right, lane_y)],
                        egui::Stroke::new(2.0_f32, LOOP_COLOR.gamma_multiply(strength * 0.8)),
                    );
                    // Where it jumps back to.
                    let start_handle = egui::Rect::from_center_size(
                        egui::pos2(left, lane_y),
                        egui::vec2(10.0, 16.0),
                    );
                    painter.add(egui::Shape::convex_polygon(
                        vec![
                            egui::pos2(left, lane_y - 7.0),
                            egui::pos2(left + 6.0, lane_y),
                            egui::pos2(left, lane_y + 7.0),
                        ],
                        LOOP_COLOR.gamma_multiply(strength),
                        egui::Stroke::NONE,
                    ));
                    // The loop point itself.
                    let marker = egui::Rect::from_center_size(
                        egui::pos2(right, lane_y),
                        egui::vec2(20.0, 18.0),
                    );
                    painter.rect_filled(marker, 4.0, LOOP_COLOR.gamma_multiply(strength));
                    if selected {
                        painter.rect_stroke(
                            marker,
                            4.0,
                            egui::Stroke::new(2.0_f32, Color32::WHITE),
                            egui::StrokeKind::Outside,
                        );
                    }
                    painter.text(
                        marker.center(),
                        egui::Align2::CENTER_CENTER,
                        "⟲",
                        egui::FontId::proportional(13.0),
                        Color32::BLACK,
                    );
                    let back_to = if region.start <= 0.0 {
                        "start".to_string()
                    } else {
                        cue_names
                            .iter()
                            .find(|(t, _)| (t - region.start).abs() < 0.05)
                            .map(|(_, n)| n.clone())
                            .unwrap_or_else(|| format!("{:.1}s", region.start))
                    };
                    let mut label = region.name.clone();
                    if !region.hotkey.is_empty() {
                        label.push_str(&format!(" [{}]", region.hotkey));
                    }
                    label.push_str(&format!(" · back to {back_to}"));
                    if region.count > 0 {
                        label.push_str(&format!(" ×{}", region.count));
                    }
                    painter.text(
                        egui::pos2(marker.right() + 6.0, lane_y),
                        egui::Align2::LEFT_CENTER,
                        label,
                        egui::FontId::proportional(11.0),
                        LOOP_COLOR,
                    );

                    let body = ui
                        .interact(
                            line,
                            egui::Id::new(("loop", region.id)),
                            egui::Sense::click_and_drag(),
                        )
                        .on_hover_text("The section that repeats · drag to move the whole loop")
                        .on_hover_cursor(egui::CursorIcon::Grab);
                    let start = ui
                        .interact(
                            start_handle.expand(2.0),
                            egui::Id::new(("loop_start", region.id)),
                            egui::Sense::click_and_drag(),
                        )
                        .on_hover_text("Where it jumps back to — drag to change")
                        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
                    let end = ui
                        .interact(
                            marker.expand(3.0),
                            egui::Id::new(("loop_end", region.id)),
                            egui::Sense::click_and_drag(),
                        )
                        .on_hover_text("Loop point — drag to move · the show jumps back from here")
                        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
                    for response in [&body, &start, &end] {
                        if response.clicked() || response.drag_started() {
                            self.selected_loop = Some(region.id);
                            self.selected_cue = None;
                        }
                    }
                    if body.dragged() && body.drag_delta().x != 0.0 {
                        loop_edit = Some((
                            region.id,
                            ClipEdit::Move,
                            (body.drag_delta().x / scale) as f64,
                        ));
                    }
                    for (response, kind) in
                        [(&start, ClipEdit::TrimStart), (&end, ClipEdit::TrimEnd)]
                    {
                        if response.dragged() {
                            if let Some(pointer) = response.interact_pointer_pos() {
                                loop_edit = Some((region.id, kind, time_at(pointer.x)));
                                self.playhead_seconds = time_at(pointer.x);
                            }
                        }
                    }
                }
                if let Some((id, kind, value)) = loop_edit {
                    let others: Vec<f64> = targets.clone();
                    if let Some(region) = self.scene_mut().loops.iter_mut().find(|l| l.id == id) {
                        let length = region.end - region.start;
                        match kind {
                            ClipEdit::Move => {
                                let start = snap_time(
                                    (region.start + value).max(0.0),
                                    &others,
                                    snap_distance,
                                );
                                region.start = start;
                                region.end = start + length;
                            }
                            ClipEdit::TrimStart => {
                                region.start = snap_time(value, &others, snap_distance)
                                    .clamp(0.0, region.end - 0.1);
                            }
                            ClipEdit::TrimEnd => {
                                region.end = snap_time(value, &others, snap_distance)
                                    .max(region.start + 0.1);
                            }
                        }
                    }
                }

                // Scene end.
                if scene_duration > 0.0 {
                    let x = x_for(scene_duration);
                    painter.extend(egui::Shape::dashed_line(
                        &[egui::pos2(x, canvas.top()), egui::pos2(x, drop_top)],
                        egui::Stroke::new(1.5_f32, Color32::from_rgb(200, 90, 90)),
                        6.0,
                        4.0,
                    ));
                    painter.text(
                        egui::pos2(x + 4.0, drop_top - 4.0),
                        egui::Align2::LEFT_BOTTOM,
                        format!("end · {}", end_action_name(self.scene().end_action)),
                        egui::FontId::proportional(10.0),
                        Color32::from_rgb(220, 120, 120),
                    );
                }

                // Clicking or dragging on the ruler or empty space moves the playhead.
                if let Some(pointer) = response.interact_pointer_pos() {
                    if (response.clicked() || response.dragged()) && pointer.x >= origin_x {
                        self.playhead_seconds = time_at(pointer.x).min(end + 20.0);
                        if response.clicked() || response.drag_stopped() {
                            self.seek(self.playhead_seconds);
                        }
                    }
                }
                // Double-click a lane to add a cue or a loop at that time.
                if response.double_clicked() {
                    if let Some(pointer) = response.interact_pointer_pos() {
                        let time = (time_at(pointer.x) * 10.0).round() / 10.0;
                        if cue_lane.contains(pointer) {
                            self.add_cue(time);
                        } else if loop_lane.contains(pointer) {
                            self.selected_layer = None;
                            self.playhead_seconds = time;
                            self.add_loop();
                        }
                    }
                }
                if response.clicked()
                    && response
                        .interact_pointer_pos()
                        .is_some_and(|p| p.y > rows_top)
                {
                    self.selected_layer = None;
                    self.selected_cue = None;
                }

                // Label column drawn last so clips scroll underneath it.
                let labels = egui::Rect::from_min_size(
                    egui::pos2(ui.clip_rect().left(), canvas.top()),
                    egui::vec2(LABEL_WIDTH, canvas.height()),
                );
                let _ = labels;

                let x = x_for(self.playhead_seconds);
                painter.line_segment(
                    [egui::pos2(x, canvas.top()), egui::pos2(x, canvas.bottom())],
                    egui::Stroke::new(2.0_f32, PLAYHEAD_COLOR),
                );
                painter.circle_filled(egui::pos2(x, canvas.top() + 4.0), 5.0, PLAYHEAD_COLOR);
            });
    }

    pub(crate) fn update_playhead(&mut self, delta: f64) {
        let Some(state) = &self.player else {
            return;
        };
        if state.scene_id != Some(self.scene().id) {
            return;
        }
        if (state.position_seconds - self.last_player_position).abs() > f64::EPSILON {
            self.last_player_position = state.position_seconds;
            self.playhead_seconds = state.position_seconds;
        } else if state.transport == Transport::Playing {
            self.playhead_seconds += delta;
        }
    }
}
