//! File metadata helpers.

use std::process::Command;

use serde_json::{Map, Value};

use crate::types::{SystemError, SystemResult};

fn parse_mdls(stdout: &str) -> Value {
    let mut map = Map::new();
    for line in stdout.lines() {
        let Some((key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_string();
        let value = raw_value.trim();
        let json_value = if value == "(null)" {
            Value::Null
        } else if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
            Value::String(value[1..value.len() - 1].to_string())
        } else if let Ok(n) = value.parse::<i64>() {
            Value::Number(n.into())
        } else if value == "1" || value == "0" {
            Value::Bool(value == "1")
        } else {
            Value::String(value.to_string())
        };
        map.insert(key, json_value);
    }
    Value::Object(map)
}

pub fn metadata(path: &std::path::Path) -> SystemResult<Value> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/mdls")
            .arg(path)
            .output()
            .map_err(|e| SystemError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(SystemError::Subprocess(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        Ok(parse_mdls(&String::from_utf8_lossy(&output.stdout)))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Err(SystemError::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mdls_key_values() {
        let parsed = parse_mdls("kMDItemFSName = \"Cargo.toml\"\nkMDItemFSSize = 42\n");
        assert_eq!(parsed["kMDItemFSName"], "Cargo.toml");
        assert_eq!(parsed["kMDItemFSSize"], 42);
    }
}
