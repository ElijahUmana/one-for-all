//! Subprocess-driven network inventory.

use std::process::Command;

use crate::types::{NetworkConnection, NetworkInterface, NetworkRoute, SystemError, SystemResult};

pub fn interfaces() -> SystemResult<Vec<NetworkInterface>> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/sbin/ifconfig")
            .arg("-a")
            .output()
            .map_err(|e| SystemError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(SystemError::Subprocess(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut current: Option<NetworkInterface> = None;
        let mut out = Vec::new();
        for line in stdout.lines() {
            if !line.starts_with('\t') && !line.starts_with(' ') {
                if let Some(iface) = current.take() {
                    out.push(iface);
                }
                let name = line.split(':').next().unwrap_or("unknown").to_string();
                current = Some(NetworkInterface {
                    name,
                    display_name: None,
                    kind: "unknown".to_string(),
                    mac: None,
                    mtu: line
                        .split("mtu ")
                        .nth(1)
                        .and_then(|s| s.split_whitespace().next())
                        .and_then(|s| s.parse::<u32>().ok()),
                    ipv4: Vec::new(),
                    ipv6: Vec::new(),
                    is_active: line.contains("status: active") || line.contains("<UP,"),
                });
                continue;
            }
            let trimmed = line.trim();
            if let Some(iface) = current.as_mut() {
                if let Some(rest) = trimmed.strip_prefix("ether ") {
                    iface.mac = rest.split_whitespace().next().map(str::to_string);
                } else if let Some(rest) = trimmed.strip_prefix("inet ") {
                    if let Some(ip) = rest.split_whitespace().next() {
                        iface.ipv4.push(ip.to_string());
                    }
                } else if let Some(rest) = trimmed.strip_prefix("inet6 ") {
                    if let Some(ip) = rest.split_whitespace().next() {
                        iface.ipv6.push(ip.to_string());
                    }
                } else if let Some(rest) = trimmed.strip_prefix("status: ") {
                    iface.is_active = rest == "active";
                }
                if iface.kind == "unknown" {
                    iface.kind = if iface.name.starts_with("en") {
                        "ethernet".to_string()
                    } else if iface.name.starts_with("awdl") || iface.name.starts_with("llw") {
                        "wireless".to_string()
                    } else if iface.name.starts_with("utun") {
                        "tunnel".to_string()
                    } else if iface.name == "lo0" {
                        "loopback".to_string()
                    } else {
                        "other".to_string()
                    };
                }
            }
        }
        if let Some(iface) = current.take() {
            out.push(iface);
        }
        Ok(out)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(SystemError::UnsupportedPlatform)
    }
}

pub fn routes() -> SystemResult<Vec<NetworkRoute>> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/sbin/netstat")
            .args(["-rn", "-f", "inet"])
            .output()
            .map_err(|e| SystemError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(SystemError::Subprocess(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut out = Vec::new();
        let mut in_table = false;
        for line in stdout.lines() {
            if line.starts_with("Destination") {
                in_table = true;
                continue;
            }
            if !in_table || line.trim().is_empty() {
                continue;
            }
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() < 4 {
                continue;
            }
            out.push(NetworkRoute {
                destination: parts[0].to_string(),
                gateway: Some(parts[1].to_string()),
                netmask: None,
                flags: parts[2].chars().map(|c| c.to_string()).collect(),
                interface: parts.get(3).map(|s| (*s).to_string()),
            });
        }
        Ok(out)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(SystemError::UnsupportedPlatform)
    }
}

pub fn connections() -> SystemResult<Vec<NetworkConnection>> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/sbin/lsof")
            .args(["-nP", "-iTCP"])
            .output()
            .map_err(|e| SystemError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(SystemError::Subprocess(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut out = Vec::new();
        for (index, line) in stdout.lines().enumerate() {
            if index == 0 || line.trim().is_empty() {
                continue;
            }
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() < 9 {
                continue;
            }
            let name_field = parts[8..].join(" ");
            let (local, remote, state) = if let Some((lhs, rhs_state)) = name_field.split_once("->")
            {
                let state = rhs_state
                    .split_whitespace()
                    .last()
                    .unwrap_or("")
                    .trim_matches(|c| c == '(' || c == ')')
                    .to_string();
                let remote = rhs_state
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string();
                (lhs.to_string(), remote, state)
            } else {
                (name_field.clone(), String::new(), String::new())
            };
            out.push(NetworkConnection {
                pid: parts[1].parse::<i32>().unwrap_or_default(),
                command: parts[0].to_string(),
                protocol: parts[7].to_string(),
                local,
                remote,
                state,
            });
        }
        Ok(out)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(SystemError::UnsupportedPlatform)
    }
}
