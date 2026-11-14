//! Round trips of type on a path through `Txt2` (`#7962`): the engine reads and
//! writes the path, this crate's save writes the frame list and reopens through
//! the engine again. The pure path math (segment validation, arc measure, range
//! units, flip/start baking) is tested in `photocraft-text-re`'s `psd::path`
//! module, where it lives now.

use super::*;
use crate::engine_data::parse;
use photocraft_geom::Affine;

fn frame(src: &str) -> E {
    parse(src.as_bytes()).unwrap()
}

/// The frame of `text/path-circle.psd` (Photoshop 2026).
const CIRCLE: &str = "<< /1 << /0 [ 32.0 128.0 32.0 74.98067 74.98067 32.0 128.0 32.0 128.0 32.0 181.01933 32.0 224.0 74.98067 224.0 128.0 224.0 128.0 224.0 181.01933 181.01933 224.0 128.0 224.0 128.0 224.0 74.98067 224.0 32.0 181.01933 32.0 128.0 ] >> /2 << /0 2 /2 [ 1.0 0.0 0.0 1.0 -32.0 -32.0 ] /6 [ -1.0 -1.0 ] >> >>";
/// The frame of `text/path-wave-open.psd` (Photoshop 2026).
const WAVE: &str = "<< /1 << /0 [ 16.0 100.0 46.0 40.0 120.0 40.0 160.0 80.0 160.0 80.0 200.0 120.0 274.0 100.0 304.0 60.0 ] >> /2 << /0 2 /2 [ 1.0 0.0 0.0 1.0 -16.0 -52.12246 ] /6 [ -1.0 -1.0 ] >> >>";

#[test]
fn photoshop_path_frames_read_into_text_space() {
    let wave = read_shape(&frame(WAVE)).unwrap();
    let TextShape::Path { path, start, end, flip } = wave else { panic!("{wave:?}") };
    assert!(!path.closed);
    assert_eq!(path.knots.len(), 3);
    assert_eq!((start, end, flip), (0.0, None, false));
    let p = |x: f64, y: f64| Point::new(x - 16.0, y - 52.12246);
    let k = &path.knots;
    assert!(same(k[0].anchor, p(16.0, 100.0)) && same(k[0].in_ctrl, p(16.0, 100.0)) && same(k[0].out_ctrl, p(46.0, 40.0)));
    assert!(same(k[1].in_ctrl, p(120.0, 40.0)) && same(k[1].anchor, p(160.0, 80.0)) && same(k[1].out_ctrl, p(200.0, 120.0)) && k[1].smooth);
    assert!(same(k[2].in_ctrl, p(274.0, 100.0)) && same(k[2].anchor, p(304.0, 60.0)));

    let circle = read_shape(&frame(CIRCLE)).unwrap();
    let TextShape::Path { path, .. } = circle else { panic!("{circle:?}") };
    assert!(path.closed);
    assert_eq!(path.knots.len(), 4);
    // The type starts at the bottom anchor (Photoshop's glyph 0) and runs to the left one.
    assert!(same(path.knots[0].anchor, Point::new(96.0, 192.0)));
    assert!(same(path.knots[1].anchor, Point::new(0.0, 96.0)));
    assert!(path.knots.iter().all(|k| k.smooth));
}

/// Verifies updating path text frames preserves existing PSD frame entries or writes transformed paths.
#[test]
fn written_frames_read_back() {
    for src in [CIRCLE, WAVE] {
        let shape = read_shape(&frame(src)).unwrap();
        let mut entry = E::Dict(vec![("0".into(), frame(src))]);
        let before = entry.clone();
        update_frame(&mut entry, &shape);
        assert_eq!(entry, before, "an unchanged path keeps Photoshop's frame");
        let TextShape::Path { path, .. } = &shape else { panic!("{shape:?}") };
        let mut moved = path.clone();
        for k in &mut moved.knots {
            *k = k.transform(&Affine::translate(5.0, -3.0));
        }
        update_frame(&mut entry, &TextShape::Path { path: moved.clone(), start: 0.0, end: None, flip: false });
        let back = read_shape(entry.get("0").unwrap()).unwrap();
        let TextShape::Path { path: back_path, .. } = &back else { panic!("{back:?}") };
        assert!(same_path(back_path, &moved), "{back:?}");
        assert_eq!(entry.path(&["0", "2", "6"]), before.path(&["0", "2", "6"]), "the frame's other keys stay");
    }
}

