use super::history::fix_selection;
use super::{Arc, EngineError, LayerId, Result, Session, channel_cmds, layer_multi_cmds, type_cmds};
use crate::TypeLayerRef;

impl Session {
    /// Replace the active document's active layer without creating a history step.
    pub fn select_layer(&mut self, id: LayerId) -> Result<()> {
        let before = self.active_layer_stamp();
        let st = self.active_mut().ok_or(EngineError::NoDocument)?;
        if st.doc.layer(id).is_none() {
            return Err(EngineError::NoLayer(id));
        }
        st.active_layer = Some(id);
        st.selected_layers = vec![id];
        st.layer_anchor = Some(id);
        channel_cmds::fix_view(st);
        // Selecting a layer is not an edit: keep a clean document clean.
        let clean = st.saved_revision == st.revision;
        st.revision += 1;
        if clean {
            st.saved_revision = st.revision;
        }
        st.last_damage = Some(photocraft_geom::Rect::EMPTY);
        self.note_layer_change(before);
        Ok(())
    }

    /// Put back a layer selection (active layer, selected layers, anchor) that a step moved, such
    /// as the undo ending an empty Type Mask, which restores the selection from before it. `None`
    /// is no layer selected. Selected layers that are gone are dropped; an active layer that is
    /// gone is an error and changes nothing. Like [`Self::select_layer`], not an edit.
    pub fn restore_layer_selection(&mut self, active: Option<LayerId>, selected: &[LayerId], anchor: Option<LayerId>) -> Result<()> {
        let before = self.active_layer_stamp();
        let st = self.active_mut().ok_or(EngineError::NoDocument)?;
        if let Some(id) = active
            && st.doc.layer(id).is_none()
        {
            return Err(EngineError::NoLayer(id));
        }
        st.active_layer = active;
        st.selected_layers = selected.to_vec();
        st.layer_anchor = anchor.filter(|_| active.is_some());
        fix_selection(st);
        channel_cmds::fix_view(st);
        let clean = st.saved_revision == st.revision;
        st.revision += 1;
        if clean {
            st.saved_revision = st.revision;
        }
        st.last_damage = Some(photocraft_geom::Rect::EMPTY);
        self.note_layer_change(before);
        Ok(())
    }

    /// Re-renders the pixels of type layers whose font arrived after they were drawn (the web
    /// build fetches served fonts on demand, `photocraft_text::served`). The layers' model is
    /// unchanged, so this is no history step and leaves a clean document clean. Unknown documents
    /// and layers are skipped. A document a background job is computing from must not move under
    /// it: its layers are returned, to try again later.
    pub fn refresh_type_layers(&mut self, layers: &[TypeLayerRef]) -> Vec<TypeLayerRef> {
        let mut busy = Vec::new();
        let mut docs: Vec<photocraft_doc::DocId> = Vec::new();
        for l in layers {
            if !docs.contains(&l.doc) {
                docs.push(l.doc);
            }
        }
        for doc_id in docs {
            if self.job_on(doc_id).is_some() {
                busy.extend(layers.iter().filter(|l| l.doc == doc_id));
                continue;
            }
            let ids = |first: bool| -> Vec<LayerId> { layers.iter().filter(|l| l.doc == doc_id && l.first_snapshot == first).map(|l| l.layer).collect() };
            let Some(st) = self.docs.iter_mut().find(|st| st.doc.id == doc_id) else { continue };
            // The first snapshot is the document as opened (#2966, #6087): it was taken before the
            // font arrived, so its type layers get the same refresh, or returning to it brings the
            // fallback-font pixels back. While it still is the open document, it shares the
            // refreshed one; once edited, it is refreshed on its own, layers the edit removed too.
            let first_is_doc = st.snapshots.iter().any(|s| s.id == 0 && Arc::ptr_eq(&s.doc, &st.doc));
            if !first_is_doc
                && let Some(first) = st.snapshots.iter_mut().find(|s| s.id == 0)
                && let Some(doc) = refresh_text_layers(&first.doc, &ids(true))
            {
                first.doc = Arc::new(doc);
            }
            // While shared, a layer picked in the snapshot is a layer of the open document too
            // (a document busy when its font arrived can be back at the snapshot by the retry).
            let mut open = ids(false);
            if first_is_doc {
                for id in ids(true) {
                    if !open.contains(&id) {
                        open.push(id);
                    }
                }
            }
            let Some(doc) = refresh_text_layers(&st.doc, &open) else { continue };
            let doc = Arc::new(doc);
            if first_is_doc && let Some(first) = st.snapshots.iter_mut().find(|s| s.id == 0) {
                first.doc = doc.clone();
            }
            let clean = st.saved_revision == st.revision;
            let before = std::mem::replace(&mut st.doc, doc);
            st.revision += 1;
            if clean {
                st.saved_revision = st.revision;
            }
            // Only the refreshed layers' caches changed: their changed tiles.
            st.last_damage = layer_multi_cmds::step_damage(&before, &st.doc);
        }
        busy
    }
}

/// `doc` with its type layers among `ids` re-rendered (`Session::refresh_type_layers`), or `None`
/// when none of them is a type layer of `doc`.
fn refresh_text_layers(doc: &Arc<photocraft_doc::Document>, ids: &[LayerId]) -> Option<photocraft_doc::Document> {
    let mut out = (**doc).clone();
    let mut changed = false;
    for id in ids {
        if let Some(photocraft_doc::Layer { content: photocraft_doc::LayerContent::Text(t), .. }) = out.layer_mut(*id) {
            type_cmds::refresh(doc, t);
            changed = true;
        }
    }
    changed.then_some(out)
}

