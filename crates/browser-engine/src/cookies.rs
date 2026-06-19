//! Cookie shape conversion per SPEC §7.
//!
//! CDP returns camelCase (`httpOnly`, `sameSite`, etc); the broker exposes
//! snake_case (`http_only`, `same_site`). This module converts at the
//! engine boundary so neither side leaks the other's vocabulary.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// SPEC §7 wire shape for a single cookie.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Cookie {
    /// Cookie name.
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// Domain attribute, e.g. `.example.com`.
    pub domain: String,
    /// Path attribute.
    pub path: String,
    /// Unix-epoch seconds. `-1` means session cookie.
    pub expires: f64,
    /// Total size of name + value, in bytes.
    pub size: u64,
    /// `httpOnly` from CDP, snake_cased on our wire.
    pub http_only: bool,
    /// `secure` flag.
    pub secure: bool,
    /// True if a session cookie.
    pub session: bool,
    /// `sameSite` from CDP: "Strict", "Lax", "None", or absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CookiePartitionKey {
    pub top_level_site: String,
    pub has_cross_site_ancestor: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeepSetCookie {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub secure: Option<bool>,
    #[serde(default)]
    pub http_only: Option<bool>,
    #[serde(default)]
    pub same_site: Option<String>,
    #[serde(default)]
    pub expires: Option<f64>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub source_scheme: Option<String>,
    #[serde(default)]
    pub source_port: Option<i64>,
    #[serde(default)]
    pub partition_key: Option<CookiePartitionKey>,
}

impl DeepSetCookie {
    pub fn to_cdp_set_cookie_param(&self) -> Value {
        let mut v = json!({
            "name": self.name,
            "value": self.value,
        });
        if let Some(url) = &self.url {
            v["url"] = json!(url);
        }
        if let Some(domain) = &self.domain {
            v["domain"] = json!(domain);
        }
        if let Some(path) = &self.path {
            v["path"] = json!(path);
        }
        if let Some(secure) = self.secure {
            v["secure"] = json!(secure);
        }
        if let Some(http_only) = self.http_only {
            v["httpOnly"] = json!(http_only);
        }
        if let Some(same_site) = &self.same_site {
            v["sameSite"] = json!(same_site);
        }
        if let Some(expires) = self.expires {
            v["expires"] = json!(expires);
        }
        if let Some(priority) = &self.priority {
            v["priority"] = json!(priority);
        }
        if let Some(source_scheme) = &self.source_scheme {
            v["sourceScheme"] = json!(source_scheme);
        }
        if let Some(source_port) = self.source_port {
            v["sourcePort"] = json!(source_port);
        }
        if let Some(partition_key) = &self.partition_key {
            v["partitionKey"] = json!({
                "topLevelSite": partition_key.top_level_site,
                "hasCrossSiteAncestor": partition_key.has_cross_site_ancestor,
            });
        }
        v
    }
}

impl Cookie {
    /// Convert one CDP `Network.Cookie` value to our snake_case shape.
    pub fn from_cdp(v: &Value) -> Self {
        Self {
            name: v
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            value: v
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            domain: v
                .get("domain")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            path: v
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            expires: v.get("expires").and_then(Value::as_f64).unwrap_or(-1.0),
            size: v.get("size").and_then(Value::as_u64).unwrap_or(0),
            http_only: v.get("httpOnly").and_then(Value::as_bool).unwrap_or(false),
            secure: v.get("secure").and_then(Value::as_bool).unwrap_or(false),
            session: v.get("session").and_then(Value::as_bool).unwrap_or(true),
            same_site: v.get("sameSite").and_then(Value::as_str).map(str::to_owned),
        }
    }

    /// Convert to a `Network.CookieParam` shape suitable for
    /// `Network.setCookies`. CDP wants camelCase here too.
    pub fn to_cdp_param(&self) -> Value {
        let mut v = json!({
            "name": self.name,
            "value": self.value,
            "domain": self.domain,
            "path": self.path,
            "secure": self.secure,
            "httpOnly": self.http_only,
        });
        if self.expires >= 0.0 {
            v["expires"] = json!(self.expires);
        }
        if let Some(s) = &self.same_site {
            v["sameSite"] = json!(s);
        }
        v
    }
}

/// Convert a list of CDP cookie values to our snake_case shape.
pub fn from_cdp_list(arr: &[Value]) -> Vec<Cookie> {
    arr.iter().map(Cookie::from_cdp).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdp_camel_to_snake_round_trip() {
        let cdp = json!({
            "name": "smoke",
            "value": "abc",
            "domain": ".example.com",
            "path": "/",
            "expires": -1,
            "size": 9,
            "httpOnly": false,
            "secure": true,
            "session": true,
            "sameSite": "Lax",
        });
        let c = Cookie::from_cdp(&cdp);
        assert_eq!(c.name, "smoke");
        assert!(!c.http_only);
        assert!(c.secure);
        assert_eq!(c.same_site.as_deref(), Some("Lax"));
        // Re-encode and confirm shape.
        let s = serde_json::to_value(&c).unwrap_or(Value::Null);
        assert_eq!(s["http_only"], false);
        assert_eq!(s["same_site"], "Lax");
        assert_eq!(s["expires"], -1.0);
    }

    #[test]
    fn to_cdp_param_uses_camel_case() {
        let c = Cookie {
            name: "x".into(),
            value: "y".into(),
            domain: ".a.com".into(),
            path: "/".into(),
            expires: 1700000000.0,
            size: 0,
            http_only: true,
            secure: true,
            session: false,
            same_site: Some("Strict".into()),
        };
        let p = c.to_cdp_param();
        assert_eq!(p["httpOnly"], true);
        assert_eq!(p["sameSite"], "Strict");
        assert!(p.get("http_only").is_none());
    }

    #[test]
    fn deep_set_cookie_uses_full_cdp_shape() {
        let c = DeepSetCookie {
            name: "chip".into(),
            value: "yes".into(),
            url: Some("https://example.com/app".into()),
            domain: Some("example.com".into()),
            path: Some("/".into()),
            secure: Some(true),
            http_only: Some(true),
            same_site: Some("None".into()),
            expires: Some(1_700_000_000.0),
            priority: Some("High".into()),
            source_scheme: Some("Secure".into()),
            source_port: Some(443),
            partition_key: Some(CookiePartitionKey {
                top_level_site: "https://top.example".into(),
                has_cross_site_ancestor: true,
            }),
        };
        let p = c.to_cdp_set_cookie_param();
        assert_eq!(p["httpOnly"], true);
        assert_eq!(p["priority"], "High");
        assert_eq!(p["sourceScheme"], "Secure");
        assert_eq!(p["partitionKey"]["topLevelSite"], "https://top.example");
        assert!(p.get("http_only").is_none());
        assert!(p.get("partition_key").is_none());
    }
}