/// A layer taken off its path doesn't keep its path frame (it would reopen on the path).
#[test]
fn a_layer_off_its_path_loses_the_path_frame() {
    let mut entry = E::Dict(vec![("0".into(), frame(CIRCLE))]);
    update_frame(&mut entry, &TextShape::Point);
    assert_eq!(entry, point_frame());
    let mut entry = E::Dict(vec![("0".into(), frame(WAVE))]);
    update_frame(&mut entry, &TextShape::Box { x: 1.0, y: 2.0, width: 30.0, height: 40.0 });
    assert_eq!(entry.get("0").map(frame_kind), Some(BOX_FRAME));
    let numbers: Vec<f64> = entry.path(&["0", "1", "0"]).and_then(E::as_array).unwrap()[..8]
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    assert_eq!(
        numbers,
        [1.0, 2.0, 1.0, 2.0, 31.0, 2.0, 31.0, 2.0],
        "Photoshop's box outline: corners as straight segments from the top left"
    );
    // A point or box frame is left alone.
    let mut point = point_frame();
    update_frame(&mut point, &TextShape::Point);
    assert_eq!(point, point_frame());
}

/// A path frame the import could not read (the layer opened as EngineData's box or point
/// text) is saved back as it was, not replaced by a box or point frame.
#[test]
fn an_unreadable_path_frame_is_kept() {
    let two_pieces = "<< /1 << /0 [ 0 0 1 1 2 2 3 3 50 50 60 60 70 70 80 80 ] >> /2 << /0 2 >> >>";
    for shape in [TextShape::Point, TextShape::Box { x: 0.0, y: 0.0, width: 80.0, height: 80.0 }] {
        let mut entry = E::Dict(vec![("0".into(), frame(two_pieces))]);
        let before = entry.clone();
        update_frame(&mut entry, &shape);
        assert_eq!(entry, before, "{shape:?}");
    }
}

/// Area type's frame becomes a text box over its path's bounds, as its EngineData describes it.
#[test]
fn an_area_layer_gets_a_box_frame() {
    let square = photocraft_doc::vector::Path::new(vec![photocraft_doc::vector::Subpath::polygon(&[(10.0, 20.0), (50.0, 20.0), (50.0, 80.0), (10.0, 80.0)])]);
    let mut entry = E::Dict(vec![("0".into(), frame(CIRCLE))]);
    update_frame(&mut entry, &TextShape::Area { path: square });
    assert_eq!(entry, box_frame(10.0, 20.0, 40.0, 60.0));
}

/// An explicit end that reaches the path's end stays explicit: it aligns centered and right type
/// within the span, where no end aligns it around the start.
#[test]
fn an_explicit_end_at_the_path_end_survives_a_round_trip() {
    use crate::warp::path_range_length;
    let wave = read_shape(&frame(WAVE)).unwrap();
    let circle = read_shape(&frame(CIRCLE)).unwrap();
    let (TextShape::Path { path: wave, .. }, TextShape::Path { path: circle, .. }) = (&wave, &circle) else { panic!("{wave:?} {circle:?}") };
    for (path, start, end, flip) in [(wave, 20.0, 100_000.0, false), (wave, 20.0, 100_000.0, true), (circle, 40.0, 40.0 + 100_000.0, false)] {
        let shape = TextShape::Path { path: path.clone(), start, end: Some(end), flip };
        let mut entry = E::dict();
        update_frame(&mut entry, &shape);
        let back = read_shape(entry.get("0").unwrap()).unwrap();
        let TextShape::Path { end: Some(_), .. } = back else { panic!("the explicit end was dropped: {back:?}") };
        let (a, b) = (path_range_length(&shape), path_range_length(&back));
        let (Some(a), Some(b)) = (a, b) else { panic!("range {a:?} vs {b:?}") };
        assert!((a - b).abs() < 0.05, "range {a} vs {b}");
    }
}

/// A closed range whose ends meet is empty, not a lap: it hides all the type and keeps doing so.
#[test]
fn a_collapsed_closed_range_stays_empty() {
    use crate::warp::{Placement, Warp, path_range_length};
    let circle = read_shape(&frame(CIRCLE)).unwrap();
    let TextShape::Path { path, .. } = &circle else { panic!("{circle:?}") };
    for start in [0.0_f32, 75.0] {
        let shape = TextShape::Path { path: path.clone(), start, end: Some(start), flip: false };
        let mut entry = E::dict();
        update_frame(&mut entry, &shape);
        let back = read_shape(entry.get("0").unwrap()).unwrap();
        let TextShape::Path { end: Some(_), .. } = back else { panic!("start {start}: {back:?}") };
        assert_eq!(path_range_length(&back).map(|n| n.round()), Some(0.0), "start {start}: {back:?}");
        let warp = Warp::on_path(&back).unwrap();
        assert!(matches!(warp.place_glyph(10.0), Placement::Hidden), "start {start}: the text is revealed");
    }
}

