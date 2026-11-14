//! Type on a path in a PSD: the bridge between the document's [`TextShape`] and
//! the reimplemented engine's [`photocraft_text_re::psd::ShapeSpec`].
//!
//! The path lives only in the type layer's frame in the document's `Txt2` block
//! (EngineData marks such a layer as box text, `ShapeType 1`, with the path's
//! bounds as its `BoxBounds`). Reading, measuring and writing that frame is now
//! the engine's (`photocraft_text_re::psd::{read_shape, to_frame_parts}`, ported
//! from this file): this module keeps only the two model translations and the
//! frame-list entry plumbing the document save still edits. The engine lays type
//! on a path out from the path's last anchor for a closed path, its first for an
//! open one, with `/2 /6` in curve-segment units — see the engine's `psd::path`
//! module docs for the block format.

use photocraft_doc::text::TextShape;
use photocraft_doc::vector::{Knot, PathOp, Subpath};
use photocraft_geom::Point;
use photocraft_text_re::engine_data::Value as Re;
use photocraft_text_re::psd::{FrameSource, FrameSpec, PathKnot, PathSpec, ShapeSpec, to_frame_parts};
use photocraft_text_re::txt2::FramePath;

use crate::engine_data::Value as E;

/// The `Txt2` frame kind (`/2 /0`) of type on a path.
pub(crate) const PATH_FRAME: i64 = photocraft_text_re::psd::PATH_FRAME;
/// The `Txt2` frame kind (`/2 /0`) of a text box.
const BOX_FRAME: i64 = 1;
/// How close (px) two points must be to count as the same path.
const SAME: f64 = 1e-3;
/// How close (px) two arc lengths must be to count as the same placement.
const SAME_ARC: f64 = 1e-3;

fn same(a: Point, b: Point) -> bool {
    (a.x - b.x).hypot(a.y - b.y) <= SAME
}

/// The engine's `EngineData` value for this crate's parsed one: the two parsers
/// write the same syntax but are different types.
fn engine_value(value: &E) -> Re {
    match value {
        E::Dict(items) => Re::Dict(items.iter().map(|(key, value)| (key.clone(), engine_value(value))).collect()),
        E::Array(items) => Re::Array(items.iter().map(engine_value).collect()),
        E::String(s) | E::RawString(s) | E::Name(s) => Re::Str(s.clone()),
        E::Int(i) => Re::Int(*i),
        E::Real(r) => Re::Float(*r),
        E::Bool(b) => Re::Bool(*b),
    }
}

/// The kind of the frame dictionary `frame` (`/2 /0`): 0 point text, 1 box, 2
/// path. The engine reads the kind from the same `/2 /0` key.
pub(crate) fn frame_kind(frame: &E) -> i64 {
    photocraft_text_re::psd::frame_kind(&engine_value(frame))
}

fn knot_of(knot: &PathKnot) -> Knot {
    Knot {
        anchor: Point::new(f64::from(knot.anchor[0]), f64::from(knot.anchor[1])),
        in_ctrl: Point::new(f64::from(knot.in_ctrl[0]), f64::from(knot.in_ctrl[1])),
        out_ctrl: Point::new(f64::from(knot.out_ctrl[0]), f64::from(knot.out_ctrl[1])),
        smooth: knot.smooth.unwrap_or(false),
    }
}

fn path_knot(knot: &Knot) -> PathKnot {
    PathKnot {
        anchor: [knot.anchor.x as f32, knot.anchor.y as f32],
        in_ctrl: [knot.in_ctrl.x as f32, knot.in_ctrl.y as f32],
        out_ctrl: [knot.out_ctrl.x as f32, knot.out_ctrl.y as f32],
        smooth: Some(knot.smooth),
    }
}

/// The engine's path spec for `path` with the type's placement on it (text-space
/// units: the engine does not scale these by the document resolution).
fn path_spec(path: &Subpath, start: f32, end: Option<f32>, flip: bool) -> PathSpec {
    PathSpec {
        knots: path.knots.iter().map(path_knot).collect(),
        closed: path.closed,
        start,
        end,
        flip,
        matrix: None,
        kind: Some("path".into()),
    }
}

