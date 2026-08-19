use crate::model::{
    now_ms, AppWorld, AxEventSummary, Coherence, CursorState, DisplayWorld, FocusedWindow,
    FrameRef, FsEventSummary, InputEvent, InputEventKind, SnapshotSource, SnapshotSummary,
    WorldSnapshot,
};
use native_control::{AppHandle, AppSnapshot, AxEvent, WindowHandle};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use system_control::types::Display;
use system_control::FsEvent;
use vision::FrameHandle;

const RECENT_AX_CAP: usize = 64;
const RECENT_FS_CAP: usize = 64;
const RECENT_INPUT_CAP: usize = 64;

#[derive(Debug, Clone)]
pub enum WorldModelInput {
    AxSnapshot {
        app: AppHandle,
        snapshot: AppSnapshot,
    },
    Lifecycle {
        apps: Vec<AppHandle>,
    },
    WindowInventory {
        bundle_id: String,
        windows: Vec<WindowHandle>,
    },
    FrameReady {
        display_id: u32,
        frame: FrameHandle,
    },
    Displays {
        displays: Vec<Display>,
    },
    Cursor {
        x: f64,
        y: f64,
    },
    Input {
        event: InputEvent,
    },
    AxEvent {
        event: AxEvent,
    },
    FsEvent {
        event: FsEvent,
    },
}

#[derive(Debug)]
struct Inner {
    generation: u64,
    apps: HashMap<String, AppWorld>,
    displays: HashMap<u32, DisplayWorld>,
    focused_window: Option<FocusedWindow>,
    cursor: Option<CursorState>,
    recent_ax_events: VecDeque<AxEventSummary>,
    recent_fs_events: VecDeque<FsEventSummary>,
    recent_input: VecDeque<InputEvent>,
    sources: Vec<SnapshotSource>,
    coherence: Coherence,
}

impl Inner {
    fn new() -> Self {
        Self {
            generation: 0,
            apps: HashMap::new(),
            displays: HashMap::new(),
            focused_window: None,
            cursor: None,
            recent_ax_events: VecDeque::new(),
            recent_fs_events: VecDeque::new(),
            recent_input: VecDeque::new(),
            sources: Vec::new(),
            coherence: Coherence::Degraded {
                reason: "world model not yet primed".to_string(),
            },
        }
    }
}

/// Explicit fused world-model publisher.
///
/// One instance ingests five named streams and publishes one atomic snapshot
/// with one generation. This is the public code artifact the architecture was
/// promising.
#[derive(Debug)]
pub struct WorldModel {
    inner: Mutex<Inner>,
}

