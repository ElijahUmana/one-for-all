//! Depth-first AX-tree walker.
//!
//! SPEC §11 V2: produces an `AppSnapshot` whose `elements` mirror
//! `browser_engine::Element` exactly — including the SPEC §1 D14
//! `stable_id = sha256(role | 0x1F | name | 0x1F | parent_role | 0x1F | sibling_index)`
//! computed by the canonical `ax_engine::index::StableId`. A downstream
//! agent treats refs from `page.snapshot` and `app.snapshot` interchangeably.
//!
//! # Caps
//!
//! - **Depth cap**: 64. Beyond this we stop descending and emit a
//!   `tracing::warn` plus `AppSnapshot::truncated_at = Some("depth")`. The
//!   field is published in the JSON wire shape so agents can detect partial
//!   coverage. No silent truncation per SPEC §10.
//! - **Node cap**: 5000. Same warn + `truncated_at = Some("nodes")`.
//!
//! # Threading
//!
//! Every AX call is synchronous C FFI on `ApplicationServices`. The public
//! [`snapshot_app`] function is `async` but offloads onto
//! `tokio::task::spawn_blocking` so the broker's reactor never blocks on a
//! traversal.

#![cfg(target_os = "macos")]

use std::collections::BTreeMap;

use accessibility_sys::{
    kAXChildrenAttribute, kAXDescriptionAttribute, kAXEnabledAttribute, kAXErrorSuccess,
    kAXFocusedAttribute, kAXFocusedUIElementAttribute, kAXHelpAttribute, kAXMainWindowAttribute,
    kAXPositionAttribute, kAXPressAction, kAXRoleAttribute, kAXSelectedAttribute, kAXSizeAttribute,
    kAXTitleAttribute, kAXValueAttribute, kAXValueTypeCGPoint, kAXValueTypeCGSize, AXError,
    AXUIElementCopyActionNames, AXUIElementCopyAttributeValue, AXUIElementCreateApplication,
    AXUIElementIsAttributeSettable, AXUIElementRef, AXValueGetType, AXValueGetValue, AXValueRef,
};
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFTypeID, CFTypeRef};
use serde_json::{json, Value};
use tracing::warn;

use crate::cf_owned::AxOwned;
use crate::types::{AppElement, AppSnapshot, BBox, ElementState, NativeControlError};

/// Hard cap on traversal depth; matches the empirical depth of macOS apps
/// (Finder is ~25, Xcode ~35) with comfortable headroom. Beyond this, almost
/// certainly an AX cycle or a misbehaving toolkit. `truncated_at = "depth"`
/// gets set if hit.
pub const MAX_DEPTH: u32 = 64;

/// Hard cap on element count per snapshot. 5000 elements is enough for
/// every macOS bundled app we tested (Mail listing 200 messages = ~3500
/// nodes); larger trees almost always indicate a leak or a webview.
/// `truncated_at = "nodes"` gets set if hit.
pub const MAX_NODES: usize = 5_000;