/// Type on a path from a path frame `frame` (the entry's `/0`): its path in the
/// `TySh` text space and the type's range on it. `Err` says why the path can't be
/// read (the caller keeps the box or point text EngineData describes).
pub(crate) fn read_shape(frame: &E) -> Result<TextShape, String> {
    let value = engine_value(frame);
    let spec = photocraft_text_re::psd::read_shape(FrameSource::EngineData(&value)).ok_or("its type path cannot be read")?;
    let path = spec.path.ok_or("its type path cannot be read")?;
    Ok(TextShape::Path {
        path: Subpath { closed: path.closed, knots: path.knots.iter().map(knot_of).collect(), op: PathOp::Combine },
        start: path.start,
        end: path.end,
        flip: path.flip,
    })
}

/// Whether two type paths are the same within [`SAME`] (the smooth flags aside).
fn same_path(a: &Subpath, b: &Subpath) -> bool {
    a.closed == b.closed && a.knots.len() == b.knots.len() && a.knots.iter().zip(&b.knots).all(|(p, q)| same(p.anchor, q.anchor) && same(p.in_ctrl, q.in_ctrl) && same(p.out_ctrl, q.out_ctrl))
}

/// Whether two type-on-path shapes place the type the same: the same path within
/// [`SAME`] and the same range within [`SAME_ARC`].
fn same_shape(a: &TextShape, b: &TextShape) -> bool {
    let close = |x: f32, y: f32| (f64::from(x) - f64::from(y)).abs() <= SAME_ARC * 10.0;
    match (a, b) {
        (TextShape::Path { path: p, start: s, end: e, flip: f }, TextShape::Path { path: q, start: t, end: g, flip: h }) => {
            f == h
                && same_path(p, q)
                && close(*s, *t)
                && match (e, g) {
                    (None, None) => true,
                    (Some(x), Some(y)) => close(*x, *y),
                    _ => false,
                }
        }
        _ => false,
    }
}

fn real(v: f64) -> E {
    E::Real(v)
}

/// The point-text frame Photoshop writes (`<< /0 << /2 << /6 [ -1.0 -1.0 ] /11 <<
/// /4 -1 /7 false >> >> >> >>`; also what [`crate::psd`] pads the frame list with).
#[allow(dead_code)] // the E frame writer's point frame; the adapter uses the engine writer
pub(crate) fn point_frame() -> E {
    point_frame_oriented(false)
}

/// [`point_frame`] for vertical text: Photoshop marks the frame's text flow with `/2 /1 2`
/// (a vertical box carries the same key beside its `/0 1` kind). Without it Photoshop opens the
/// layer horizontal even though the `TySh` `Ornt` and the engine data say vertical.
fn point_frame_oriented(vertical: bool) -> E {
    let kind = E::Dict(vec![("4".into(), E::Int(-1)), ("7".into(), E::Bool(false))]);
    let mut data = Vec::with_capacity(3);
    if vertical {
        data.push(("1".into(), E::Int(2)));
    }
    data.push(("6".into(), E::Array(vec![real(-1.0), real(-1.0)])));
    data.push(("11".into(), kind));
    E::Dict(vec![("0".into(), E::Dict(vec![("2".into(), E::Dict(data))]))])
}

/// The frame-list entry (`<< /0 frame >>`) for an engine [`FramePath`].
fn frame_entry(parts: &FramePath, with_matrix: bool) -> E {
    let mut data = vec![("0".into(), E::Int(parts.kind)), ("6".into(), E::Array(parts.range.iter().map(|v| real(*v)).collect()))];
    if with_matrix {
        // The engine writes text-space points, so the frame plainly maps into
        // the `TySh` text space (the engine's `txt2` writer emits the identity).
        let matrix = parts.matrix.unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        data.push(("2".into(), E::Array(matrix.iter().map(|v| real(*v)).collect())));
    }
    if let Some(flip) = parts.flip {
        data.push(("11".into(), E::Dict(vec![("0".into(), E::Bool(flip))])));
    }
    let points = parts.points.iter().map(|v| real(*v)).collect();
    E::Dict(vec![(
        "0".into(),
        E::Dict(vec![("1".into(), E::Dict(vec![("0".into(), E::Array(points))])), ("2".into(), E::Dict(data))]),
    )])
}

/// A text box's frame as Photoshop writes it (`text/paragraph-box-left.psd`): the
/// rectangle as four straight segments from its top left, clockwise. The points
/// are the engine's canonical box path (`to_frame_parts` keeps them verbatim).
fn box_frame(x: f64, y: f64, w: f64, h: f64) -> E {
    box_frame_oriented(x, y, w, h, false)
}

