//! Child frame composition.
//!
//! Owned by `ax-engine`. The root document's snapshot lists everything in
//! its top-level frame; for sites that embed iframes we walk
//! `Page.getFrameTree`, attach to each child frame target with
//! `Target.attachToTarget { flatten: true }`, run a per-frame [`merge`],
//! and splice the per-frame element list into the parent.
//!
//! Each frame's `Element.frame_id` is set to the frame id reported by
//! Chromium so callers can disambiguate same-named elements across frames.
//! `stable_id` is *also* re-derived inside the child frame's tree (the
//! sibling counter is per-frame), but at the snapshot layer we additionally
//! mix the frame id into the stored hash to keep cross-frame uniqueness.

use crate::index::StableId;
use crate::merge::Snapshot;
use sha2::{Digest, Sha256};

/// Splice a child frame's snapshot into the parent at the end. Renumbers
/// `index`/`ref` to stay dense, prefixes the child elements' frame_id, and
/// remixes their stable_id with the frame id so they don't collide with
/// any same-shape element in the parent frame.
pub fn splice(parent: &mut Snapshot, child: Snapshot, frame_id: &str) {
    let base = parent.elements.len() as u32;
    for (i, mut el) in child.elements.into_iter().enumerate() {
        el.index = base + i as u32;
        el.r#ref = format!("e{}", el.index);
        el.frame_id = frame_id.to_string();
        el.stable_id = remix_with_frame(el.stable_id, frame_id);
        parent.elements.push(el);
    }
}

fn remix_with_frame(id: StableId, frame_id: &str) -> StableId {
    let mut h = Sha256::new();
    h.update(id.0);
    h.update([0x1F]);
    h.update(frame_id.as_bytes());
    let out = h.finalize();
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&out);
    StableId(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::{Element, ElementState, Snapshot};

    fn fake_element(idx: u32, role: &str, name: &str) -> Element {
        Element {
            index: idx,
            r#ref: format!("e{idx}"),
            role: role.into(),
            name: name.into(),
            value: None,
            description: None,
            state: ElementState::default(),
            bbox: None,
            interactable: false,
            frame_id: String::new(),
            stable_id: StableId::compute(role, name, "", 0),
            backend_node_id: idx as i64 + 100,
        }
    }

    #[test]
    fn splice_renumbers_and_assigns_frame_id() {
        let mut parent = Snapshot {
            elements: vec![fake_element(0, "button", "A")],
            ..Snapshot::default()
        };
        let child = Snapshot {
            elements: vec![fake_element(0, "button", "B"), fake_element(1, "link", "X")],
            ..Snapshot::default()
        };
        splice(&mut parent, child, "FRAME-1");
        assert_eq!(parent.elements.len(), 3);
        assert_eq!(parent.elements[0].index, 0);
        assert_eq!(parent.elements[1].index, 1);
        assert_eq!(parent.elements[2].index, 2);
        assert_eq!(parent.elements[1].r#ref, "e1");
        assert_eq!(parent.elements[1].frame_id, "FRAME-1");
        assert_eq!(parent.elements[2].frame_id, "FRAME-1");
    }

    #[test]
    fn frame_remix_makes_cross_frame_collisions_separable() {
        let same_a = StableId::compute("button", "OK", "form", 0);
        let same_b = StableId::compute("button", "OK", "form", 0);
        assert_eq!(same_a, same_b);
        let a = remix_with_frame(same_a, "FRAME-A");
        let b = remix_with_frame(same_b, "FRAME-B");
        assert_ne!(a, b);
    }
}