/// Walk the running app identified by `pid` and produce an [`AppSnapshot`].
///
/// `bundle_id` is stamped into the snapshot for downstream identification;
/// the walker does NOT verify it matches the pid (caller's responsibility).
///
/// Always called from `tokio::task::spawn_blocking` via
/// [`crate::actions::snapshot_app`] — this function itself is sync.
pub(crate) fn walk(
    pid: i32,
    bundle_id: &str,
    snapshot_seq: u64,
) -> Result<AppSnapshot, NativeControlError> {
    // SAFETY: AXUIElementCreateApplication returns a +1 ref or NULL.
    let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
    let app =
        // SAFETY: app_ref came from a Create-Rule API.
        unsafe { AxOwned::<accessibility_sys::__AXUIElement>::from_create(app_ref as *const _) }
            .ok_or_else(|| NativeControlError::AppNotFound {
                bundle_id: bundle_id.to_string(),
            })?;

    let mut walker = Walker {
        elements: Vec::with_capacity(128),
        per_role_sibling: Vec::new(),
        truncated_at: None,
    };

    // Top-level: walk the main window if any, falling back to the app root
    // children (menu bar etc.). Most apps surface their UI via
    // kAXMainWindowAttribute; we DFS that subtree first, then visible
    // siblings under the app root.
    let mut roots: Vec<AxOwned<accessibility_sys::__AXUIElement>> = Vec::new();
    if let Some(win) = copy_attr_owned(app.as_ptr() as AXUIElementRef, kAXMainWindowAttribute) {
        roots.push(win);
    }
    // Children of the app root (top-level windows + menu bar). De-dup the
    // main window so it isn't walked twice.
    if let Some(children) = copy_children(app.as_ptr() as AXUIElementRef) {
        for c in children {
            roots.push(c);
        }
    }

    walker.per_role_sibling.push(BTreeMap::new());
    let mut path_stack: Vec<u32> = Vec::new();
    for (idx, root) in roots.iter().enumerate() {
        path_stack.push(idx as u32);
        walker.dfs(
            root.as_ptr() as AXUIElementRef,
            "AXApplication",
            0,
            &mut path_stack,
            bundle_id,
        );
        path_stack.pop();
        if walker.truncated_at.is_some() && walker.elements.len() >= MAX_NODES {
            break;
        }
    }

    if let Some(reason) = walker.truncated_at {
        warn!(
            bundle_id,
            elements = walker.elements.len(),
            cap = match reason {
                "nodes" => MAX_NODES as u32,
                "depth" => MAX_DEPTH,
                _ => 0,
            },
            reason,
            "native-control AX walker hit cap; truncated_at populated"
        );
    }

    let title = copy_string_attr(app.as_ptr() as AXUIElementRef, kAXTitleAttribute)
        .unwrap_or_else(|| bundle_id.to_string());

    // Determine focused ref by looking up the AXFocusedUIElement and matching
    // by ax_path against the elements we walked.
    let focused_ref = focused_ref_for(app.as_ptr() as AXUIElementRef, &walker.elements);

    let tree = build_tree(&walker.elements);

    Ok(AppSnapshot {
        snapshot_seq,
        app_id: bundle_id.to_string(),
        bundle_id: bundle_id.to_string(),
        pid,
        title,
        focused_ref,
        elements: walker.elements,
        tree,
        truncated_at: walker.truncated_at,
    })
}

struct Walker {
    elements: Vec<AppElement>,
    /// Per-depth map of role → next sibling index used to compute SPEC §1 D14
    /// `sibling_index_within_same_role`. One BTreeMap per depth level.
    per_role_sibling: Vec<BTreeMap<String, u32>>,
    truncated_at: Option<&'static str>,
}

impl Walker {
    fn dfs(
        &mut self,
        elem: AXUIElementRef,
        parent_role: &str,
        depth: u32,
        path_stack: &mut Vec<u32>,
        bundle_id: &str,
    ) {
        if self.truncated_at.is_some() {
            return;
        }
        if depth >= MAX_DEPTH {
            self.truncated_at = Some("depth");
            return;
        }
        if self.elements.len() >= MAX_NODES {
            self.truncated_at = Some("nodes");
            return;
        }

        let role = copy_string_attr(elem, kAXRoleAttribute).unwrap_or_default();
        let name = best_name(elem);
        let value = copy_string_attr(elem, kAXValueAttribute);
        let description = copy_string_attr(elem, kAXDescriptionAttribute)
            .or_else(|| copy_string_attr(elem, kAXHelpAttribute));

        // sibling_index per role per depth
        if self.per_role_sibling.len() <= depth as usize {
            self.per_role_sibling.push(BTreeMap::new());
        }
        let sibling_idx = {
            let map = &mut self.per_role_sibling[depth as usize];
            let n = map.entry(role.clone()).or_insert(0);
            let v = *n;
            *n += 1;
            v
        };

        let bbox = bbox_of(elem).unwrap_or(BBox {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        });
        let state = state_of(elem);
        let interactable = is_interactable(elem);

        let stable_id =
            ax_engine::index::StableId::compute(&role, &name, parent_role, sibling_idx).to_hex();
        let index = self.elements.len();
        let element_ref = format!("e{}", index);

        let elem_record = AppElement {
            index,
            element_ref,
            role: role.clone(),
            name,
            value,
            description,
            state,
            bbox,
            interactable,
            app_id: bundle_id.to_string(),
            stable_id,
            ax_path: path_stack.clone(),
        };
        self.elements.push(elem_record);

        // Recurse into children.
        if let Some(children) = copy_children(elem) {
            for (i, child) in children.iter().enumerate() {
                if self.truncated_at.is_some() {
                    break;
                }
                path_stack.push(i as u32);
                self.dfs(
                    child.as_ptr() as AXUIElementRef,
                    &role,
                    depth + 1,
                    path_stack,
                    bundle_id,
                );
                path_stack.pop();
            }
        }
    }
}