/// [`box_frame`] for vertical text: Photoshop writes the text-flow marker `/2 /1 2` beside the
/// `/0 1` box kind, and the layout details `/11` as `/4 -2 /7 false` (a vertical box without the
/// marker opens as horizontal text).
fn box_frame_oriented(x: f64, y: f64, w: f64, h: f64, vertical: bool) -> E {
    let Some(parts) = to_frame_parts(&box_spec(x, y, w, h)) else { return point_frame_oriented(vertical) };
    let mut entry = frame_entry(&parts, false);
    // Photoshop's box frame also carries the layout details under `/2 /11`.
    if let Some(data @ E::Dict(_)) = entry.get_mut("0").and_then(|frame| frame.get_mut("2")) {
        if vertical {
            data.set("1", E::Int(2));
        }
        data.set("11", E::Dict(vec![("4".into(), E::Int(-2)), ("7".into(), E::Bool(false))]));
    }
    entry
}

/// Photoshop's box outline for a text box: four straight cubic segments from the
/// top left, clockwise (8 numbers each, the shared anchors repeated).
fn box_spec(x: f64, y: f64, w: f64, h: f64) -> ShapeSpec {
    let corners = [(x, y), (x + w, y), (x + w, y + h), (x, y + h), (x, y)];
    let mut points = Vec::with_capacity(32);
    for pair in corners.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        for p in [a, a, b, b] {
            points.push(p.0);
            points.push(p.1);
        }
    }
    ShapeSpec {
        frame: Some(FrameSpec { points, kind: BOX_FRAME, range: [-2.0, -2.0], flip: None, matrix: None }),
        path: None,
    }
}

/// The canonical engine [`ShapeSpec`] for `shape`: the path plus the type's
/// placement, or the rectangle as Photoshop's kind-1 box path. The engine's
/// writer bakes the start and flip into a path frame and fills `/2 /6`, so a
/// path spec deliberately has no stored `frame`; a box spec is its four straight
/// segments (the writer keeps them verbatim).
pub(crate) fn shape_spec(shape: &TextShape) -> Option<ShapeSpec> {
    match shape {
        TextShape::Path { path, start, end, flip } => Some(ShapeSpec { frame: None, path: Some(path_spec(path, *start, *end, *flip)) }),
        TextShape::Box { x, y, width, height } => Some(box_spec(f64::from(*x), f64::from(*y), f64::from(*width), f64::from(*height))),
        TextShape::Area { path } => {
            let (x0, y0, x1, y1) = path.control_bounds()?;
            Some(box_spec(x0, y0, x1 - x0, y1 - y0))
        }
        TextShape::Point => None,
    }
}

/// The frame kind and text-space segment points the engine writes for `shape`'s
/// path frame: the canonical baked frame (what the `TySh` bounds are measured
/// from). Empty for any other shape.
pub(crate) fn shape_for_frame(shape: &TextShape) -> (i64, Vec<[f32; 2]>) {
    let Some(parts) = shape_spec(shape).and_then(|spec| to_frame_parts(&spec)) else { return (PATH_FRAME, Vec::new()) };
    let points = parts.points.as_chunks::<2>().0.iter().map(|&[x, y]| [x as f32, y as f32]).collect();
    (parts.kind, points)
}

/// Brings the frame list entry `entry` (`<< /0 frame >>`) of a type layer up to
/// date with its `shape`: type on a path gets a path frame for its path (left
/// byte for byte when it already holds that path, e.g. Photoshop's own after an
/// unedited save), and a layer no longer on a path loses its path frame (else it
/// would reopen on the path).
pub(crate) fn update_frame(entry: &mut E, shape: &TextShape) {
    update_frame_oriented(entry, shape, false);
}