impl WorldModel {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::new()),
        })
    }

    pub fn apply(&self, input: WorldModelInput) -> WorldSnapshot {
        let mut g = self.inner.lock();
        g.generation = g.generation.saturating_add(1);
        g.sources.clear();
        g.coherence = Coherence::Coherent;

        match input {
            WorldModelInput::AxSnapshot { app, snapshot } => {
                let bundle_id = app.bundle_id.clone();
                let windows = g
                    .apps
                    .get(&bundle_id)
                    .map(|a| a.windows.clone())
                    .unwrap_or_default();
                g.apps.insert(
                    bundle_id.clone(),
                    AppWorld {
                        handle: app,
                        windows,
                        snapshot: Some(SnapshotSummary {
                            snapshot_seq: snapshot.snapshot_seq,
                            focused_ref: snapshot.focused_ref.clone(),
                            title: snapshot.title.clone(),
                            payload: serde_json::to_value(&snapshot)
                                .unwrap_or(serde_json::Value::Null),
                        }),
                    },
                );
                if let Some(focused_ref) = snapshot.focused_ref.as_ref() {
                    if let Some(element) = snapshot
                        .elements
                        .iter()
                        .find(|e| &e.element_ref == focused_ref)
                    {
                        g.focused_window = Some(FocusedWindow {
                            bundle_id,
                            window_id: element.app_id.clone(),
                            title: snapshot.title.clone(),
                        });
                    }
                }
                g.sources.push(SnapshotSource::AxStructure);
            }
            WorldModelInput::Lifecycle { apps } => {
                let focused_bundle = apps
                    .iter()
                    .find(|a| a.has_focus)
                    .map(|a| a.bundle_id.clone());
                for app in apps {
                    let entry = g
                        .apps
                        .entry(app.bundle_id.clone())
                        .or_insert_with(|| AppWorld {
                            handle: app.clone(),
                            windows: Vec::new(),
                            snapshot: None,
                        });
                    entry.handle = app;
                }
                if let Some(bundle_id) = focused_bundle {
                    if let Some(app) = g.apps.get(&bundle_id) {
                        if let Some(win) = app.windows.iter().find(|w| w.main) {
                            g.focused_window = Some(FocusedWindow {
                                bundle_id,
                                window_id: win.window_id.clone(),
                                title: win.title.clone(),
                            });
                        }
                    }
                }
                g.sources.push(SnapshotSource::Lifecycle);
            }
            WorldModelInput::WindowInventory { bundle_id, windows } => {
                if let Some(app) = g.apps.get_mut(&bundle_id) {
                    let focused = app.handle.has_focus;
                    let main = windows.iter().find(|w| w.main).cloned();
                    app.windows = windows;
                    if focused {
                        if let Some(win) = main {
                            g.focused_window = Some(FocusedWindow {
                                bundle_id: bundle_id.clone(),
                                window_id: win.window_id,
                                title: win.title,
                            });
                        }
                    }
                } else {
                    g.coherence = Coherence::Skewed {
                        stale_ms: 0,
                        reason: format!(
                            "window inventory arrived before lifecycle for {bundle_id}"
                        ),
                    };
                }
                g.sources.push(SnapshotSource::WindowServer);
            }
            WorldModelInput::FrameReady { display_id, frame } => {
                if let Some(display) = g.displays.get_mut(&display_id) {
                    display.latest_frame = Some(FrameRef {
                        shm_path: frame.shm_path.to_string_lossy().into_owned(),
                        slot_seq: frame.slot_seq,
                        slot_index: frame.slot_index,
                        offset: frame.offset,
                        len: frame.len,
                        ts_us: frame.ts_us,
                    });
                } else {
                    g.coherence = Coherence::Skewed {
                        stale_ms: 0,
                        reason: format!(
                            "frame arrived before display inventory for display {display_id}"
                        ),
                    };
                }
                g.sources.push(SnapshotSource::Capture);
            }
            WorldModelInput::Displays { displays } => {
                g.displays = displays
                    .into_iter()
                    .map(|display| {
                        (
                            display.id,
                            DisplayWorld {
                                display,
                                latest_frame: None,
                            },
                        )
                    })
                    .collect();
                g.sources.push(SnapshotSource::WindowServer);
            }
            WorldModelInput::Cursor { x, y } => {
                g.cursor = Some(CursorState { x, y });
                g.recent_input.push_back(InputEvent {
                    at_ms: now_ms(),
                    kind: InputEventKind::MouseMove,
                    x: Some(x),
                    y: Some(y),
                    key: None,
                });
                while g.recent_input.len() > RECENT_INPUT_CAP {
                    g.recent_input.pop_front();
                }
                g.sources.push(SnapshotSource::CursorInput);
            }
            WorldModelInput::Input { event } => {
                g.recent_input.push_back(event);
                while g.recent_input.len() > RECENT_INPUT_CAP {
                    g.recent_input.pop_front();
                }
                g.sources.push(SnapshotSource::CursorInput);
            }
            WorldModelInput::AxEvent { event } => {
                g.recent_ax_events.push_back(event.into());
                while g.recent_ax_events.len() > RECENT_AX_CAP {
                    g.recent_ax_events.pop_front();
                }
                g.sources.push(SnapshotSource::AxStructure);
            }
            WorldModelInput::FsEvent { event } => {
                g.recent_fs_events.push_back(event.into());
                while g.recent_fs_events.len() > RECENT_FS_CAP {
                    g.recent_fs_events.pop_front();
                }
                g.sources.push(SnapshotSource::WindowServer);
            }
        }

        WorldSnapshot {
            generation: g.generation,
            captured_at_ms: now_ms(),
            sources: g.sources.clone(),
            coherence: g.coherence.clone(),
            apps: g.apps.values().cloned().collect(),
            displays: g.displays.values().cloned().collect(),
            focused_window: g.focused_window.clone(),
            cursor: g.cursor.clone(),
            recent_ax_events: g.recent_ax_events.iter().cloned().collect(),
            recent_fs_events: g.recent_fs_events.iter().cloned().collect(),
            recent_input: g.recent_input.iter().cloned().collect(),
        }
    }

    pub fn latest(&self) -> WorldSnapshot {
        let g = self.inner.lock();
        if g.generation == 0 {
            return WorldSnapshot::empty();
        }
        WorldSnapshot {
            generation: g.generation,
            captured_at_ms: now_ms(),
            sources: g.sources.clone(),
            coherence: g.coherence.clone(),
            apps: g.apps.values().cloned().collect(),
            displays: g.displays.values().cloned().collect(),
            focused_window: g.focused_window.clone(),
            cursor: g.cursor.clone(),
            recent_ax_events: g.recent_ax_events.iter().cloned().collect(),
            recent_fs_events: g.recent_fs_events.iter().cloned().collect(),
            recent_input: g.recent_input.iter().cloned().collect(),
        }
    }
}