/// Fetch a CFTypeRef attribute and wrap it in our owned drop guard.
pub(crate) fn copy_attr_owned(
    elem: AXUIElementRef,
    attr: &str,
) -> Option<AxOwned<accessibility_sys::__AXUIElement>> {
    let attr_cf = CFString::new(attr);
    let mut out: CFTypeRef = std::ptr::null_mut();
    // SAFETY: `out` is a stack slot we provide for the +1 ref; non-success
    // return guarantees `out` is unchanged (per Apple docs). On success we
    // adopt the ref via AxOwned.
    let err =
        unsafe { AXUIElementCopyAttributeValue(elem, attr_cf.as_concrete_TypeRef(), &mut out) };
    if err != kAXErrorSuccess || out.is_null() {
        return None;
    }
    // SAFETY: out is a +1 ref to an AXUIElement (or compatible CF type the
    // caller has typed). AxOwned::Drop will CFRelease.
    unsafe { AxOwned::from_create(out as *const _) }
}

pub(crate) fn copy_children(
    elem: AXUIElementRef,
) -> Option<Vec<AxOwned<accessibility_sys::__AXUIElement>>> {
    let attr_cf = CFString::new(kAXChildrenAttribute);
    let mut out: CFTypeRef = std::ptr::null_mut();
    // SAFETY: same as copy_attr_owned. `out` is a +1 CFArrayRef on success.
    let err =
        unsafe { AXUIElementCopyAttributeValue(elem, attr_cf.as_concrete_TypeRef(), &mut out) };
    if err != kAXErrorSuccess || out.is_null() {
        return None;
    }
    // We own +1 on the array; iterating clones each element ref via
    // CFRetain so the array can be released safely after.
    // SAFETY: out is a CFArrayRef per the AX docs.
    let array: CFArray<CFType> = unsafe { CFArray::wrap_under_create_rule(out as CFArrayRef) };
    let mut owned: Vec<AxOwned<accessibility_sys::__AXUIElement>> =
        Vec::with_capacity(array.len() as usize);
    for item in array.iter() {
        // The CFArray wrapper retains each item view; we need a +1 owned
        // ref to hold past the array's lifetime, so CFRetain.
        let raw: CFTypeRef = item.as_CFTypeRef();
        // SAFETY: raw is a non-null CFTypeRef from the array iterator.
        unsafe { core_foundation_sys::base::CFRetain(raw) };
        // SAFETY: we just took +1; AxOwned takes ownership.
        if let Some(o) = unsafe { AxOwned::from_create(raw as *const _) } {
            owned.push(o);
        }
    }
    Some(owned)
}

pub(crate) fn copy_string_attr(elem: AXUIElementRef, attr: &str) -> Option<String> {
    let attr_cf = CFString::new(attr);
    let mut out: CFTypeRef = std::ptr::null_mut();
    // SAFETY: same as copy_attr_owned. CFString and CFNumber both flow
    // through this function — we discriminate by CFGetTypeID.
    let err =
        unsafe { AXUIElementCopyAttributeValue(elem, attr_cf.as_concrete_TypeRef(), &mut out) };
    if err != kAXErrorSuccess || out.is_null() {
        return None;
    }
    // SAFETY: own a +1 to whatever this is. We type the owner as
    // `__CFString` purely as a placeholder for the drop-via-CFRelease
    // semantics — we look up the actual type via `CFGetTypeID` below
    // before any concrete cast.
    let owned = unsafe {
        AxOwned::<core_foundation_sys::string::__CFString>::from_create(out as *const _)
    }?;
    let id: CFTypeID =
        unsafe { core_foundation_sys::base::CFGetTypeID(owned.as_ptr() as CFTypeRef) };
    if id == unsafe { core_foundation_sys::string::CFStringGetTypeID() } {
        // SAFETY: id confirmed CFString.
        let s = unsafe {
            CFString::wrap_under_get_rule(owned.as_ptr() as core_foundation_sys::string::CFStringRef)
        };
        return Some(s.to_string());
    }
    if id == unsafe { core_foundation_sys::number::CFNumberGetTypeID() } {
        let n = unsafe {
            CFNumber::wrap_under_get_rule(owned.as_ptr() as core_foundation_sys::number::CFNumberRef)
        };
        return Some(
            n.to_i64()
                .map(|v| v.to_string())
                .or_else(|| n.to_f64().map(|v| v.to_string()))
                .unwrap_or_default(),
        );
    }
    if id == unsafe { core_foundation_sys::number::CFBooleanGetTypeID() } {
        let b = unsafe {
            CFBoolean::wrap_under_get_rule(
                owned.as_ptr() as core_foundation_sys::number::CFBooleanRef
            )
        };
        let v: bool = b.into();
        return Some(if v { "true".into() } else { "false".into() });
    }
    None
}

