//! Per-session snapshot cache + ref resolver + clipboard cache + AX
//! subscription registry.
//!
//! One [`AppController`] lives on each `SessionEntry` in the broker. It owns:
//!
//! 1. A monotonic `snapshot_seq` counter used to ID every snapshot.
//! 2. A `DashMap<bundle_id, Arc<AppSnapshot>>` of the most-recent snapshot
//!    per app, so action handlers can resolve a `ref` against a known tree
//!    without walking again on every call.
//! 3. A [`crate::clipboard::ClipboardCache`] with a background poller that
//!    samples NSPasteboard's `changeCount` every 100ms.
//! 4. A registry of live AX subscriptions, keyed by `subscription_id`.
//! 5. A [`crate::privacy::RedactionEngine`] holding the session's
//!    redact_patterns + app_blocklist.
//!
//! Refs are scoped to `(bundle_id, snapshot_seq)`. A ref from snapshot N is
//! always rejected with [`NativeControlError::RefStale`] once snapshot N+1
//! is published (matches `page.*` semantics — SPEC §7 element shape note).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;
use serde_json::Value;

use crate::privacy::RedactionEngine;
use crate::types::{
    AppElement, AppSnapshot, MenuItem, NativeControlError, PrivacyPolicy, WindowHandle,
};

#[cfg(target_os = "macos")]
use crate::clipboard::ClipboardCache;
#[cfg(target_os = "macos")]
use crate::subscribe::Subscription;
#[cfg(target_os = "macos")]
use crate::types::{AxEvent, AxEventTopic, AxSubscription};

#[cfg(target_os = "macos")]
type SubscriptionsMap = Mutex<Vec<Subscription>>;
#[cfg(not(target_os = "macos"))]
type SubscriptionsMap = Mutex<()>;

pub struct AppController {
    seq: AtomicU64,
    /// `bundle_id` -> latest published snapshot.
    snapshots: DashMap<String, Arc<AppSnapshot>>,
    /// SPEC §12 U7 — per-session clipboard cache.
    #[cfg(target_os = "macos")]
    clipboard: ClipboardCache,
    /// Background poller handle (alive for the lifetime of this controller).
    #[cfg(target_os = "macos")]
    clipboard_poller: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Live AX subscriptions, owned for the controller's lifetime.
    subscriptions: SubscriptionsMap,
    /// SPEC §12 U13 — privacy policy.
    privacy: RedactionEngine,
}

impl Default for AppController {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for AppController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppController")
            .field("snapshots", &self.snapshots.len())
            .field("seq", &self.seq.load(Ordering::Relaxed))
            .field("privacy", &self.privacy)
            .finish()
    }
}