/// The shape a layer's frame is written as and read back: the engine's canonical
/// form, so the round trips below compare against what the writer emits.
fn baked(shape: &TextShape) -> TextShape {
    let mut entry = E::dict();
    update_frame(&mut entry, shape);
    read_shape(entry.get("0").unwrap()).unwrap()
}

/// #7962: type on a path survives a PSD save and open, for the file's own text object and for
/// a layer the file had no object for; a moved start and flipped type come back laid out the
/// same, in the path; a second save changes nothing.
#[test]
fn path_text_round_trips_through_tysh_and_txt2() {
    let circle = read_shape(&frame(CIRCLE)).unwrap();
    let wave = read_shape(&frame(WAVE)).unwrap();
    let a = layer("On a circle", circle.clone());
    let b = layer("Along a wave", wave.clone());
    let prev = engine_txt2();
    let (txt2, back) = save_and_open(&[&a, &b], &prev);
    for (got, want) in back.iter().zip([&circle, &wave]) {
        let got = got.as_ref().unwrap_or_else(|| panic!("layer did not reopen"));
        let baked = baked(want);
        assert!(same_shape(&got.shape, &baked), "{:?} vs {baked:?}", got.shape);
        assert_eq!(got.transform, Affine::translate(32.0, 20.0), "the frame's matrix is not applied twice");
    }
    let (again, _) = save_and_open(&[back[0].as_ref().unwrap(), back[1].as_ref().unwrap()], &txt2);
    assert_eq!(again, txt2, "a re-save of the opened file changes nothing");
    // Off its path, the layer opens as point text again.
    let detached = layer("On a circle", TextShape::Point);
    let (_, back) = save_and_open(&[&detached], &txt2);
    assert_eq!(back[0].as_ref().map(|l| &l.shape), Some(&TextShape::Point));
}

/// A malformed path frame leaves the layer as EngineData describes it.
#[test]
fn a_malformed_path_frame_keeps_the_engine_data_shape() {
    let a = layer("Hostile", TextShape::Box { x: 0.0, y: 0.0, width: 50.0, height: 20.0 });
    let tysh = crate::psd::set_text_index(&crate::psd::build_tysh(&a, 72.0, None), 0).unwrap();
    for bad in ["<< /1 << /0 [ 1 2 3 ] >> /2 << /0 2 >> >>", "<< /1 << /0 [ 0 0 1 1 2 2 3 3 ] >> /2 << /0 2 /2 [ 0 0 0 0 0 0 ] >> >>"] {
        let entry = E::Dict(vec![("0".into(), frame(bad))]);
        let txt2 = photoshop_txt2_in("Hostile\r", entry);
        let l = crate::psd::text_layer_from_tysh_with_txt2(&tysh, &txt2, 72.0).unwrap_or_else(|| panic!("{bad}: layer"));
        assert!(matches!(l.shape, TextShape::Box { .. }), "{bad}: {:?}", l.shape);
    }
}

/// A moved legacy anchor with no end must not turn into a bounded range on save: centered and
/// right-aligned text would move by half/all of the remaining path length.
#[test]
fn a_legacy_open_path_anchor_keeps_alignment_through_psd() {
    use crate::TextEngine;
    use photocraft_doc::text::{ParagraphRun, ParagraphStyle, TextAlign};

    let path = Subpath::polyline(&[(0.0, 0.0), (100.0, 0.0)]);
    let mut engine = TextEngine::new();
    for align in [TextAlign::Center, TextAlign::Right] {
        for flip in [false, true] {
            let mut text = layer("hello", TextShape::Path { path: path.clone(), start: 20.0, end: None, flip });
            text.paragraphs = vec![ParagraphRun { len: text.text.len(), style: ParagraphStyle { align, ..Default::default() } }];
            text.sync_summary();
            let before = engine.layout(&text, 72.0);
            let (txt2, back) = save_and_open(&[&text], &engine_txt2());
            let reopened = back[0].as_ref().unwrap_or_else(|| panic!("{align:?} flip {flip}: layer"));
            let after = engine.layout(reopened, 72.0);
            assert!((before.lines[0].x0 - after.lines[0].x0).abs() < 0.01, "{align:?} flip {flip}: before {:?}, after {:?}", before.lines[0], after.lines[0]);
            assert!(matches!(reopened.shape, TextShape::Path { end: None, .. }), "{align:?} flip {flip}: {:?}", reopened.shape);
            let (again, _) = save_and_open(&[reopened], &txt2);
            assert_eq!(again, txt2, "{align:?} flip {flip}: saving twice changes the frame");
        }
    }
}

