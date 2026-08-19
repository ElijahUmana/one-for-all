use native_control::{AppHandle, AppSnapshot, AxEvent, AxEventTopic, BBox, WindowHandle};
use std::path::PathBuf;
use system_control::types::Display;
use system_control::{FsEvent, FsEventFlag};
use vision::FrameHandle;
use world_model::{
    Coherence, InputEvent, InputEventKind, SnapshotSource, WorldModel, WorldModelInput,
};

fn app(bundle_id: &str, pid: i32, focus: bool) -> AppHandle {
    AppHandle {
        bundle_id: bundle_id.to_string(),
        pid,
        name: bundle_id.to_string(),
        has_focus: focus,
    }
}

fn window(id: &str, bundle_id: &str, title: &str, main: bool) -> WindowHandle {
    WindowHandle {
        window_id: id.to_string(),
        bundle_id: bundle_id.to_string(),
        title: title.to_string(),
        bbox: BBox {
            x: 10.0,
            y: 20.0,
            w: 100.0,
            h: 50.0,
        },
        minimized: false,
        fullscreen: false,
        main,
    }
}

fn snapshot(bundle_id: &str, focused_ref: Option<&str>) -> AppSnapshot {
    AppSnapshot {
        snapshot_seq: 7,
        app_id: bundle_id.to_string(),
        bundle_id: bundle_id.to_string(),
        pid: 42,
        title: "Front Window".into(),
        focused_ref: focused_ref.map(str::to_string),
        elements: vec![],
        tree: serde_json::json!({"role":"AXWindow"}),
        truncated_at: None,
    }
}

fn display(id: u32) -> Display {
    Display {
        id,
        origin_x: 0,
        origin_y: 0,
        width: 1920,
        height: 1080,
        scale: 2.0,
        is_main: true,
    }
}

fn frame(display_id: u32) -> FrameHandle {
    FrameHandle {
        shm_path: PathBuf::from(format!("/tmp/frame-{display_id}")),
        slot_seq: 9,
        slot_index: 1,
        offset: 1024,
        len: 2048,
        ts_us: 123_456,
    }
}

#[test]
fn world_model_names_the_five_streams_in_code() {
    let wm = WorldModel::new();

    let s1 = wm.apply(WorldModelInput::Lifecycle {
        apps: vec![app("com.apple.TextEdit", 42, true)],
    });
    assert_eq!(s1.generation, 1);
    assert_eq!(s1.sources, vec![SnapshotSource::Lifecycle]);

    let s2 = wm.apply(WorldModelInput::WindowInventory {
        bundle_id: "com.apple.TextEdit".into(),
        windows: vec![window("w0", "com.apple.TextEdit", "Front Window", true)],
    });
    assert_eq!(s2.generation, 2);
    assert_eq!(s2.sources, vec![SnapshotSource::WindowServer]);

    let s3 = wm.apply(WorldModelInput::AxSnapshot {
        app: app("com.apple.TextEdit", 42, true),
        snapshot: snapshot("com.apple.TextEdit", Some("e0")),
    });
    assert_eq!(s3.generation, 3);
    assert_eq!(s3.sources, vec![SnapshotSource::AxStructure]);

    let s4 = wm.apply(WorldModelInput::Displays {
        displays: vec![display(1)],
    });
    assert_eq!(s4.generation, 4);
    assert_eq!(s4.sources, vec![SnapshotSource::WindowServer]);

    let s5 = wm.apply(WorldModelInput::FrameReady {
        display_id: 1,
        frame: frame(1),
    });
    assert_eq!(s5.generation, 5);
    assert_eq!(s5.sources, vec![SnapshotSource::Capture]);

    let s6 = wm.apply(WorldModelInput::Cursor { x: 500.0, y: 300.0 });
    assert_eq!(s6.generation, 6);
    assert_eq!(s6.sources, vec![SnapshotSource::CursorInput]);
}

