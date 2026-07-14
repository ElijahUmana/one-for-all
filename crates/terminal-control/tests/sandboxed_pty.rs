#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::time::Duration;

use terminal_control::{SessionSandbox, SpawnTerminalRequest, TerminalController};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sandboxed_spawn_stays_inside_rootfs() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let rootfs_raw = tmp.path().join("rootfs");
    std::fs::create_dir_all(&rootfs_raw).expect("mkdir rootfs");
    let rootfs = std::fs::canonicalize(&rootfs_raw).expect("canonicalize rootfs");
    let profile_path = rootfs.join("sandbox.sb");
    let outside = tmp.path().join("outside.txt");

    let params = sandbox::SbplParams {
        session_id: "s_term_test".into(),
        rootfs: rootfs.clone(),
        allowed_rw: vec![rootfs.clone()],
        allowed_ro: Vec::new(),
        network_outbound: false,
        native_ax_allowed: false,
    };
    sandbox::write_sbpl(&profile_path, &sandbox::generate_sbpl(&params)).expect("write profile");

    let controller = TerminalController::default();
    let spawned = controller
        .spawn_terminal(SpawnTerminalRequest {
            shell: "/bin/sh".to_owned(),
            cwd: Some(PathBuf::from("/")),
            env: vec![],
            rows: 24,
            cols: 80,
            sandbox: Some(SessionSandbox {
                rootfs: rootfs.clone(),
                user_data_dir: rootfs.clone(),
                profile_path: profile_path.clone(),
                seed_plan_path: rootfs.join("v_r1_seed.json"),
                inherit: vec![],
                network_outbound: false,
                native_ax_allowed: false,
                enforced: true,
            }),
        })
        .await
        .expect("spawn sandboxed terminal");

    let mut startup = Vec::new();
    for _ in 0..20 {
        let chunk = controller
            .read_output(&spawned.session_id, 4096)
            .expect("read startup output");
        if !chunk.data.is_empty() {
            startup.extend_from_slice(&chunk.data);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let command = "pwd > pwd.txt; touch inside.txt; touch ../outside.txt 2>/dev/null; exit\r";
    controller
        .write_bytes(&spawned.session_id, command.as_bytes())
        .expect("write command");

    for _ in 0..60 {
        if rootfs.join("inside.txt").exists() && rootfs.join("pwd.txt").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(rootfs.join("inside.txt").exists(), "inside file missing");
    let pwd = std::fs::read_to_string(rootfs.join("pwd.txt")).expect("read pwd file");
    assert_eq!(pwd.trim(), rootfs.to_string_lossy().as_ref());
    assert!(
        !outside.exists(),
        "sandbox should block write outside rootfs"
    );

    let exit = controller
        .close(&spawned.session_id)
        .await
        .expect("close terminal");
    assert!(exit.exited);
}
