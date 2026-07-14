//! File-system event streaming via macOS FSEvents.

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{SystemTime, UNIX_EPOCH};

    use core_foundation::array::CFArray;
    use core_foundation::base::TCFType;
    use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoop};
    use core_foundation::string::CFString;
    use core_foundation_sys::base::{CFAllocatorRef, CFIndex, CFTypeRef};
    use core_foundation_sys::string::CFStringRef;
    use parking_lot::Mutex;
    use tokio::sync::mpsc;
    use tracing::warn;

    use crate::types::{FsEvent, FsEventFlag, SystemError, SystemResult};
    use crate::{NotificationSink, WatchId};

    pub const CHANNEL_CAPACITY: usize = 1024;
    const STREAM_LATENCY_SECS: f64 = 0.1;

    const K_FSEVENT_STREAM_EVENT_ID_SINCE_NOW: u64 = u64::MAX;
    const K_FSEVENT_STREAM_CREATE_FLAG_USE_CF_TYPES: u32 = 0x0000_0001;
    const K_FSEVENT_STREAM_CREATE_FLAG_NO_DEFER: u32 = 0x0000_0002;
    const K_FSEVENT_STREAM_CREATE_FLAG_WATCH_ROOT: u32 = 0x0000_0004;
    const K_FSEVENT_STREAM_CREATE_FLAG_FILE_EVENTS: u32 = 0x0000_0010;

    const ITEM_CREATED: u32 = 0x0000_0100;
    const ITEM_REMOVED: u32 = 0x0000_0200;
    const ITEM_INODE_META_MOD: u32 = 0x0000_0400;
    const ITEM_RENAMED: u32 = 0x0000_0800;
    const ITEM_MODIFIED: u32 = 0x0000_1000;
    const ITEM_FINDER_INFO_MOD: u32 = 0x0000_2000;
    const ITEM_CHANGE_OWNER: u32 = 0x0000_4000;
    const ITEM_XATTR_MOD: u32 = 0x0000_8000;
    const ITEM_IS_FILE: u32 = 0x0001_0000;
    const ITEM_IS_DIR: u32 = 0x0002_0000;
    const ITEM_IS_SYMLINK: u32 = 0x0004_0000;
    const OWN_EVENT: u32 = 0x0008_0000;
    const ITEM_IS_HARDLINK: u32 = 0x0010_0000;
    const ITEM_IS_LAST_HARDLINK: u32 = 0x0020_0000;
    const ITEM_CLONED: u32 = 0x0040_0000;
    const HISTORY_DONE: u32 = 0x0000_0010;
    const ROOT_CHANGED: u32 = 0x0000_0020;
    const MOUNT: u32 = 0x0000_0040;
    const UNMOUNT: u32 = 0x0000_0080;
    const MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;
    const USER_DROPPED: u32 = 0x0000_0002;
    const KERNEL_DROPPED: u32 = 0x0000_0004;
    const EVENT_IDS_WRAPPED: u32 = 0x0000_0008;

    #[allow(non_camel_case_types)]
    type FSEventStreamEventId = u64;
    #[allow(non_camel_case_types)]
    type FSEventStreamEventFlags = u32;
    #[allow(non_camel_case_types)]
    type FSEventStreamRef = *mut std::ffi::c_void;

    #[repr(C)]
    struct FSEventStreamContext {
        version: CFIndex,
        info: *mut std::ffi::c_void,
        retain: Option<extern "C" fn(*const std::ffi::c_void) -> *const std::ffi::c_void>,
        release: Option<extern "C" fn(*const std::ffi::c_void)>,
        copy_description: Option<extern "C" fn(*const std::ffi::c_void) -> CFStringRef>,
    }

    type FSEventStreamCallback = extern "C" fn(
        stream_ref: FSEventStreamRef,
        client_call_back_info: *mut std::ffi::c_void,
        num_events: usize,
        event_paths: *mut std::ffi::c_void,
        event_flags: *const FSEventStreamEventFlags,
        event_ids: *const FSEventStreamEventId,
    );

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn FSEventStreamCreate(
            allocator: CFAllocatorRef,
            callback: FSEventStreamCallback,
            context: *const FSEventStreamContext,
            paths_to_watch: CFTypeRef,
            since_when: FSEventStreamEventId,
            latency: f64,
            flags: u32,
        ) -> FSEventStreamRef;
        fn FSEventStreamScheduleWithRunLoop(
            stream_ref: FSEventStreamRef,
            run_loop: core_foundation_sys::runloop::CFRunLoopRef,
            run_loop_mode: CFStringRef,
        );
        fn FSEventStreamStart(stream_ref: FSEventStreamRef) -> bool;
        fn FSEventStreamStop(stream_ref: FSEventStreamRef);
        fn FSEventStreamInvalidate(stream_ref: FSEventStreamRef);
        fn FSEventStreamRelease(stream_ref: FSEventStreamRef);
    }

    struct CallbackContext {
        watch_id: String,
        tx: mpsc::Sender<FsEvent>,
        alive: Arc<AtomicBool>,
    }

    struct RunLoopHandle {
        runloop: CFRunLoop,
    }

    unsafe impl Send for RunLoopHandle {}

    pub struct WatchHandle {
        watch_id: String,
        alive: Arc<AtomicBool>,
        runloop: Mutex<Option<RunLoopHandle>>,
        thread: Mutex<Option<JoinHandle<()>>>,
        forwarder: Mutex<Option<tokio::task::JoinHandle<()>>>,
    }

    impl WatchHandle {
        pub fn watch_id(&self) -> &str {
            &self.watch_id
        }
    }

    impl Drop for WatchHandle {
        fn drop(&mut self) {
            self.alive.store(false, Ordering::SeqCst);
            if let Some(runloop) = self.runloop.lock().take() {
                runloop.runloop.stop();
            }
            if let Some(handle) = self.thread.lock().take() {
                let _ = handle.join();
            }
            if let Some(forwarder) = self.forwarder.lock().take() {
                forwarder.abort();
            }
        }
    }

    extern "C" fn fsevent_callback(
        _stream_ref: FSEventStreamRef,
        client_call_back_info: *mut std::ffi::c_void,
        num_events: usize,
        event_paths: *mut std::ffi::c_void,
        event_flags: *const FSEventStreamEventFlags,
        event_ids: *const FSEventStreamEventId,
    ) {
        if client_call_back_info.is_null() {
            return;
        }
        let ctx = unsafe { &*(client_call_back_info as *const CallbackContext) };
        if !ctx.alive.load(Ordering::SeqCst) {
            return;
        }
        let paths = unsafe {
            CFArray::<CFString>::wrap_under_get_rule(
                event_paths as core_foundation_sys::array::CFArrayRef,
            )
        };
        for index in 0..num_events {
            let Some(path_cf) = paths.get(index as isize) else {
                continue;
            };
            let flags = unsafe { *event_flags.add(index) };
            let event_id = unsafe { *event_ids.add(index) };
            if flags & (MUST_SCAN_SUBDIRS | USER_DROPPED | KERNEL_DROPPED | EVENT_IDS_WRAPPED) != 0
            {
                warn!(watch_id = %ctx.watch_id, flags, event_id, "fsevents stream lost fidelity; resync required");
            }
            if flags & (OWN_EVENT | ITEM_IS_HARDLINK | ITEM_IS_LAST_HARDLINK | ITEM_CLONED) != 0 {
                warn!(watch_id = %ctx.watch_id, flags, event_id, "fsevents event carried unmodeled flags");
            }
            let event = FsEvent {
                watch_id: ctx.watch_id.clone(),
                path: PathBuf::from(path_cf.to_string()),
                flags: map_flags(flags),
                event_id,
                ts_ns: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            };
            if ctx.tx.try_send(event).is_err() {
                warn!(watch_id = %ctx.watch_id, "fsevents channel full; dropping event");
            }
        }
    }

    fn map_flags(flags: u32) -> Vec<FsEventFlag> {
        let mut out = Vec::new();
        if flags & ITEM_CREATED != 0 {
            out.push(FsEventFlag::Created);
        }
        if flags & ITEM_REMOVED != 0 {
            out.push(FsEventFlag::Removed);
        }
        if flags & ITEM_RENAMED != 0 {
            out.push(FsEventFlag::Renamed);
        }
        if flags & ITEM_MODIFIED != 0 {
            out.push(FsEventFlag::Modified);
        }
        if flags & (ITEM_INODE_META_MOD | ITEM_FINDER_INFO_MOD) != 0 {
            out.push(FsEventFlag::InodeMetaModified);
        }
        if flags & ITEM_CHANGE_OWNER != 0 {
            out.push(FsEventFlag::OwnerChanged);
        }
        if flags & ITEM_XATTR_MOD != 0 {
            out.push(FsEventFlag::XattrChanged);
        }
        if flags & ITEM_IS_FILE != 0 {
            out.push(FsEventFlag::IsFile);
        }
        if flags & ITEM_IS_DIR != 0 {
            out.push(FsEventFlag::IsDir);
        }
        if flags & ITEM_IS_SYMLINK != 0 {
            out.push(FsEventFlag::IsSymlink);
        }
        if flags & MOUNT != 0 {
            out.push(FsEventFlag::MountPoint);
        }
        if flags & UNMOUNT != 0 {
            out.push(FsEventFlag::UnmountPoint);
        }
        if flags & HISTORY_DONE != 0 {
            out.push(FsEventFlag::HistoryDone);
        }
        if flags & ROOT_CHANGED != 0 {
            out.push(FsEventFlag::RootChanged);
        }
        out
    }

    pub fn watch(
        paths: &[PathBuf],
        watch_id: WatchId,
        sink: Arc<dyn NotificationSink>,
    ) -> SystemResult<WatchHandle> {
        if paths.is_empty() {
            return Err(SystemError::InvalidArgument(
                "paths must not be empty".to_string(),
            ));
        }
        let mut unique = HashSet::new();
        let canonicalized: Vec<PathBuf> = paths
            .iter()
            .map(|path| {
                let canonical = path
                    .canonicalize()
                    .map_err(|e| SystemError::Io(e.to_string()))?;
                if !unique.insert(canonical.clone()) {
                    return Err(SystemError::InvalidArgument(format!(
                        "duplicate watch path {canonical:?}"
                    )));
                }
                Ok(canonical)
            })
            .collect::<SystemResult<_>>()?;

        let (tx, mut rx) = mpsc::channel::<FsEvent>(CHANNEL_CAPACITY);
        let alive = Arc::new(AtomicBool::new(true));
        let (runloop_tx, runloop_rx) =
            std::sync::mpsc::sync_channel::<SystemResult<RunLoopHandle>>(1);
        let ctx_watch_id = watch_id.clone();
        let alive_thread = Arc::clone(&alive);
        let thread_paths = canonicalized;
        let thread = std::thread::Builder::new()
            .name(format!("fsevents-{watch_id}"))
            .spawn(move || {
                run_watch_thread(thread_paths, ctx_watch_id, tx, alive_thread, runloop_tx)
            })
            .map_err(|e| SystemError::Internal(format!("spawn fsevents thread: {e}")))?;
        let runloop = runloop_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| {
                SystemError::Internal("fsevents thread never reported startup status".to_string())
            })??;

        let forward_watch_id = watch_id.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match serde_json::to_value(event) {
                    Ok(payload) => sink.notify(payload),
                    Err(e) => {
                        warn!(watch_id = %forward_watch_id, error = %e, "serialize fsevents event failed")
                    }
                }
            }
        });

        Ok(WatchHandle {
            watch_id,
            alive,
            runloop: Mutex::new(Some(runloop)),
            thread: Mutex::new(Some(thread)),
            forwarder: Mutex::new(Some(forwarder)),
        })
    }

    fn run_watch_thread(
        paths: Vec<PathBuf>,
        watch_id: String,
        tx: mpsc::Sender<FsEvent>,
        alive: Arc<AtomicBool>,
        runloop_tx: std::sync::mpsc::SyncSender<SystemResult<RunLoopHandle>>,
    ) {
        let cf_strings: Vec<CFString> = paths
            .iter()
            .map(|path| CFString::new(path.to_string_lossy().as_ref()))
            .collect();
        let cf_paths = CFArray::from_CFTypes(&cf_strings);
        let mut ctx = Box::new(CallbackContext {
            watch_id: watch_id.clone(),
            tx,
            alive,
        });
        let context = FSEventStreamContext {
            version: 0,
            info: (&mut *ctx) as *mut CallbackContext as *mut std::ffi::c_void,
            retain: None,
            release: None,
            copy_description: None,
        };
        let stream = unsafe {
            FSEventStreamCreate(
                std::ptr::null(),
                fsevent_callback,
                &context,
                cf_paths.as_CFTypeRef(),
                K_FSEVENT_STREAM_EVENT_ID_SINCE_NOW,
                STREAM_LATENCY_SECS,
                K_FSEVENT_STREAM_CREATE_FLAG_USE_CF_TYPES
                    | K_FSEVENT_STREAM_CREATE_FLAG_NO_DEFER
                    | K_FSEVENT_STREAM_CREATE_FLAG_WATCH_ROOT
                    | K_FSEVENT_STREAM_CREATE_FLAG_FILE_EVENTS,
            )
        };
        let runloop = CFRunLoop::get_current();
        if stream.is_null() {
            let _ = runloop_tx.send(Err(SystemError::Internal(
                "FSEventStreamCreate returned null".to_string(),
            )));
            warn!(watch_id = %watch_id, "FSEventStreamCreate returned null");
            drop(ctx);
            return;
        }
        let _ = runloop_tx.send(Ok(RunLoopHandle {
            runloop: runloop.clone(),
        }));
        unsafe {
            FSEventStreamScheduleWithRunLoop(
                stream,
                runloop.as_concrete_TypeRef(),
                kCFRunLoopDefaultMode,
            );
        }
        if unsafe { FSEventStreamStart(stream) } {
            CFRunLoop::run_current();
        } else {
            warn!("FSEventStreamStart failed");
        }
        unsafe {
            FSEventStreamStop(stream);
            FSEventStreamInvalidate(stream);
            FSEventStreamRelease(stream);
        }
        drop(ctx);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn maps_known_fsevent_flags() {
            let flags = map_flags(ITEM_CREATED | ITEM_IS_FILE | ITEM_XATTR_MOD);
            assert!(flags.contains(&FsEventFlag::Created));
            assert!(flags.contains(&FsEventFlag::IsFile));
            assert!(flags.contains(&FsEventFlag::XattrChanged));
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::types::{SystemError, SystemResult};
    use crate::{NotificationSink, WatchId};

    pub struct WatchHandle;

    pub fn watch(
        _paths: &[PathBuf],
        _watch_id: WatchId,
        _sink: Arc<dyn NotificationSink>,
    ) -> SystemResult<WatchHandle> {
        Err(SystemError::UnsupportedPlatform)
    }
}

pub use imp::{watch, WatchHandle};