#[cfg(test)]
mod refresh_damage_tests {
    use super::*;
    use crate::TypeLayerRef;
    use photocraft_color::{ColorMode, SampleType};
    use photocraft_doc::text::{CharStyle, TextRun};
    use photocraft_doc::{Document, Layer, LayerContent, TextLayer};
    use photocraft_geom::{Affine, Rect, Size};

    /// Re-rendering a type layer's cache (a font that arrived after the open, a type edit) must
    /// repaint the union of where the old and the new glyphs are, or the old ones stay on screen
    /// beside the new ones (ghosting). The refresh keeps the file's `TySh` data, so the step can
    /// differ only in the cache: the narrower path.
    #[test]
    fn a_refreshed_type_cache_damages_the_union_of_old_and_new_pixels() {
        let mut doc = Document::with_background("t", Size::new(320, 200), ColorMode::Rgb, SampleType::U8, photocraft_doc::Color::WHITE);
        let mut text = TextLayer {
            text: "Wg".into(),
            runs: vec![TextRun { len: 2, style: CharStyle { font_family: "Inter".into(), size_pt: 48.0, ..Default::default() } }],
            transform: Affine::translate(30.0, 140.0),
            ..Default::default()
        };
        text.sync_summary();
        crate::cmd::type_cmds::refresh_at(doc.resolution_dpi, doc.pixel_format(), &mut text);
        let fresh = text.cache.as_ref().expect("rendered").content_bounds();
        assert!(!fresh.is_empty());
        // What a PSD used to open with: Photoshop's pixels, here somewhere else entirely.
        let mut stale = photocraft_raster::Surface::new(doc.pixel_format());
        let stale_rect = Rect::new(5, 5, 100, 60);
        stale.fill_rect(stale_rect, &[0.0, 0.0, 0.0, 1.0]);
        text.cache = Some(stale);
        let raw = text.psd_raw.clone();
        let layer = Layer::new("type", LayerContent::Text(text.clone()));
        let id = layer.id;
        doc.layers.push(layer);

        let mut s = Session::new();
        s.add_document(doc, None);
        let doc_id = s.active().unwrap().doc.id;
        s.refresh_type_layers(&[TypeLayerRef { doc: doc_id, layer: id, first_snapshot: false }]);

        let st = s.active().unwrap();
        let LayerContent::Text(t) = &st.doc.layer(id).unwrap().content else { panic!("not a type layer") };
        assert_eq!(t.cache.as_ref().map(|c| c.content_bounds()), Some(fresh), "the cache is the model's render again");
        assert!(t.psd_raw.as_ref().zip(raw.as_ref()).is_some_and(|(a, b)| a.as_slice() == b.as_slice()), "the TySh data is unchanged");
        let want = stale_rect.union(&fresh);
        let damage = st.last_damage.expect("a bounded damage");
        assert_eq!(damage.intersect(&want), want, "damage {damage:?} must repaint the old {stale_rect:?} and new {fresh:?} glyphs");
    }

    /// Entering text edit on a type layer whose cache is not the model's render (an imported
    /// raster, a font change) replaces it through `type.edit`, the command the Type tool runs
    /// first. The canvas must repaint the union of the old and the new glyphs and the layer must
    /// end with the model's render alone: anything else leaves the old text visible under the
    /// new one (the user's duplicate-and-edit ghost).
    #[test]
    fn entering_text_edit_on_a_stale_cache_draws_the_model_only() {
        let mut doc = Document::with_background("t", Size::new(320, 200), ColorMode::Rgb, SampleType::U8, photocraft_doc::Color::WHITE);
        let mut text = TextLayer {
            text: "Wg".into(),
            runs: vec![TextRun { len: 2, style: CharStyle { font_family: "Inter".into(), size_pt: 48.0, ..Default::default() } }],
            transform: Affine::translate(30.0, 140.0),
            ..Default::default()
        };
        text.sync_summary();
        crate::cmd::type_cmds::refresh_at(doc.resolution_dpi, doc.pixel_format(), &mut text);
        let fresh = text.cache.as_ref().expect("rendered").content_bounds();
        let mut stale = photocraft_raster::Surface::new(doc.pixel_format());
        let stale_rect = Rect::new(5, 5, 100, 60);
        stale.fill_rect(stale_rect, &[0.0, 0.0, 0.0, 1.0]);
        text.cache = Some(stale);
        let layer = Layer::new("type", LayerContent::Text(text));
        let id = layer.id;
        doc.layers.push(layer);
        let mut s = Session::new();
        s.add_document(doc, None);
        s.select_layer(id).unwrap();
        s.execute("type.edit", serde_json::json!({ "layer": id })).expect("entering edit re-renders the layer");

        let st = s.active().unwrap();
        let LayerContent::Text(t) = &st.doc.layer(id).unwrap().content else { panic!("not a type layer") };
        assert_eq!(t.cache.as_ref().map(|c| c.content_bounds()), Some(fresh), "the layer must show the model's render only");
        let old = t.cache.as_ref().unwrap().pixel(stale_rect.x0, stale_rect.y0);
        assert_eq!(old[3], 0.0, "the stale pixel {old:?} is gone");
        let want = stale_rect.union(&fresh);
        let damage = st.last_damage.expect("a bounded damage");
        assert_eq!(damage.intersect(&want), want, "damage {damage:?} must repaint the old {stale_rect:?} and new {fresh:?} glyphs");
    }
}