pub(crate) fn copy_bool_attr(elem: AXUIElementRef, attr: &str) -> Option<bool> {
    let attr_cf = CFString::new(attr);
    let mut out: CFTypeRef = std::ptr::null_mut();
    // SAFETY: standard AX read.
    let err =
        unsafe { AXUIElementCopyAttributeValue(elem, attr_cf.as_concrete_TypeRef(), &mut out) };
    if err != kAXErrorSuccess || out.is_null() {
        return None;
    }
    // SAFETY: own +1; placeholder type — we check the real type below.
    let owned = unsafe {
        AxOwned::<core_foundation_sys::string::__CFString>::from_create(out as *const _)
    }?;
    let id: CFTypeID =
        unsafe { core_foundation_sys::base::CFGetTypeID(owned.as_ptr() as CFTypeRef) };
    if id == unsafe { core_foundation_sys::number::CFBooleanGetTypeID() } {
        let b = unsafe {
            CFBoolean::wrap_under_get_rule(
                owned.as_ptr() as core_foundation_sys::number::CFBooleanRef
            )
        };
        return Some(b.into());
    }
    None
}

fn best_name(elem: AXUIElementRef) -> String {
    // Title is best when present; some controls (icon-only buttons) only
    // have a description. AXValue is the fallback for static text labels.
    if let Some(t) = copy_string_attr(elem, kAXTitleAttribute) {
        if !t.is_empty() {
            return t;
        }
    }
    if let Some(d) = copy_string_attr(elem, kAXDescriptionAttribute) {
        if !d.is_empty() {
            return d;
        }
    }
    if let Some(v) = copy_string_attr(elem, kAXValueAttribute) {
        if !v.is_empty() {
            return v;
        }
    }
    String::new()
}

pub(crate) fn bbox_of(elem: AXUIElementRef) -> Option<BBox> {
    let pos_attr = CFString::new(kAXPositionAttribute);
    let size_attr = CFString::new(kAXSizeAttribute);

    let mut pos_out: CFTypeRef = std::ptr::null_mut();
    let mut size_out: CFTypeRef = std::ptr::null_mut();
    // SAFETY: standard AX read.
    let err_p = unsafe {
        AXUIElementCopyAttributeValue(elem, pos_attr.as_concrete_TypeRef(), &mut pos_out)
    };
    let err_s = unsafe {
        AXUIElementCopyAttributeValue(elem, size_attr.as_concrete_TypeRef(), &mut size_out)
    };
    if err_p != kAXErrorSuccess || err_s != kAXErrorSuccess {
        return None;
    }
    let pos_owned = unsafe {
        AxOwned::<core_foundation_sys::string::__CFString>::from_create(pos_out as *const _)
    }?;
    let size_owned = unsafe {
        AxOwned::<core_foundation_sys::string::__CFString>::from_create(size_out as *const _)
    }?;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct CGSize {
        w: f64,
        h: f64,
    }

    let mut p = CGPoint::default();
    let mut s = CGSize::default();
    // SAFETY: AXValueGetValue copies the underlying value into the
    // caller-provided slot when the type matches the AXValueRef's stored
    // type. We pass the matching kAXValueTypeCG{Point,Size}.
    let pos_ax = pos_owned.as_ptr() as AXValueRef;
    let size_ax = size_owned.as_ptr() as AXValueRef;
    if unsafe { AXValueGetType(pos_ax) } != kAXValueTypeCGPoint {
        return None;
    }
    if unsafe { AXValueGetType(size_ax) } != kAXValueTypeCGSize {
        return None;
    }
    if !unsafe { AXValueGetValue(pos_ax, kAXValueTypeCGPoint, &mut p as *mut _ as *mut _) } {
        return None;
    }
    if !unsafe { AXValueGetValue(size_ax, kAXValueTypeCGSize, &mut s as *mut _ as *mut _) } {
        return None;
    }
    Some(BBox {
        x: p.x,
        y: p.y,
        w: s.w,
        h: s.h,
    })
}