#[test]
fn publishes_one_atomic_snapshot_with_fused_state() {
    let wm = WorldModel::new();
    wm.apply(WorldModelInput::Lifecycle {
        apps: vec![app("com.apple.TextEdit", 42, true)],
    });
    wm.apply(WorldModelInput::WindowInventory {
        bundle_id: "com.apple.TextEdit".into(),
        windows: vec![window("w0", "com.apple.TextEdit", "Front Window", true)],
    });
    wm.apply(WorldModelInput::AxSnapshot {
        app: app("com.apple.TextEdit", 42, true),
        snapshot: snapshot("com.apple.TextEdit", Some("e0")),
    });
    wm.apply(WorldModelInput::Displays {
        displays: vec![display(7)],
    });
    wm.apply(WorldModelInput::FrameReady {
        display_id: 7,
        frame: frame(7),
    });
    wm.apply(WorldModelInput::Cursor { x: 11.0, y: 22.0 });
    wm.apply(WorldModelInput::Input {
        event: InputEvent {
            at_ms: 1,
            kind: InputEventKind::KeyDown,
            x: None,
            y: None,
            key: Some("cmd+s".into()),
        },
    });
    wm.apply(WorldModelInput::AxEvent {
        event: AxEvent {
            bundle_id: "com.apple.TextEdit".into(),
            topic: AxEventTopic::FocusedChanged,
            timestamp_ms: 2,
            element_ref: Some("e0".into()),
            role: Some("AXButton".into()),
            name: Some("Save".into()),
            value: None,
        },
    });
    wm.apply(WorldModelInput::FsEvent {
        event: FsEvent {
            watch_id: "watch-1".into(),
            path: "/tmp/demo.txt".into(),
            flags: vec![FsEventFlag::Modified],
            event_id: 9,
            ts_ns: 3_000_000,
        },
    });

    let snap = wm.latest();
    assert!(snap.generation >= 9);
    assert_eq!(snap.apps.len(), 1);
    assert_eq!(snap.displays.len(), 1);
    assert_eq!(snap.cursor.as_ref().map(|c| (c.x, c.y)), Some((11.0, 22.0)));
    assert_eq!(
        snap.focused_window.as_ref().map(|w| w.title.as_str()),
        Some("Front Window")
    );
    assert_eq!(
        snap.apps[0]
            .snapshot
            .as_ref()
            .unwrap()
            .focused_ref
            .as_deref(),
        Some("e0")
    );
    assert!(snap.apps[0].snapshot.as_ref().unwrap().payload.is_object());
    assert_eq!(snap.displays[0].latest_frame.as_ref().unwrap().slot_seq, 9);
    assert_eq!(snap.recent_input.len(), 2); // cursor move + keydown
    assert_eq!(snap.recent_ax_events.len(), 1);
    assert_eq!(snap.recent_fs_events.len(), 1);
    assert_eq!(snap.coherence, Coherence::Coherent);
}

#[test]
fn flags_skew_when_capture_arrives_before_display_inventory() {
    let wm = WorldModel::new();
    let snap = wm.apply(WorldModelInput::FrameReady {
        display_id: 99,
        frame: frame(99),
    });
    match snap.coherence {
        Coherence::Skewed {
            stale_ms: _,
            reason,
        } => {
            assert!(reason.contains("display 99"));
        }
        other => panic!("expected Skewed, got {other:?}"),
    }
}

#[test]
fn caps_recent_event_rings() {
    let wm = WorldModel::new();
    for i in 0..80u64 {
        wm.apply(WorldModelInput::Input {
            event: InputEvent {
                at_ms: i,
                kind: InputEventKind::MouseMove,
                x: Some(i as f64),
                y: Some(i as f64),
                key: None,
            },
        });
    }
    let snap = wm.latest();
    assert_eq!(snap.recent_input.len(), 64);
    assert_eq!(snap.recent_input.first().unwrap().at_ms, 16);
    assert_eq!(snap.recent_input.last().unwrap().at_ms, 79);
}