fn d(items: Vec<(&str, E)>) -> E {
    E::Dict(items.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// A `Txt2` block as the engine's writer produces it: its object/frame/paragraph
/// structure is exactly what [`crate::psd::build_txt2`] rewrites.
fn engine_txt2() -> Vec<u8> {
    photocraft_text_re::psd::build_txt2(&[], None).expect("scaffold")
}

/// [`photoshop_txt2`] with the frame list entry `frame`.
fn photoshop_txt2_in(text: &str, frame: E) -> Vec<u8> {
    photoshop_txt2_of(vec![photoshop_object(text, 0)], vec![frame])
}

/// A Photoshop-shaped `Txt2` block holding `objects` and the frame list `frames`.
fn photoshop_txt2_of(objects: Vec<E>, frames: Vec<E>) -> Vec<u8> {
    crate::engine_data::write_bare(&[
        ("98".into(), d(vec![("0", E::Int(14))])),
        ("0".into(), d(vec![("8", d(vec![("0", E::Array(frames))]))])),
        ("1".into(), d(vec![("1", E::Array(objects))])),
    ])
}

/// A Photoshop-shaped text object for `text` drawn in frame `at`.
fn photoshop_object(text: &str, at: i64) -> E {
    let len = text.encode_utf16().count() as i64;
    let run =
        || d(vec![("0", d(vec![("0", d(vec![("0", E::String(String::new())), ("6", d(vec![("0", E::Int(0)), ("1", E::Real(12.0))]))]))])), ("1", E::Int(len))]);
    let model = d(vec![("0", E::String(text.into())), ("5", d(vec![("0", E::Array(vec![run()]))])), ("6", d(vec![("0", E::Array(vec![run()]))]))]);
    let strike = d(vec![("99", E::Name("PC".into())), ("6", E::Array(vec![d(vec![("99", E::Name("F".into())), ("10", E::Int(at))])]))]);
    let view = d(vec![
        ("0", E::Array(vec![d(vec![("0", E::Int(at))])])),
        ("1", d(vec![("0", E::Array(vec![d(vec![("0", d(vec![("1", E::Int(1))])), ("1", E::Int(len))])]))])),
        ("2", E::Array(vec![strike])),
    ]);
    d(vec![("0", model), ("1", view)])
}

fn layer(text: &str, shape: TextShape) -> photocraft_doc::TextLayer {
    use photocraft_doc::text::{CharStyle, TextRun};
    let mut t = photocraft_doc::TextLayer {
        text: text.into(),
        runs: vec![TextRun { len: text.len(), style: CharStyle { size_pt: 12.0, ..Default::default() } }],
        shape,
        transform: Affine::translate(32.0, 20.0),
        ..Default::default()
    };
    t.sync_summary();
    t
}

/// A layer opened from the saved `Txt2` (see [`crate::psd::text_layer_from_tysh_with_txt2`]).
type Opened = Option<photocraft_doc::TextLayer>;

/// Saves `layers` (object `i` each) into a file whose `Txt2` is `prev`, then reads each back:
/// its `TySh` and the `Txt2` block through the engine's reader.
fn save_and_open(layers: &[&photocraft_doc::TextLayer], prev: &[u8]) -> (Vec<u8>, Vec<Opened>) {
    let objects: Vec<(i32, &photocraft_doc::TextLayer)> = layers.iter().enumerate().map(|(i, l)| (i as i32, *l)).collect();
    let txt2 = crate::psd::build_txt2(&objects, Some(prev));
    let back = objects
        .iter()
        .map(|(i, l)| {
            let tysh = crate::psd::set_text_index(&crate::psd::build_tysh(l, 72.0, None), *i).unwrap();
            crate::psd::text_layer_from_tysh_with_txt2(&tysh, &txt2, 72.0)
        })
        .collect();
    (txt2, back)
}
