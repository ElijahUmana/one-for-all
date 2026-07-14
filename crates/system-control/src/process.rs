//! Process inventory and signaling.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use nix::sys::signal::{kill as nix_kill, Signal};
use nix::unistd::Pid;

use crate::types::{ProcessInfo, ProcessSummary, SystemError, SystemResult};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn list() -> SystemResult<Vec<ProcessSummary>> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/bin/ps")
            .args(["-axo", "pid=,ppid=,uid=,comm="])
            .output()
            .map_err(|e| SystemError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(SystemError::Subprocess(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut out = Vec::new();
        for line in stdout.lines() {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() < 4 {
                continue;
            }
            out.push(ProcessSummary {
                pid: parts[0].parse::<i32>().unwrap_or_default(),
                ppid: parts[1].parse::<i32>().unwrap_or_default(),
                uid: parts[2].parse::<u32>().unwrap_or_default(),
                name: parts[3].to_string(),
                start_time_unix_ms: now_ms(),
            });
        }
        Ok(out)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(SystemError::UnsupportedPlatform)
    }
}

pub fn info(pid: i32) -> SystemResult<ProcessInfo> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/bin/ps")
            .args([
                "-p",
                &pid.to_string(),
                "-o",
                "pid=,ppid=,uid=,gid=,comm=,etime=,utime=,stime=,rss=,vsz=",
            ])
            .output()
            .map_err(|e| SystemError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(SystemError::Subprocess(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout
            .lines()
            .find(|l| !l.trim().is_empty())
            .ok_or_else(|| SystemError::NotFound(format!("pid {pid}")))?;
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() < 9 {
            return Err(SystemError::Internal(format!(
                "unexpected ps output: {line}"
            )));
        }
        Ok(ProcessInfo {
            pid: parts[0].parse::<i32>().unwrap_or_default(),
            ppid: parts[1].parse::<i32>().unwrap_or_default(),
            uid: parts[2].parse::<u32>().unwrap_or_default(),
            gid: parts[3].parse::<u32>().unwrap_or_default(),
            name: parts[4].to_string(),
            path: parts[4].to_string(),
            start_time_unix_ms: now_ms(),
            cpu_user_us: 0,
            cpu_system_us: 0,
            rss_bytes: parts[7].parse::<u64>().unwrap_or_default() * 1024,
            vsize_bytes: parts[8].parse::<u64>().unwrap_or_default() * 1024,
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        Err(SystemError::UnsupportedPlatform)
    }
}

pub fn signal(pid: i32, signal: i32) -> SystemResult<()> {
    let sig = Signal::try_from(signal)
        .map_err(|_| SystemError::InvalidArgument(format!("invalid signal {signal}")))?;
    nix_kill(Pid::from_raw(pid), sig).map_err(|e| SystemError::Os {
        domain: "kill",
        code: e as i32 as i64,
    })?;
    Ok(())
}