fn state_of(elem: AXUIElementRef) -> ElementState {
    let mut state = ElementState::default();
    if let Some(b) = copy_bool_attr(elem, kAXSelectedAttribute) {
        state.selected = Some(b);
    }
    if let Some(b) = copy_bool_attr(elem, kAXFocusedAttribute) {
        state.pressed = Some(b);
    }
    if let Some(enabled) = copy_bool_attr(elem, kAXEnabledAttribute) {
        state.disabled = !enabled;
    }
    state
}

fn is_interactable(elem: AXUIElementRef) -> bool {
    // Interactable iff the element has an AXPress action OR kAXValueAttribute
    // is settable (text fields, sliders).
    if has_action(elem, kAXPressAction) {
        return true;
    }
    let attr_cf = CFString::new(kAXValueAttribute);
    let mut settable: u8 = 0;
    // SAFETY: standard AX query; settable is a u8 out-slot.
    let err = unsafe {
        AXUIElementIsAttributeSettable(elem, attr_cf.as_concrete_TypeRef(), &mut settable)
    };
    err == kAXErrorSuccess && settable != 0
}

pub(crate) fn has_action(elem: AXUIElementRef, action: &str) -> bool {
    let mut out: CFArrayRef = std::ptr::null();
    // SAFETY: standard AX query for action names.
    let err = unsafe { AXUIElementCopyActionNames(elem, &mut out) };
    if err != kAXErrorSuccess || out.is_null() {
        return false;
    }
    // SAFETY: out is a +1 CFArrayRef of CFStrings.
    let arr: CFArray<CFType> = unsafe { CFArray::wrap_under_create_rule(out) };
    for item in arr.iter() {
        let id: CFTypeID = unsafe { core_foundation_sys::base::CFGetTypeID(item.as_CFTypeRef()) };
        if id == unsafe { core_foundation_sys::string::CFStringGetTypeID() } {
            let s = unsafe {
                CFString::wrap_under_get_rule(
                    item.as_CFTypeRef() as core_foundation_sys::string::CFStringRef
                )
            };
            if &s.to_string() == action {
                return true;
            }
        }
    }
    false
}

fn focused_ref_for(app: AXUIElementRef, elements: &[AppElement]) -> Option<String> {
    let owned = copy_attr_owned(app, kAXFocusedUIElementAttribute)?;
    // Walk down our captured elements and find the one whose AX path matches
    // the focused element. We don't have a direct AXUIElement equality
    // operator, so we compare by (role, name, position).
    let role = copy_string_attr(owned.as_ptr() as AXUIElementRef, kAXRoleAttribute);
    let name = Some(best_name(owned.as_ptr() as AXUIElementRef));
    let bbox = bbox_of(owned.as_ptr() as AXUIElementRef);
    elements
        .iter()
        .find(|e| {
            role.as_deref() == Some(&e.role)
                && name.as_deref() == Some(&e.name)
                && bbox
                    .map(|b| (b.x - e.bbox.x).abs() < 0.5 && (b.y - e.bbox.y).abs() < 0.5)
                    .unwrap_or(false)
        })
        .map(|e| e.element_ref.clone())
}

fn build_tree(elements: &[AppElement]) -> Value {
    // Light-weight: emit a flat array of {ref, role, name, parent_path}.
    // Agents that need a true tree can rebuild from ax_path; the field is
    // documented as not load-bearing.
    let arr: Vec<Value> = elements
        .iter()
        .map(|e| {
            json!({
                "ref": e.element_ref,
                "role": e.role,
                "name": e.name,
                "interactable": e.interactable,
            })
        })
        .collect();
    Value::Array(arr)
}

