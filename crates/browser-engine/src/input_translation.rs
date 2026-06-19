//! Realistic input dispatch for `page.click`, `page.type`, etc.
//!
//! - Mouse paths: cubic Bezier from current to target with ~20 samples and
//!   5–15 ms per-step jitter. The `realistic` flag on `page.click` toggles
//!   this; default-off issues a single direct mouseMoved-then-press for
//!   speed.
//! - Key sequences: per-character Input.dispatchKeyEvent triplets
//!   (rawKeyDown → char → keyUp) with 30–80 ms inter-key jitter.
//!
//! Determinism: every randomized helper accepts an injectable `Rng`. Tests
//! pass a fixed seed.

use std::time::Duration;

use cdp_client::generated::domains::input as cdp_input;
use serde_json::{json, Value};

/// Tiny deterministic xorshift64* RNG. Avoids pulling in `rand`.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Avoid zero state.
        Self(if seed == 0 { 0x12345 } else { seed })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn range_f64(&mut self, lo: f64, hi: f64) -> f64 {
        let unit = (self.next_u64() as f64) / (u64::MAX as f64);
        lo + unit * (hi - lo)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub fn cdp_str(self) -> &'static str {
        match self {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
        }
    }

    /// CDP `buttons` bitmask.
    pub fn buttons_mask(self) -> i64 {
        match self {
            MouseButton::Left => 1,
            MouseButton::Right => 2,
            MouseButton::Middle => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollEasing {
    Linear,
    EaseOut,
    EaseInOut,
}

impl ScrollEasing {
    fn weight(self, t: f64) -> f64 {
        match self {
            ScrollEasing::Linear => 1.0,
            ScrollEasing::EaseOut => 1.0 - (1.0 - t).powi(2),
            ScrollEasing::EaseInOut => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    1.0 - ((-2.0 * t + 2.0).powi(2) / 2.0)
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WheelStep {
    pub dx: f64,
    pub dy: f64,
    pub sleep_ms: u64,
}

pub fn wheel_profile(dx: f64, dy: f64, momentum: bool, easing: ScrollEasing) -> Vec<WheelStep> {
    if dx == 0.0 && dy == 0.0 {
        return vec![];
    }

    let base_steps = if momentum { 12 } else { 6 };
    let tail_steps = if momentum { 4 } else { 0 };
    let total_steps = base_steps + tail_steps;

    let mut weights = Vec::with_capacity(total_steps);
    for idx in 0..base_steps {
        let t = (idx + 1) as f64 / base_steps as f64;
        weights.push(easing.weight(t).max(0.01));
    }
    if momentum {
        let base_tail = *weights.last().unwrap_or(&1.0);
        for idx in 0..tail_steps {
            let decay = 0.55_f64.powi((idx + 1) as i32);
            weights.push((base_tail * decay).max(0.01));
        }
    }

    let total_weight = weights.iter().sum::<f64>().max(0.000_001);
    let mut remaining_dx = dx;
    let mut remaining_dy = dy;
    let mut out = Vec::with_capacity(total_steps);
    for (idx, weight) in weights.iter().enumerate() {
        let is_last = idx + 1 == weights.len();
        let step_dx = if is_last {
            remaining_dx
        } else {
            dx * (*weight / total_weight)
        };
        let step_dy = if is_last {
            remaining_dy
        } else {
            dy * (*weight / total_weight)
        };
        remaining_dx -= step_dx;
        remaining_dy -= step_dy;
        out.push(WheelStep {
            dx: step_dx,
            dy: step_dy,
            sleep_ms: if momentum && idx >= base_steps { 12 } else { 8 },
        });
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct TouchContact {
    pub id: i64,
    pub x: f64,
    pub y: f64,
    pub radius_x: f64,
    pub radius_y: f64,
    pub rotation_angle: f64,
    pub force: f64,
    pub tangential_pressure: f64,
    pub tilt_x: f64,
    pub tilt_y: f64,
    pub twist: i64,
}

impl TouchContact {
    pub fn new(id: i64, x: f64, y: f64) -> Self {
        Self {
            id,
            x,
            y,
            radius_x: 1.0,
            radius_y: 1.0,
            rotation_angle: 0.0,
            force: 1.0,
            tangential_pressure: 0.0,
            tilt_x: 0.0,
            tilt_y: 0.0,
            twist: 0,
        }
    }
}

pub fn touch_contact_value(contact: &TouchContact) -> Value {
    json!({
        "id": contact.id,
        "x": contact.x,
        "y": contact.y,
        "radiusX": contact.radius_x,
        "radiusY": contact.radius_y,
        "rotationAngle": contact.rotation_angle,
        "force": contact.force,
        "tangentialPressure": contact.tangential_pressure,
        "tiltX": contact.tilt_x,
        "tiltY": contact.tilt_y,
        "twist": contact.twist,
    })
}

pub fn cdp_touch_event(
    event_type: &str,
    contacts: &[TouchContact],
) -> cdp_input::DispatchTouchEventParams {
    cdp_input::DispatchTouchEventParams {
        r#type: event_type.to_owned(),
        touch_points: Value::Array(contacts.iter().map(touch_contact_value).collect()),
        ..Default::default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PointerDetails {
    pub force: Option<f64>,
    pub tangential_pressure: Option<f64>,
    pub tilt_x: Option<f64>,
    pub tilt_y: Option<f64>,
    pub twist: Option<i64>,
}

pub fn cdp_pointer_event(
    event_type: &str,
    x: f64,
    y: f64,
    button: Option<MouseButton>,
    buttons: i64,
    click_count: Option<u32>,
    pointer: PointerDetails,
) -> cdp_input::DispatchMouseEventParams {
    cdp_input::DispatchMouseEventParams {
        r#type: event_type.to_owned(),
        x,
        y,
        button: Some(serde_json::Value::String(
            button.map_or("none", MouseButton::cdp_str).to_owned(),
        )),
        buttons: Some(buttons),
        click_count: click_count.map(|c| c as i64),
        force: pointer.force,
        tangential_pressure: pointer.tangential_pressure,
        tilt_x: pointer.tilt_x,
        tilt_y: pointer.tilt_y,
        twist: pointer.twist,
        pointer_type: Some("pen".to_owned()),
        ..Default::default()
    }
}

pub fn build_file_drag_data(file_paths: &[String]) -> Value {
    let file_names: Vec<String> = file_paths
        .iter()
        .map(|path| {
            std::path::Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone())
        })
        .collect();
    json!({
        "items": [{
            "mimeType": "text/plain",
            "data": file_names.join("\n"),
        }],
        "files": file_paths,
        "dragOperationsMask": 1,
    })
}

/// Sample a cubic Bezier mouse path from `from` to `to`. Returns a vector
/// of `(x, y, sleep_ms)` tuples. Two random control points are placed
/// perpendicular to the line, magnitude proportional to distance.
pub fn bezier_mouse_path(
    from: (f64, f64),
    to: (f64, f64),
    samples: u32,
    rng: &mut Rng,
) -> Vec<(f64, f64, u32)> {
    if samples == 0 {
        return vec![(to.0, to.1, 0)];
    }
    let (x0, y0) = from;
    let (x3, y3) = to;
    let dx = x3 - x0;
    let dy = y3 - y0;
    let dist = (dx * dx + dy * dy).sqrt().max(1.0);
    // Unit perpendicular.
    let (px, py) = (-dy / dist, dx / dist);
    let mag = dist * 0.18;

    let off1 = rng.range_f64(-mag, mag);
    let off2 = rng.range_f64(-mag, mag);
    let cx1 = x0 + dx * (1.0 / 3.0) + px * off1;
    let cy1 = y0 + dy * (1.0 / 3.0) + py * off1;
    let cx2 = x0 + dx * (2.0 / 3.0) + px * off2;
    let cy2 = y0 + dy * (2.0 / 3.0) + py * off2;

    let mut out = Vec::with_capacity(samples as usize);
    for i in 1..=samples {
        let t = i as f64 / samples as f64;
        let omt = 1.0 - t;
        let x = omt.powi(3) * x0
            + 3.0 * omt.powi(2) * t * cx1
            + 3.0 * omt * t.powi(2) * cx2
            + t.powi(3) * x3;
        let y = omt.powi(3) * y0
            + 3.0 * omt.powi(2) * t * cy1
            + 3.0 * omt * t.powi(2) * cy2
            + t.powi(3) * y3;
        let sleep_ms = rng.range_f64(5.0, 15.0).round() as u32;
        out.push((x, y, sleep_ms));
    }
    out
}

/// Build typed `Input.dispatchMouseEvent` params for a single move.
pub fn cdp_mouse_move(x: f64, y: f64) -> cdp_input::DispatchMouseEventParams {
    cdp_input::DispatchMouseEventParams {
        r#type: "mouseMoved".to_owned(),
        x,
        y,
        button: Some(serde_json::Value::String("none".to_owned())),
        buttons: Some(0),
        ..Default::default()
    }
}

/// Build typed `Input.dispatchMouseEvent` params for a wheel scroll.
pub fn cdp_mouse_wheel(x: f64, y: f64, dx: f64, dy: f64) -> cdp_input::DispatchMouseEventParams {
    cdp_input::DispatchMouseEventParams {
        r#type: "mouseWheel".to_owned(),
        x,
        y,
        delta_x: Some(dx),
        delta_y: Some(dy),
        ..Default::default()
    }
}

/// Build typed `Input.dispatchMouseEvent` params for press/release.
pub fn cdp_mouse_press(
    x: f64,
    y: f64,
    button: MouseButton,
    click_count: u32,
    down: bool,
) -> cdp_input::DispatchMouseEventParams {
    cdp_input::DispatchMouseEventParams {
        r#type: if down {
            "mousePressed".to_owned()
        } else {
            "mouseReleased".to_owned()
        },
        x,
        y,
        button: Some(serde_json::Value::String(button.cdp_str().to_owned())),
        buttons: Some(button.buttons_mask()),
        click_count: Some(click_count as i64),
        ..Default::default()
    }
}

/// Build typed `Input.dispatchKeyEvent` params for a keyDown.
pub fn cdp_key_down(
    key: &str,
    code: &str,
    text: Option<&str>,
    modifiers: i64,
) -> cdp_input::DispatchKeyEventParams {
    cdp_input::DispatchKeyEventParams {
        r#type: "keyDown".to_owned(),
        key: Some(key.to_owned()),
        code: Some(code.to_owned()),
        modifiers: Some(modifiers),
        text: text.map(str::to_owned),
        unmodified_text: text.map(str::to_owned),
        ..Default::default()
    }
}

/// Build typed `Input.dispatchKeyEvent` params for a raw keyDown.
pub fn cdp_key_raw_down(
    key: &str,
    code: &str,
    modifiers: i64,
) -> cdp_input::DispatchKeyEventParams {
    cdp_input::DispatchKeyEventParams {
        r#type: "rawKeyDown".to_owned(),
        key: Some(key.to_owned()),
        code: Some(code.to_owned()),
        modifiers: Some(modifiers),
        ..Default::default()
    }
}

/// Build typed `Input.dispatchKeyEvent` params for a keyUp.
pub fn cdp_key_up(key: &str, code: &str, modifiers: i64) -> cdp_input::DispatchKeyEventParams {
    cdp_input::DispatchKeyEventParams {
        r#type: "keyUp".to_owned(),
        key: Some(key.to_owned()),
        code: Some(code.to_owned()),
        modifiers: Some(modifiers),
        ..Default::default()
    }
}

fn parse_modifier_name(name: &str) -> Option<i64> {
    match name.trim().to_ascii_lowercase().as_str() {
        "alt" | "option" => Some(1),
        "ctrl" | "control" => Some(2),
        "cmd" | "meta" | "command" => Some(4),
        "shift" => Some(8),
        _ => None,
    }
}

pub fn parse_modifier_list<'a, I>(modifiers: I) -> Result<i64, String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut bits = 0_i64;
    for modifier in modifiers {
        let bit = parse_modifier_name(modifier)
            .ok_or_else(|| format!("unsupported modifier {modifier:?}"))?;
        bits |= bit;
    }
    Ok(bits)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAccelerator {
    pub modifiers: i64,
    pub key: String,
    pub code: String,
}

pub fn key_code_for_token(token: &str) -> Result<(String, String), String> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err("missing key token".to_owned());
    }
    if trimmed.len() == 1 {
        let ch = trimmed
            .chars()
            .next()
            .ok_or_else(|| "missing key token".to_owned())?;
        let (key, code, _) = char_to_cdp_key(ch);
        return Ok((key, code));
    }

    let lower = trimmed.to_ascii_lowercase();
    let named = match lower.as_str() {
        "enter" => Some(("Enter", "Enter")),
        "tab" => Some(("Tab", "Tab")),
        "escape" | "esc" => Some(("Escape", "Escape")),
        "backspace" => Some(("Backspace", "Backspace")),
        "delete" => Some(("Delete", "Delete")),
        "arrowup" | "up" => Some(("ArrowUp", "ArrowUp")),
        "arrowdown" | "down" => Some(("ArrowDown", "ArrowDown")),
        "arrowleft" | "left" => Some(("ArrowLeft", "ArrowLeft")),
        "arrowright" | "right" => Some(("ArrowRight", "ArrowRight")),
        "home" => Some(("Home", "Home")),
        "end" => Some(("End", "End")),
        "pageup" => Some(("PageUp", "PageUp")),
        "pagedown" => Some(("PageDown", "PageDown")),
        "space" => Some((" ", "Space")),
        _ => None,
    };
    if let Some((key, code)) = named {
        return Ok((key.to_owned(), code.to_owned()));
    }

    if let Some(rest) = lower.strip_prefix('f') {
        if let Ok(idx) = rest.parse::<u8>() {
            if (1..=12).contains(&idx) {
                let label = format!("F{idx}");
                return Ok((label.clone(), label));
            }
        }
    }

    Err(format!("unsupported key token {trimmed:?}"))
}

pub fn parse_accelerator(accel: &str) -> Result<ParsedAccelerator, String> {
    let tokens: Vec<&str> = accel
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if tokens.is_empty() {
        return Err("missing accelerator".to_owned());
    }
    let (modifier_tokens, key_token) = tokens.split_at(tokens.len() - 1);
    let modifiers = parse_modifier_list(modifier_tokens.iter().copied())?;
    let (key, code) = key_code_for_token(
        key_token
            .first()
            .copied()
            .ok_or_else(|| "missing accelerator key".to_owned())?,
    )?;
    Ok(ParsedAccelerator {
        modifiers,
        key,
        code,
    })
}

/// Map a single character to a `(key, code, text)` triple suitable for CDP.
/// Handles only ASCII printables + a small set of named keys; complex
/// multi-byte/IME input is out of scope for v1 (caller can use
/// `Input.insertText`).
pub fn char_to_cdp_key(ch: char) -> (String, String, String) {
    let key = match ch {
        '\n' => "Enter".to_owned(),
        '\t' => "Tab".to_owned(),
        '\u{0008}' => "Backspace".to_owned(),
        '\u{007F}' => "Delete".to_owned(),
        c => c.to_string(),
    };
    let code = match ch {
        '\n' => "Enter".to_owned(),
        '\t' => "Tab".to_owned(),
        '\u{0008}' => "Backspace".to_owned(),
        c if c.is_ascii_alphabetic() => format!("Key{}", c.to_ascii_uppercase()),
        c if c.is_ascii_digit() => format!("Digit{c}"),
        ' ' => "Space".to_owned(),
        '.' => "Period".to_owned(),
        ',' => "Comma".to_owned(),
        '/' => "Slash".to_owned(),
        ';' => "Semicolon".to_owned(),
        '\'' => "Quote".to_owned(),
        '`' => "Backquote".to_owned(),
        '-' => "Minus".to_owned(),
        '=' => "Equal".to_owned(),
        _ => "Unidentified".to_owned(),
    };
    let text = match ch {
        '\n' => "\r".to_owned(),
        '\t' => "\t".to_owned(),
        '\u{0008}' | '\u{007F}' => String::new(),
        c => c.to_string(),
    };
    (key, code, text)
}

pub fn dead_key_accent_hint(accent: &str) -> Option<ParsedAccelerator> {
    let accel = match accent.trim().to_ascii_lowercase().as_str() {
        "acute" => "Option+e",
        "grave" => "Option+`",
        "circumflex" => "Option+i",
        "tilde" => "Option+n",
        "diaeresis" | "umlaut" => "Option+u",
        "ring" => "Option+k",
        _ => return None,
    };
    parse_accelerator(accel).ok()
}

pub fn compose_dead_key(accent: &str, base: char) -> Option<String> {
    let composed = match accent.trim().to_ascii_lowercase().as_str() {
        "acute" => match base {
            'a' => 'á',
            'e' => 'é',
            'i' => 'í',
            'o' => 'ó',
            'u' => 'ú',
            'y' => 'ý',
            'A' => 'Á',
            'E' => 'É',
            'I' => 'Í',
            'O' => 'Ó',
            'U' => 'Ú',
            'Y' => 'Ý',
            'c' => 'ć',
            'C' => 'Ć',
            'n' => 'ń',
            'N' => 'Ń',
            _ => return None,
        },
        "grave" => match base {
            'a' => 'à',
            'e' => 'è',
            'i' => 'ì',
            'o' => 'ò',
            'u' => 'ù',
            'A' => 'À',
            'E' => 'È',
            'I' => 'Ì',
            'O' => 'Ò',
            'U' => 'Ù',
            _ => return None,
        },
        "circumflex" => match base {
            'a' => 'â',
            'e' => 'ê',
            'i' => 'î',
            'o' => 'ô',
            'u' => 'û',
            'A' => 'Â',
            'E' => 'Ê',
            'I' => 'Î',
            'O' => 'Ô',
            'U' => 'Û',
            _ => return None,
        },
        "tilde" => match base {
            'a' => 'ã',
            'n' => 'ñ',
            'o' => 'õ',
            'A' => 'Ã',
            'N' => 'Ñ',
            'O' => 'Õ',
            _ => return None,
        },
        "diaeresis" | "umlaut" => match base {
            'a' => 'ä',
            'e' => 'ë',
            'i' => 'ï',
            'o' => 'ö',
            'u' => 'ü',
            'y' => 'ÿ',
            'A' => 'Ä',
            'E' => 'Ë',
            'I' => 'Ï',
            'O' => 'Ö',
            'U' => 'Ü',
            _ => return None,
        },
        "cedilla" => match base {
            'c' => 'ç',
            'C' => 'Ç',
            _ => return None,
        },
        "ring" => match base {
            'a' => 'å',
            'A' => 'Å',
            _ => return None,
        },
        _ => return None,
    };
    Some(composed.to_string())
}

/// Per-keystroke jitter window. Public so `actions` can consume it.
pub const TYPE_DELAY_MIN_MS: f64 = 30.0;
pub const TYPE_DELAY_MAX_MS: f64 = 80.0;

pub fn typing_delay(rng: &mut Rng) -> Duration {
    let ms = rng.range_f64(TYPE_DELAY_MIN_MS, TYPE_DELAY_MAX_MS);
    Duration::from_millis(ms as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..32 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn bezier_emits_expected_count_and_endpoint() {
        let mut rng = Rng::new(7);
        let path = bezier_mouse_path((0.0, 0.0), (200.0, 100.0), 16, &mut rng);
        assert_eq!(path.len(), 16);
        let last = path
            .last()
            .expect("16 samples requested → at least one element");
        assert!((last.0 - 200.0).abs() < 0.001);
        assert!((last.1 - 100.0).abs() < 0.001);
    }

    #[test]
    fn char_mapping_letter_and_digit() {
        let (k, c, t) = char_to_cdp_key('A');
        assert_eq!(k, "A");
        assert_eq!(c, "KeyA");
        assert_eq!(t, "A");

        let (k, c, t) = char_to_cdp_key('7');
        assert_eq!(k, "7");
        assert_eq!(c, "Digit7");
        assert_eq!(t, "7");
    }

    #[test]
    fn char_mapping_enter() {
        let (k, c, t) = char_to_cdp_key('\n');
        assert_eq!(k, "Enter");
        assert_eq!(c, "Enter");
        assert_eq!(t, "\r");
    }

    #[test]
    fn parse_accelerator_supports_common_chords() {
        let parsed = parse_accelerator("Cmd+Shift+K").expect("accelerator");
        assert_eq!(parsed.modifiers, 12);
        assert_eq!(parsed.key, "K");
        assert_eq!(parsed.code, "KeyK");

        let parsed = parse_accelerator("Option+`").expect("accent");
        assert_eq!(parsed.modifiers, 1);
        assert_eq!(parsed.code, "Backquote");
    }

    #[test]
    fn wheel_profile_preserves_total_delta() {
        let profile = wheel_profile(120.0, -240.0, true, ScrollEasing::EaseOut);
        let total_dx: f64 = profile.iter().map(|step| step.dx).sum();
        let total_dy: f64 = profile.iter().map(|step| step.dy).sum();
        assert!((total_dx - 120.0).abs() < 0.000_1, "dx={total_dx}");
        assert!((total_dy + 240.0).abs() < 0.000_1, "dy={total_dy}");
        assert!(profile.len() > 6);
    }

    #[test]
    fn dead_key_composition_maps_common_accents() {
        assert_eq!(compose_dead_key("acute", 'e').as_deref(), Some("é"));
        assert_eq!(compose_dead_key("tilde", 'N').as_deref(), Some("Ñ"));
        assert_eq!(compose_dead_key("cedilla", 'c').as_deref(), Some("ç"));
        assert!(compose_dead_key("acute", 'z').is_none());
    }

    #[test]
    fn cdp_touch_event_serializes_all_contacts() {
        let params = cdp_touch_event(
            "touchMove",
            &[
                TouchContact::new(1, 10.5, 20.25),
                TouchContact::new(2, 30.75, 40.125),
            ],
        );
        assert_eq!(params.r#type, "touchMove");
        let points = params
            .touch_points
            .as_array()
            .expect("touch points serialize to an array");
        assert_eq!(points.len(), 2);
        assert_eq!(points[0]["id"], 1);
        assert_eq!(points[0]["x"], 10.5);
        assert_eq!(points[0]["y"], 20.25);
        assert_eq!(points[1]["id"], 2);
        assert_eq!(points[1]["x"], 30.75);
        assert_eq!(points[1]["y"], 40.125);
    }

    #[test]
    fn cdp_pointer_event_carries_pen_details() {
        let params = cdp_pointer_event(
            "mousePressed",
            12.0,
            34.0,
            Some(MouseButton::Right),
            MouseButton::Right.buttons_mask(),
            Some(2),
            PointerDetails {
                force: Some(0.7),
                tangential_pressure: Some(-0.25),
                tilt_x: Some(15.0),
                tilt_y: Some(-10.0),
                twist: Some(270),
            },
        );
        assert_eq!(params.r#type, "mousePressed");
        assert_eq!(params.button, Some(Value::String("right".to_owned())));
        assert_eq!(params.buttons, Some(2));
        assert_eq!(params.click_count, Some(2));
        assert_eq!(params.force, Some(0.7));
        assert_eq!(params.tangential_pressure, Some(-0.25));
        assert_eq!(params.tilt_x, Some(15.0));
        assert_eq!(params.tilt_y, Some(-10.0));
        assert_eq!(params.twist, Some(270));
        assert_eq!(params.pointer_type.as_deref(), Some("pen"));
    }
}