/// Make the frame's text-flow marker say what the layer says, keeping the rest of the frame as
/// the file wrote it. A vertical layer whose frame lacks `/2 /1 2` opens horizontal in Photoshop
/// even when the `TySh` `Ornt` and the engine data say vertical (and a horizontal layer whose
/// frame still carries the marker opens vertical); the marker is all Photoshop reads for this.
/// Used for objects the save keeps from the file: their frames are not rewritten by
/// [`update_frame_oriented`], so a file written before the marker existed would keep the wrong
/// flow (the u12 vertical layers opened horizontal after a plain resave).
pub(crate) fn set_frame_orientation(entry: &mut E, vertical: bool) {
    let Some(E::Dict(frame_items)) = entry.get_mut("0") else { return };
    let Some((_, E::Dict(data_items))) = frame_items.iter_mut().find(|(k, _)| k == "2") else { return };
    let kind = data_items.iter().find(|(k, _)| k == "0").and_then(|(_, v)| v.as_i64()).unwrap_or(0);
    let marked = data_items.iter().any(|(k, v)| k == "1" && v.as_i64() == Some(2));
    if vertical == marked {
        return;
    }
    if vertical {
        data_items.push(("1".into(), E::Int(2)));
    } else {
        data_items.retain(|(k, _)| k != "1");
    }
    // Photoshop's layout details for the frame: `-2` for a text box, `-1` for point text.
    let base = if kind == BOX_FRAME { -2 } else { -1 };
    let details = E::Dict(vec![("4".into(), E::Int(base)), ("7".into(), E::Bool(false))]);
    match data_items.iter_mut().find(|(k, _)| k == "11") {
        Some((_, slot)) => *slot = details,
        None => data_items.push(("11".into(), details)),
    }
}

/// [`update_frame`] with the layer's text flow: vertical text needs Photoshop's `/2 /1 2` frame
/// marker, or Photoshop opens the layer horizontal and re-flows it.
pub(crate) fn update_frame_oriented(entry: &mut E, shape: &TextShape, vertical: bool) {
    let frame = entry.get("0");
    let kind = frame.map_or(0, frame_kind);
    match shape {
        TextShape::Path { path, start, end, flip } => {
            let spec = ShapeSpec { frame: None, path: Some(path_spec(path, *start, *end, *flip)) };
            let Some(parts) = to_frame_parts(&spec) else { return };
            let fresh = frame_entry(&parts, true);
            if kind == PATH_FRAME
                && frame
                    .and_then(|f| read_shape(f).ok())
                    .zip(fresh.get("0").and_then(|f| read_shape(f).ok()))
                    .is_some_and(|(a, b)| same_shape(&a, &b))
            {
                return;
            }
            // Keep the other keys of a path frame; anything else becomes one.
            let mut data = match frame.and_then(|f| f.get("2")) {
                Some(d @ E::Dict(_)) if kind == PATH_FRAME => d.clone(),
                _ => E::Dict(vec![("6".into(), E::Array(vec![real(-1.0), real(-1.0)]))]),
            };
            data.set("0", E::Int(PATH_FRAME));
            data.set("6", E::Array(parts.range.iter().map(|v| real(*v)).collect()));
            // The written path is never flipped (see the module docs).
            if let Some(flip) = parts.flip {
                if let Some(path_data @ E::Dict(_)) = data.get_mut("11") {
                    path_data.set("0", E::Bool(flip));
                } else {
                    data.set("11", E::Dict(vec![("0".into(), E::Bool(flip))]));
                }
            }
            // The path is written in text space.
            data.set("2", E::Array([1.0, 0.0, 0.0, 1.0, 0.0, 0.0].map(real).to_vec()));
            let mut frame = match frame {
                Some(f @ E::Dict(_)) if kind == PATH_FRAME => f.clone(),
                _ => E::dict(),
            };
            frame.set("1", E::Dict(vec![("0".into(), E::Array(parts.points.iter().map(|v| real(*v)).collect()))]));
            frame.set("2", data);
            *entry = E::Dict(vec![("0".into(), frame)]);
        }
        // A file's own non-path frame is Photoshop's layout; only a placeholder (the point frame
        // `pad_frames` writes) or a path frame being detached is replaced. An unreadable path
        // frame is left unchanged too: it was not imported as a detached path.
        _ if kind == PATH_FRAME && frame.is_some_and(|f| read_shape(f).is_err()) => {}
        _ if kind != PATH_FRAME && frame.is_some() && *entry != point_frame() => {}
        TextShape::Box { x, y, width, height } => {
            *entry = box_frame_oriented(f64::from(*x), f64::from(*y), f64::from(*width), f64::from(*height), vertical)
        }
        TextShape::Point => *entry = point_frame_oriented(vertical),
        // Area type saves as a text box over its path's bounds, as its EngineData does.
        TextShape::Area { path } => {
            *entry = match crate::area::path_bounds(path) {
                Some((x0, y0, x1, y1)) => box_frame_oriented(x0, y0, x1 - x0, y1 - y0, vertical),
                None => point_frame_oriented(vertical),
            }
        }
    }
}

#[cfg(test)]
mod tests;