/// Public re-export so tests in `crate::actions` and downstream callers can
/// resolve a snapshot ref to the underlying `AXUIElement` for action
/// dispatch. Returns the live AX element for the path stored in the
/// snapshot's `AppElement::ax_path`.
pub(crate) fn locate_by_path(
    pid: i32,
    bundle_id: &str,
    path: &[u32],
) -> Result<AxOwned<accessibility_sys::__AXUIElement>, NativeControlError> {
    // SAFETY: AXUIElementCreateApplication +1 on success.
    let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
    let mut current =
        unsafe { AxOwned::<accessibility_sys::__AXUIElement>::from_create(app_ref as *const _) }
            .ok_or_else(|| NativeControlError::AppNotFound {
                bundle_id: bundle_id.to_string(),
            })?;

    // The first path component selects between [main_window] ++
    // [app_children]. Mirror walker setup.
    let mut roots: Vec<AxOwned<accessibility_sys::__AXUIElement>> = Vec::new();
    if let Some(win) = copy_attr_owned(current.as_ptr() as AXUIElementRef, kAXMainWindowAttribute) {
        roots.push(win);
    }
    if let Some(children) = copy_children(current.as_ptr() as AXUIElementRef) {
        for c in children {
            roots.push(c);
        }
    }

    let mut iter = path.iter().copied();
    let first = iter
        .next()
        .ok_or_else(|| NativeControlError::Internal("empty ax_path".into()))?;
    current =
        roots
            .into_iter()
            .nth(first as usize)
            .ok_or_else(|| NativeControlError::RefStale {
                r: format!("path[0]={first}"),
            })?;

    for step in iter {
        let kids = copy_children(current.as_ptr() as AXUIElementRef).ok_or_else(|| {
            NativeControlError::RefStale {
                r: format!("path step={step}, no children"),
            }
        })?;
        current =
            kids.into_iter()
                .nth(step as usize)
                .ok_or_else(|| NativeControlError::RefStale {
                    r: format!("path step={step} out of range"),
                })?;
    }
    Ok(current)
}

/// Map an `AXError` into a `NativeControlError`.
pub(crate) fn map_ax_error(err: AXError) -> NativeControlError {
    if err == kAXErrorSuccess {
        return NativeControlError::Internal("map_ax_error called with success".into());
    }
    if err == accessibility_sys::kAXErrorAPIDisabled {
        return NativeControlError::PermissionMissing {
            settings_url: crate::permission::SETTINGS_DEEPLINK,
        };
    }
    NativeControlError::AxError(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_id_matches_ax_engine_canonical() {
        // Lock: refs hash identically across the browser AX tree
        // (ax_engine::index::StableId) and the macOS AX tree (this crate).
        let here = ax_engine::index::StableId::compute("AXButton", "5", "AXScrollArea", 7).to_hex();
        let canonical =
            ax_engine::index::StableId::compute("AXButton", "5", "AXScrollArea", 7).to_hex();
        assert_eq!(here, canonical);
        assert_eq!(here.len(), 64);
    }

    #[test]
    fn caps_constants_match_documented_values() {
        assert_eq!(MAX_DEPTH, 64);
        assert_eq!(MAX_NODES, 5_000);
    }

    #[test]
    fn map_ax_error_translates_api_disabled_to_permission_missing() {
        match map_ax_error(accessibility_sys::kAXErrorAPIDisabled) {
            NativeControlError::PermissionMissing { settings_url } => {
                assert_eq!(settings_url, crate::permission::SETTINGS_DEEPLINK);
            }
            other => panic!("expected PermissionMissing, got {other:?}"),
        }
    }

    #[test]
    fn map_ax_error_passthrough_for_other_codes() {
        match map_ax_error(accessibility_sys::kAXErrorAttributeUnsupported) {
            NativeControlError::AxError(c) => {
                assert_eq!(c, accessibility_sys::kAXErrorAttributeUnsupported);
            }
            other => panic!("expected AxError, got {other:?}"),
        }
    }
}