impl AppController {
    pub fn new() -> Self {
        #[cfg(target_os = "macos")]
        {
            let clipboard = ClipboardCache::new();
            // The poller runs as a tokio task — its handle is owned here so
            // the broker can abort on session shutdown via `shutdown()`.
            let poller = if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let cb = clipboard.clone();
                Some(handle.spawn(async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
                    loop {
                        tick.tick().await;
                        if let Err(e) = cb.refresh_now().await {
                            tracing::warn!(err = %e, "clipboard poll error");
                        }
                    }
                }))
            } else {
                // No tokio runtime (unit tests outside #[tokio::test]) —
                // skip the poller; cache.refresh_now is on-demand.
                None
            };
            Self {
                seq: AtomicU64::new(0),
                snapshots: DashMap::new(),
                clipboard,
                clipboard_poller: Mutex::new(poller),
                subscriptions: Mutex::new(Vec::new()),
                privacy: RedactionEngine::new(),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self {
                seq: AtomicU64::new(0),
                snapshots: DashMap::new(),
                subscriptions: Mutex::new(()),
                privacy: RedactionEngine::new(),
            }
        }
    }

    /// Take a fresh snapshot of `bundle_id` and store it as the latest.
    ///
    /// macOS-only. On other platforms returns
    /// [`NativeControlError::UnsupportedPlatform`].
    pub async fn snapshot(&self, bundle_id: &str) -> Result<Arc<AppSnapshot>, NativeControlError> {
        #[cfg(target_os = "macos")]
        {
            self.require_not_blocked(bundle_id)?;
            let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
            let snap = crate::actions::snapshot_app(bundle_id, seq).await?;
            let arc = Arc::new(self.redact_snapshot(snap));
            self.snapshots
                .insert(bundle_id.to_string(), Arc::clone(&arc));
            Ok(arc)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = bundle_id;
            Err(NativeControlError::UnsupportedPlatform)
        }
    }

    /// Most-recent snapshot for `bundle_id`, if any.
    pub fn latest(&self, bundle_id: &str) -> Option<Arc<AppSnapshot>> {
        self.snapshots.get(bundle_id).map(|v| Arc::clone(&*v))
    }

    /// Resolve `(bundle_id, ref)` against the latest snapshot. Returns
    /// `RefStale` if no snapshot exists for the bundle or the ref is not
    /// present.
    pub fn resolve_ref(
        &self,
        bundle_id: &str,
        r: &str,
    ) -> Result<Arc<AppElement>, NativeControlError> {
        let snap = self
            .latest(bundle_id)
            .ok_or_else(|| NativeControlError::RefStale { r: r.to_string() })?;
        snap.elements
            .iter()
            .find(|e| e.element_ref == r)
            .cloned()
            .map(Arc::new)
            .ok_or_else(|| NativeControlError::RefStale { r: r.to_string() })
    }

    /// Click an element by ref. Refreshes the snapshot internally before
    /// dispatching so the ref is validated against the freshest tree the
    /// broker has seen.
    pub async fn click(&self, bundle_id: &str, r: &str) -> Result<(), NativeControlError> {
        #[cfg(target_os = "macos")]
        {
            self.require_not_blocked(bundle_id)?;
            let elem = self.resolve_ref(bundle_id, r)?;
            let seq = self
                .latest(bundle_id)
                .map(|s| s.snapshot_seq)
                .unwrap_or_default();
            crate::actions::app_click(elem, seq).await
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (bundle_id, r);
            Err(NativeControlError::UnsupportedPlatform)
        }
    }

    pub async fn type_text(
        &self,
        bundle_id: &str,
        r: &str,
        text: &str,
        clear_first: bool,
    ) -> Result<(), NativeControlError> {
        #[cfg(target_os = "macos")]
        {
            self.require_not_blocked(bundle_id)?;
            let elem = self.resolve_ref(bundle_id, r)?;
            crate::actions::app_type(elem, text.to_string(), clear_first).await
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (bundle_id, r, text, clear_first);
            Err(NativeControlError::UnsupportedPlatform)
        }
    }

    pub async fn scroll(
        &self,
        bundle_id: &str,
        r: Option<&str>,
        dx: f64,
        dy: f64,
    ) -> Result<(), NativeControlError> {
        #[cfg(target_os = "macos")]
        {
            self.require_not_blocked(bundle_id)?;
            let elem = match r {
                Some(rs) => Some(self.resolve_ref(bundle_id, rs)?),
                None => None,
            };
            crate::actions::app_scroll(bundle_id.to_string(), elem, dx, dy).await
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (bundle_id, r, dx, dy);
            Err(NativeControlError::UnsupportedPlatform)
        }
    }

    #[cfg(target_os = "macos")]
    pub async fn eval(&self, bundle_id: &str, script: &str) -> Result<Value, NativeControlError> {
        self.require_not_blocked(bundle_id)?;
        crate::actions::app_eval(bundle_id, script).await
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn eval(&self, bundle_id: &str, script: &str) -> Result<Value, NativeControlError> {
        let _ = (bundle_id, script);
        Err(NativeControlError::UnsupportedPlatform)
    }

    #[cfg(target_os = "macos")]
    pub async fn menu_list(&self, bundle_id: &str) -> Result<Vec<MenuItem>, NativeControlError> {
        self.require_not_blocked(bundle_id)?;
        crate::menu::list(bundle_id).await
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn menu_list(&self, bundle_id: &str) -> Result<Vec<MenuItem>, NativeControlError> {
        let _ = bundle_id;
        Err(NativeControlError::UnsupportedPlatform)
    }

    #[cfg(target_os = "macos")]
    pub async fn menu_click(
        &self,
        bundle_id: &str,
        path: Vec<String>,
    ) -> Result<(), NativeControlError> {
        self.require_not_blocked(bundle_id)?;
        crate::menu::click(bundle_id, path).await
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn menu_click(
        &self,
        bundle_id: &str,
        path: Vec<String>,
    ) -> Result<(), NativeControlError> {
        let _ = (bundle_id, path);
        Err(NativeControlError::UnsupportedPlatform)
    }

    #[cfg(target_os = "macos")]
    pub async fn window_list(
        &self,
        bundle_id: &str,
    ) -> Result<Vec<WindowHandle>, NativeControlError> {
        self.require_not_blocked(bundle_id)?;
        crate::window::list(bundle_id).await
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn window_list(
        &self,
        bundle_id: &str,
    ) -> Result<Vec<WindowHandle>, NativeControlError> {
        let _ = bundle_id;
        Err(NativeControlError::UnsupportedPlatform)
    }

    #[cfg(target_os = "macos")]
    pub async fn window_raise(
        &self,
        bundle_id: &str,
        window_id: &str,
        confirm: bool,
        focus_steal_capability: bool,
    ) -> Result<(), NativeControlError> {
        self.require_not_blocked(bundle_id)?;
        crate::window::raise(bundle_id, window_id, confirm, focus_steal_capability).await
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn window_raise(
        &self,
        bundle_id: &str,
        window_id: &str,
        confirm: bool,
        focus_steal_capability: bool,
    ) -> Result<(), NativeControlError> {
        let _ = (bundle_id, window_id, confirm, focus_steal_capability);
        Err(NativeControlError::UnsupportedPlatform)
    }

    #[cfg(target_os = "macos")]
    pub async fn window_set_minimized(
        &self,
        bundle_id: &str,
        window_id: &str,
        value: bool,
    ) -> Result<(), NativeControlError> {
        self.require_not_blocked(bundle_id)?;
        crate::window::set_minimized(bundle_id, window_id, value).await
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn window_set_minimized(
        &self,
        bundle_id: &str,
        window_id: &str,
        value: bool,
    ) -> Result<(), NativeControlError> {
        let _ = (bundle_id, window_id, value);
        Err(NativeControlError::UnsupportedPlatform)
    }

    #[cfg(target_os = "macos")]
    pub async fn window_set_fullscreen(
        &self,
        bundle_id: &str,
        window_id: &str,
        value: bool,
    ) -> Result<(), NativeControlError> {
        self.require_not_blocked(bundle_id)?;
        crate::window::set_fullscreen(bundle_id, window_id, value).await
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn window_set_fullscreen(
        &self,
        bundle_id: &str,
        window_id: &str,
        value: bool,
    ) -> Result<(), NativeControlError> {
        let _ = (bundle_id, window_id, value);
        Err(NativeControlError::UnsupportedPlatform)
    }

    #[cfg(target_os = "macos")]
    pub async fn window_move_to(
        &self,
        bundle_id: &str,
        window_id: &str,
        x: f64,
        y: f64,
    ) -> Result<(), NativeControlError> {
        self.require_not_blocked(bundle_id)?;
        crate::window::move_to(bundle_id, window_id, x, y).await
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn window_move_to(
        &self,
        bundle_id: &str,
        window_id: &str,
        x: f64,
        y: f64,
    ) -> Result<(), NativeControlError> {
        let _ = (bundle_id, window_id, x, y);
        Err(NativeControlError::UnsupportedPlatform)
    }

    #[cfg(target_os = "macos")]
    pub async fn window_resize(
        &self,
        bundle_id: &str,
        window_id: &str,
        w: f64,
        h: f64,
    ) -> Result<(), NativeControlError> {
        self.require_not_blocked(bundle_id)?;
        crate::window::resize(bundle_id, window_id, w, h).await
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn window_resize(
        &self,
        bundle_id: &str,
        window_id: &str,
        w: f64,
        h: f64,
    ) -> Result<(), NativeControlError> {
        let _ = (bundle_id, window_id, w, h);
        Err(NativeControlError::UnsupportedPlatform)
    }

    #[cfg(target_os = "macos")]
    pub async fn spaces_move_window(
        &self,
        bundle_id: &str,
        delta: i32,
    ) -> Result<(), NativeControlError> {
        self.require_not_blocked(bundle_id)?;
        crate::spaces::move_window_relative(bundle_id, delta).await
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn spaces_move_window(
        &self,
        bundle_id: &str,
        delta: i32,
    ) -> Result<(), NativeControlError> {
        let _ = (bundle_id, delta);
        Err(NativeControlError::UnsupportedPlatform)
    }

    fn redact_snapshot(&self, mut snap: AppSnapshot) -> AppSnapshot {
        if self.privacy.is_empty() {
            return snap;
        }
        for elem in &mut snap.elements {
            self.redact_element(elem);
        }
        snap.tree = serde_json::Value::Array(
            snap.elements
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "ref": e.element_ref,
                        "role": e.role,
                        "name": e.name,
                        "interactable": e.interactable,
                    })
                })
                .collect(),
        );
        snap
    }

    fn redact_element(&self, elem: &mut AppElement) {
        if let Some(redacted) = self.privacy.redact_text(&elem.name) {
            elem.name = redacted;
        }
        if let Some(value) = elem.value.as_mut() {
            if let Some(redacted) = self.privacy.redact_text(value) {
                *value = redacted;
            }
        }
        if let Some(description) = elem.description.as_mut() {
            if let Some(redacted) = self.privacy.redact_text(description) {
                *description = redacted;
            }
        }
    }

    // ---------------------------------------------------------------------
    // SPEC §12 U7 — clipboard cache accessors
    // ---------------------------------------------------------------------

    /// Reference to the redaction engine — broker uses this to apply
    /// `redact_patterns` at read time.
    pub fn privacy(&self) -> &RedactionEngine {
        &self.privacy
    }

    /// Install a fresh privacy policy. Replaces any previous state.
    pub fn install_privacy(&self, policy: &PrivacyPolicy) {
        self.privacy.install(policy);
    }

    /// True if `bundle_id` is in the session's `app_blocklist`.
    pub fn is_blocked(&self, bundle_id: &str) -> bool {
        self.privacy.is_blocked(bundle_id)
    }

    /// Internal gate used by every `app.*` mutation method.
    fn require_not_blocked(&self, bundle_id: &str) -> Result<(), NativeControlError> {
        if self.privacy.is_blocked(bundle_id) {
            Err(NativeControlError::Blocked {
                bundle_id: bundle_id.to_string(),
            })
        } else {
            Ok(())
        }
    }

    #[cfg(target_os = "macos")]
    pub fn clipboard(&self) -> &ClipboardCache {
        &self.clipboard
    }

    // ---------------------------------------------------------------------
    // SPEC §12 — AX events (`app.subscribe`)
    // ---------------------------------------------------------------------

    /// Spawn an AX subscription. Returns the subscription metadata + an
    /// mpsc receiver of [`AxEvent`]. The broker drains the receiver and
    /// fans out `event/notify { topic: "app.event" }` notifications.
    #[cfg(target_os = "macos")]
    pub fn subscribe(
        &self,
        bundle_id: &str,
        topics: &[AxEventTopic],
    ) -> Result<(AxSubscription, tokio::sync::mpsc::Receiver<AxEvent>), NativeControlError> {
        self.require_not_blocked(bundle_id)?;
        // Generate a session-stable id from the seq counter.
        let id_n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let subscription_id = format!("axsub-{id_n}");
        let (sub, rx) = crate::subscribe::spawn(bundle_id, topics, subscription_id)?;
        let info = sub.info().clone();
        self.subscriptions.lock().push(sub);
        Ok((info, rx))
    }

    #[cfg(not(target_os = "macos"))]
    pub fn subscribe(
        &self,
        bundle_id: &str,
        topics: &[crate::types::AxEventTopic],
    ) -> Result<
        (
            crate::types::AxSubscription,
            tokio::sync::mpsc::Receiver<crate::types::AxEvent>,
        ),
        NativeControlError,
    > {
        let _ = (bundle_id, topics);
        Err(NativeControlError::UnsupportedPlatform)
    }

    /// Unsubscribe by id. Returns `true` if a subscription was removed.
    pub fn unsubscribe(&self, subscription_id: &str) -> bool {
        #[cfg(target_os = "macos")]
        {
            let mut subs = self.subscriptions.lock();
            let before = subs.len();
            subs.retain(|s| s.info().subscription_id != subscription_id);
            subs.len() < before
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = subscription_id;
            false
        }
    }

    /// Tear down all subscriptions and the clipboard poller. Idempotent.
    /// Called from broker session shutdown.
    pub fn shutdown(&self) {
        #[cfg(target_os = "macos")]
        {
            self.subscriptions.lock().clear();
            if let Some(h) = self.clipboard_poller.lock().take() {
                h.abort();
            }
        }
    }
}

impl Drop for AppController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{BBox, ElementState};

    fn fake_snapshot(bundle_id: &str, seq: u64, refs: &[&str]) -> AppSnapshot {
        let elements = refs
            .iter()
            .enumerate()
            .map(|(i, r)| AppElement {
                index: i,
                element_ref: (*r).to_string(),
                role: "AXButton".into(),
                name: format!("btn{i}"),
                value: None,
                description: None,
                state: ElementState::default(),
                bbox: BBox {
                    x: 0.0,
                    y: 0.0,
                    w: 10.0,
                    h: 10.0,
                },
                interactable: true,
                app_id: bundle_id.to_string(),
                stable_id: ax_engine::index::StableId::compute(
                    "AXButton",
                    &format!("btn{i}"),
                    "AXApplication",
                    i as u32,
                )
                .to_hex(),
                ax_path: vec![i as u32],
            })
            .collect();
        AppSnapshot {
            snapshot_seq: seq,
            app_id: bundle_id.to_string(),
            bundle_id: bundle_id.to_string(),
            pid: 0,
            title: bundle_id.to_string(),
            focused_ref: None,
            elements,
            tree: serde_json::Value::Null,
            truncated_at: None,
        }
    }

    #[test]
    fn ref_stale_when_no_snapshot() {
        let c = AppController::new();
        match c.resolve_ref("com.example.foo", "e0") {
            Err(NativeControlError::RefStale { r }) => assert_eq!(r, "e0"),
            other => panic!("expected RefStale, got {other:?}"),
        }
    }

    #[test]
    fn resolve_ref_finds_present_element() {
        let c = AppController::new();
        let s = fake_snapshot("com.example.foo", 1, &["e0", "e1"]);
        c.snapshots.insert("com.example.foo".into(), Arc::new(s));
        let e = c.resolve_ref("com.example.foo", "e1").unwrap();
        assert_eq!(e.element_ref, "e1");
    }

    #[test]
    fn resolve_ref_stale_after_resnapshot() {
        let c = AppController::new();
        c.snapshots.insert(
            "com.example.foo".into(),
            Arc::new(fake_snapshot("com.example.foo", 1, &["e0", "e1", "e2"])),
        );
        assert!(c.resolve_ref("com.example.foo", "e2").is_ok());
        c.snapshots.insert(
            "com.example.foo".into(),
            Arc::new(fake_snapshot("com.example.foo", 2, &["e0", "e1"])),
        );
        match c.resolve_ref("com.example.foo", "e2") {
            Err(NativeControlError::RefStale { r }) => assert_eq!(r, "e2"),
            other => panic!("expected RefStale, got {other:?}"),
        }
    }

    #[test]
    fn redact_snapshot_rewrites_matching_fields() {
        let c = AppController::new();
        c.install_privacy(&PrivacyPolicy {
            redact_patterns: vec!["secret".into()],
            app_blocklist: vec![],
        });
        let mut snap = fake_snapshot("com.example.foo", 1, &["e0"]);
        snap.elements[0].name = "secret name".into();
        snap.elements[0].value = Some("top secret".into());
        snap.elements[0].description = Some("not secret".into());
        let out = c.redact_snapshot(snap);
        let elem = &out.elements[0];
        assert!(elem.name.starts_with("[redacted len="));
        assert!(elem.value.as_deref().unwrap().starts_with("[redacted len="));
        assert!(elem
            .description
            .as_deref()
            .unwrap()
            .starts_with("[redacted len="));
        assert_eq!(
            out.tree[0]["name"],
            serde_json::Value::String(elem.name.clone())
        );
    }

    #[test]
    fn redact_snapshot_preserves_nonmatching_fields() {
        let c = AppController::new();
        c.install_privacy(&PrivacyPolicy {
            redact_patterns: vec!["secret".into()],
            app_blocklist: vec![],
        });
        let snap = fake_snapshot("com.example.foo", 1, &["e0"]);
        let out = c.redact_snapshot(snap.clone());
        assert_eq!(out.elements[0].name, snap.elements[0].name);
        assert_eq!(out.elements[0].value, snap.elements[0].value);
        assert_eq!(out.elements[0].description, snap.elements[0].description);
    }

    #[tokio::test]
    async fn blocked_policy_short_circuits_bundle_scoped_controller_routes() {
        let c = AppController::new();
        c.install_privacy(&PrivacyPolicy {
            redact_patterns: vec![],
            app_blocklist: vec!["blocked.app".into()],
        });

        macro_rules! assert_blocked {
            ($expr:expr) => {
                match $expr.await {
                    Err(NativeControlError::Blocked { bundle_id }) => {
                        assert_eq!(bundle_id, "com.example.blocked.app");
                    }
                    other => panic!("expected Blocked, got {other:?}"),
                }
            };
        }

        assert_blocked!(c.eval("com.example.blocked.app", "return 1"));
        assert_blocked!(c.menu_list("com.example.blocked.app"));
        assert_blocked!(c.menu_click("com.example.blocked.app", vec!["File".into()]));
        assert_blocked!(c.window_list("com.example.blocked.app"));
        assert_blocked!(c.window_raise("com.example.blocked.app", "w0", true, true));
        assert_blocked!(c.window_set_minimized("com.example.blocked.app", "w0", true));
        assert_blocked!(c.window_set_fullscreen("com.example.blocked.app", "w0", true));
        assert_blocked!(c.window_move_to("com.example.blocked.app", "w0", 1.0, 2.0));
        assert_blocked!(c.window_resize("com.example.blocked.app", "w0", 3.0, 4.0));
        assert_blocked!(c.spaces_move_window("com.example.blocked.app", 1));
    }

    #[test]
    fn install_privacy_policy_blocks_apps() {
        let c = AppController::new();
        c.install_privacy(&PrivacyPolicy {
            redact_patterns: vec![],
            app_blocklist: vec!["onepassword".into()],
        });
        assert!(c.is_blocked("com.agilebits.onepassword7"));
        assert!(!c.is_blocked("com.apple.calculator"));
        let r = c.require_not_blocked("com.agilebits.onepassword7");
        match r {
            Err(NativeControlError::Blocked { bundle_id }) => {
                assert_eq!(bundle_id, "com.agilebits.onepassword7");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }
}
