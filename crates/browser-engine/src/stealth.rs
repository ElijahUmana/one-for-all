//! M3 — Anti-fingerprinting stealth bundle.
//!
//! Per SPEC §10 M3, every new context (when `stealth: true`, default) gets
//! a `Page.addScriptToEvaluateOnNewDocument` injection that patches:
//! - `navigator.webdriver` → `undefined`
//! - `navigator.plugins` → realistic populated array
//! - `navigator.languages` → from request
//! - `chrome.runtime` → present
//! - `Notification.permission` → `default`
//! - canvas / WebGL noise (deterministic per-context seed)
//! - `RTCPeerConnection` IP leak prevention (force `relay`-only)
//!
//! Returns the script source as a `String`. Apply via:
//!
//! ```ignore
//! page.cdp_call(
//!     "Page.addScriptToEvaluateOnNewDocument",
//!     Some(json!({"source": stealth::script(seed)})),
//! ).await?;
//! ```

/// Build the stealth script. `seed` keys the canvas/WebGL noise so a given
/// context produces consistent fingerprints across reloads.
pub fn script(seed: u64) -> String {
    // The script is one big self-executing function so it cannot be
    // observed mid-patch. Every patch is tolerant of multiple invocations
    // because Chromium re-evaluates `addScriptToEvaluateOnNewDocument`
    // scripts on every navigation.
    format!(
        r#"(() => {{
  try {{
    // navigator.webdriver
    Object.defineProperty(Navigator.prototype, 'webdriver', {{ get: () => undefined }});

    // navigator.plugins — realistic 3-entry array
    const plugins = [
      {{ name: 'PDF Viewer', filename: 'internal-pdf-viewer', description: 'Portable Document Format' }},
      {{ name: 'Chrome PDF Viewer', filename: 'internal-pdf-viewer', description: '' }},
      {{ name: 'Chromium PDF Viewer', filename: 'internal-pdf-viewer', description: '' }},
    ];
    Object.defineProperty(Navigator.prototype, 'plugins', {{
      get: () => Object.assign(plugins.slice(), {{
        item: (i) => plugins[i] || null,
        namedItem: (n) => plugins.find(p => p.name === n) || null,
        refresh: () => undefined,
        length: plugins.length,
      }}),
    }});

    // navigator.languages — fall back to navigator.language if missing
    if (!navigator.languages || navigator.languages.length === 0) {{
      Object.defineProperty(Navigator.prototype, 'languages', {{
        get: () => [navigator.language || 'en-US', 'en'],
      }});
    }}

    // chrome.runtime presence
    if (!window.chrome) {{
      Object.defineProperty(window, 'chrome', {{ value: {{ runtime: {{}} }}, configurable: true }});
    }} else if (!window.chrome.runtime) {{
      Object.defineProperty(window.chrome, 'runtime', {{ value: {{}}, configurable: true }});
    }}

    // Notification.permission — return 'default' for non-https/localhost
    const origGetNotif = Object.getOwnPropertyDescriptor(Notification, 'permission');
    if (origGetNotif && origGetNotif.get) {{
      Object.defineProperty(Notification, 'permission', {{
        get: () => {{
          const orig = origGetNotif.get.call(Notification);
          if (orig === 'denied' && location.protocol !== 'https:' && location.hostname !== 'localhost') {{
            return 'default';
          }}
          return orig;
        }},
      }});
    }}

    // Canvas noise — deterministic per-context seed.
    const seed = {seed}n;
    let rng = seed === 0n ? 0x12345n : seed;
    const nextByte = () => {{
      // xorshift64*
      let x = rng;
      x ^= x >> 12n;
      x ^= x << 25n & 0xFFFFFFFFFFFFFFFFn;
      x ^= x >> 27n;
      rng = x & 0xFFFFFFFFFFFFFFFFn;
      return Number((rng * 0x2545F4914F6CDD1Dn) & 0xFFn);
    }};

    const origToDataURL = HTMLCanvasElement.prototype.toDataURL;
    HTMLCanvasElement.prototype.toDataURL = function(...args) {{
      try {{
        const ctx = this.getContext('2d');
        if (ctx && this.width > 0 && this.height > 0) {{
          const data = ctx.getImageData(0, 0, this.width, this.height);
          for (let i = 0; i < data.data.length; i += 4) {{
            data.data[i] = data.data[i] ^ (nextByte() & 0x03);
          }}
          ctx.putImageData(data, 0, 0);
        }}
      }} catch (_) {{}}
      return origToDataURL.apply(this, args);
    }};

    // WebGL — vendor / renderer noise.
    const getParameterPatched = function(orig) {{
      return function(parameter) {{
        if (parameter === 37445) return 'Apple Inc.';        // UNMASKED_VENDOR_WEBGL
        if (parameter === 37446) return 'Apple GPU';         // UNMASKED_RENDERER_WEBGL
        return orig.call(this, parameter);
      }};
    }};
    if (window.WebGLRenderingContext) {{
      const orig = WebGLRenderingContext.prototype.getParameter;
      WebGLRenderingContext.prototype.getParameter = getParameterPatched(orig);
    }}
    if (window.WebGL2RenderingContext) {{
      const orig = WebGL2RenderingContext.prototype.getParameter;
      WebGL2RenderingContext.prototype.getParameter = getParameterPatched(orig);
    }}

    // RTCPeerConnection — block IP leak via relay-only enforcement.
    if (window.RTCPeerConnection) {{
      const Orig = window.RTCPeerConnection;
      window.RTCPeerConnection = function(config, ...rest) {{
        const cfg = config || {{}};
        cfg.iceTransportPolicy = 'relay';
        return new Orig(cfg, ...rest);
      }};
      window.RTCPeerConnection.prototype = Orig.prototype;
    }}
  }} catch (e) {{
    // Stealth must never break a page even if the patch fails.
    console.debug('[one-for-all stealth] patch error (non-fatal):', e);
  }}
}})();"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_is_self_invoking_and_seeded() {
        let s = script(0xDEAD_BEEF);
        assert!(s.starts_with("(() => {"));
        assert!(s.contains("navigator.webdriver"));
        assert!(s.contains("0xDEADBEEFn") || s.contains("3735928559n"));
        assert!(s.contains("RTCPeerConnection"));
        assert!(s.contains("WebGLRenderingContext"));
    }

    #[test]
    fn different_seeds_produce_different_scripts() {
        let a = script(1);
        let b = script(2);
        assert_ne!(a, b);
    }
}
