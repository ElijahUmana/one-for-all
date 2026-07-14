use std::time::Duration;

use terminal_control::{SpawnTerminalRequest, TerminalController};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_write_read_snapshot_and_close_round_trip() {
    let controller = TerminalController::default();
    let spawned = controller
        .spawn_terminal(SpawnTerminalRequest {
            shell: "/bin/sh".to_owned(),
            cwd: None,
            env: vec![("TERM".to_owned(), "xterm-256color".to_owned())],
            rows: 24,
            cols: 80,
            sandbox: None,
        })
        .await
        .expect("spawn terminal");

    controller
        .write_bytes(&spawned.session_id, b"printf 'hello from pty\n'; exit\n")
        .expect("write command");

    let mut combined = Vec::new();
    for _ in 0..40 {
        let chunk = controller
            .read_output(&spawned.session_id, 4096)
            .expect("read output");
        if !chunk.data.is_empty() {
            combined.extend_from_slice(&chunk.data);
        }
        if String::from_utf8_lossy(&combined).contains("hello from pty") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let output = String::from_utf8_lossy(&combined);
    assert!(output.contains("hello from pty"), "output was: {output:?}");

    let snapshot = controller.snapshot(&spawned.session_id).expect("snapshot");
    let visible = snapshot
        .visible_rows
        .iter()
        .map(|row| row.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        visible.contains("hello from pty"),
        "snapshot visible rows: {visible:?}"
    );

    let exit = controller
        .close(&spawned.session_id)
        .await
        .expect("close terminal");
    assert!(exit.exited);
}
