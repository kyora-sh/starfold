//! `POST /input`: the mouse and keyboard for agents and scripts, as JSON. Each action
//! becomes the input events the web client's socket sends, on the same input thread.
//!
//! The body is one action or an array of them, applied in order:
//!
//! - `{"type":"move","x":10,"y":20}`
//! - `{"type":"click","x":10,"y":20,"button":"left","count":2}`: `x`/`y` move first
//!   (optional, as for `down`, `up` and `scroll`), `button` is left (default), middle,
//!   right, back, forward or an X button number, `count` defaults to 1.
//! - `{"type":"down","button":"left"}` and `{"type":"up"}`: press or release a button.
//! - `{"type":"scroll","dx":0,"dy":3}`: wheel notches, positive is down and right.
//! - `{"type":"key","key":"ctrl+shift+t"}`: press keys together, then release them;
//!   `key_down` and `key_up` only press or only release. Keys are named (Enter, Tab,
//!   Escape, BackSpace, Delete, Home, End, PageUp, PageDown, Up, Down, Left, Right,
//!   Insert, Space, F1-F24, ctrl, shift, alt, altgr, super, ...) or a single character.
//!   After modifiers, a letter's case follows Shift (`ctrl+T` is Ctrl+t, `ctrl+shift+t`
//!   is Ctrl+Shift+T), and with Shift a digit or punctuation key is its shifted symbol on
//!   a US layout (`cmd+shift+4` is Cmd+$), which the desktop presses as Shift+4.
//! - `{"type":"type","text":"hello\n"}`: types the text, newlines as Enter.
//! - `{"type":"pause","ms":50}`: waits before the next action.
//!
//! The answer comes once every event has reached the desktop (a pause, too, starts once
//! the events before it have): `{"seq":n,"events":m}`, where `seq` is the screen's change
//! count from before the first event went out, so `/wait?change&since=n` returns once the
//! actions changed the screen. A request runs to the end even if its client goes away, so
//! it never leaves a key or button pressed halfway. A --read-only server refuses input
//! with 403.

use crate::input::InputEvent;
use crate::server::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

const MAX_ACTIONS: usize = 1000;
const MAX_TEXT: usize = 10_000;
const MAX_CLICKS: u8 = 10;
const MAX_KEYS: usize = 8;
const MAX_PAUSE: Duration = Duration::from_secs(10);
const MAX_TOTAL_PAUSE: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Move {
        x: i32,
        y: i32,
    },
    Click {
        x: Option<i32>,
        y: Option<i32>,
        button: Option<Value>,
        count: Option<u8>,
    },
    Down {
        x: Option<i32>,
        y: Option<i32>,
        button: Option<Value>,
    },
    Up {
        x: Option<i32>,
        y: Option<i32>,
        button: Option<Value>,
    },
    Scroll {
        x: Option<i32>,
        y: Option<i32>,
        #[serde(default)]
        dx: i32,
        #[serde(default)]
        dy: i32,
    },
    Key {
        key: String,
    },
    KeyDown {
        key: String,
    },
    KeyUp {
        key: String,
    },
    Type {
        text: String,
    },
    Pause {
        ms: u64,
    },
}

/// The X button number of `left`, `right`, ... or 1-15; left when not given.
fn button(b: Option<&Value>) -> Result<u8, String> {
    let n = match b {
        None => return Ok(1),
        Some(Value::String(name)) => match name.to_ascii_lowercase().as_str() {
            "left" => 1,
            "middle" => 2,
            "right" => 3,
            "back" => 8,
            "forward" => 9,
            _ => return Err(format!("unknown button {name:?}")),
        },
        Some(v) => v.as_u64().unwrap_or(0),
    };
    if (1..=15).contains(&n) {
        Ok(n as u8)
    } else {
        Err("button must be left, middle, right, back, forward or 1-15".into())
    }
}

/// What a request does, in order.
#[derive(Debug, PartialEq)]
enum Step {
    Event(Ev),
    Pause(Duration),
}

/// The input events actions turn into.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Ev {
    Move(i32, i32),
    Button(u8, bool),
    Scroll(i32, i32),
    Key(u32, bool),
}

impl Ev {
    fn event(self) -> InputEvent {
        match self {
            Ev::Move(x, y) => InputEvent::Move { x, y },
            Ev::Button(button, down) => InputEvent::Button { button, down },
            Ev::Scroll(dx, dy) => InputEvent::Scroll { dx, dy },
            Ev::Key(keysym, down) => InputEvent::Key { keysym, down },
        }
    }
}

/// Keysym of a character, as the web client sends it (keyboard.ts): Latin-1 directly,
/// the rest of Unicode at 0x01000000 + its code point; newline and tab as Enter and Tab.
fn char_keysym(c: char) -> Option<u32> {
    let cp = c as u32;
    match c {
        '\n' => Some(0xff0d),
        '\t' => Some(0xff09),
        c if c.is_control() => None,
        _ if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) => Some(cp),
        _ => Some(0x0100_0000 + cp),
    }
}

fn named_keysym(name: &str) -> Option<u32> {
    let n = name.to_ascii_lowercase().replace('_', "");
    let ks = match n.as_str() {
        "backspace" => 0xff08,
        "tab" => 0xff09,
        "enter" | "return" => 0xff0d,
        "escape" | "esc" => 0xff1b,
        "delete" | "del" => 0xffff,
        "home" => 0xff50,
        "left" | "arrowleft" => 0xff51,
        "up" | "arrowup" => 0xff52,
        "right" | "arrowright" => 0xff53,
        "down" | "arrowdown" => 0xff54,
        "pageup" | "pgup" | "prior" => 0xff55,
        "pagedown" | "pgdn" | "next" => 0xff56,
        "end" => 0xff57,
        "insert" | "ins" => 0xff63,
        "capslock" => 0xffe5,
        "numlock" => 0xff7f,
        "scrolllock" => 0xff14,
        "pause" => 0xff13,
        "printscreen" | "print" => 0xff61,
        "menu" | "contextmenu" => 0xff67,
        "space" => 0x20,
        "plus" => 0x2b,
        "shift" | "shiftl" => 0xffe1,
        "shiftr" => 0xffe2,
        "ctrl" | "control" | "controll" => 0xffe3,
        "controlr" => 0xffe4,
        "alt" | "altl" | "option" => 0xffe9,
        "altr" => 0xffea,
        "altgr" | "altgraph" => 0xfe03,
        // The web client sends Command as Super; the Mac backend takes Super and Meta.
        "super" | "superl" | "meta" | "cmd" | "command" | "win" => 0xffeb,
        "superr" => 0xffec,
        _ => {
            let f: u32 = n.strip_prefix('f')?.parse().ok()?;
            return (1..=24).contains(&f).then_some(0xffbd + f);
        }
    };
    Some(ks)
}

fn keysym(name: &str) -> Result<u32, String> {
    let mut chars = name.chars();
    let found = match (chars.next(), chars.next()) {
        (Some(c), None) => char_keysym(c),
        _ => named_keysym(name),
    };
    found.ok_or_else(|| format!("unknown key {name:?}"))
}

/// "ctrl+shift+t" -> its keysyms in order. "+" and "ctrl++" name the plus key.
fn combo(s: &str) -> Result<Vec<u32>, String> {
    if s.split('+').count() > MAX_KEYS + 1 {
        return Err(format!("at most {MAX_KEYS} keys together"));
    }
    let (mods, last) = match s.strip_suffix("++") {
        Some(m) => (m, "+"),
        None if s == "+" => ("", "+"),
        None => s.rsplit_once('+').unwrap_or(("", s)),
    };
    let mods = mods.split('+').filter(|_| !mods.is_empty());
    let mut keys: Vec<u32> = mods
        .chain([last])
        .map(|part| match part {
            "" => Err(format!("key {s:?} has an empty part")),
            part => keysym(part),
        })
        .collect::<Result<_, _>>()?;
    if keys.len() > MAX_KEYS {
        return Err(format!("at most {MAX_KEYS} keys together"));
    }
    // The input threads press a character's own level, lifting a held Shift for an
    // unshifted one (as the web client sends the character Shift made), so the
    // character has to match the modifiers.
    if let [held @ .., k] = &mut keys[..] {
        let shift = held.iter().any(|h| matches!(h, 0xffe1 | 0xffe2));
        match char::from_u32(*k) {
            Some(c) if !held.is_empty() && c.is_ascii_alphabetic() => {
                *k = if shift { *k & !0x20 } else { *k | 0x20 };
            }
            Some(c) if shift => {
                if let Some(i) = US_PLAIN.find(c) {
                    *k = US_SHIFTED.as_bytes()[i] as u32;
                }
            }
            _ => {}
        }
    }
    Ok(keys)
}

/// Digits and punctuation, and what Shift makes of each on a US layout.
const US_PLAIN: &str = "1234567890-=[]\\;',./`";
const US_SHIFTED: &str = "!@#$%^&*()_+{}|:\"<>?~";

#[derive(Default)]
struct Plan {
    steps: Vec<Step>,
    text: usize,
    paused: Duration,
}

impl Plan {
    fn ev(&mut self, e: Ev) {
        self.steps.push(Step::Event(e));
    }

    fn goto(&mut self, x: Option<i32>, y: Option<i32>) -> Result<(), String> {
        match (x, y) {
            (Some(x), Some(y)) => self.ev(Ev::Move(x, y)),
            (None, None) => {}
            _ => return Err("give both x and y, or neither".into()),
        }
        Ok(())
    }

    fn add(&mut self, a: Action) -> Result<(), String> {
        match a {
            Action::Move { x, y } => self.ev(Ev::Move(x, y)),
            Action::Click {
                x,
                y,
                button: b,
                count,
            } => {
                let b = button(b.as_ref())?;
                let count = count.unwrap_or(1);
                if !(1..=MAX_CLICKS).contains(&count) {
                    return Err(format!("count must be 1-{MAX_CLICKS}"));
                }
                self.goto(x, y)?;
                for _ in 0..count {
                    self.ev(Ev::Button(b, true));
                    self.ev(Ev::Button(b, false));
                }
            }
            Action::Down { x, y, button: b } => {
                let b = button(b.as_ref())?;
                self.goto(x, y)?;
                self.ev(Ev::Button(b, true));
            }
            Action::Up { x, y, button: b } => {
                let b = button(b.as_ref())?;
                self.goto(x, y)?;
                self.ev(Ev::Button(b, false));
            }
            Action::Scroll { x, y, dx, dy } => {
                if dx == 0 && dy == 0 {
                    return Err("scroll needs dx or dy".into());
                }
                self.goto(x, y)?;
                self.ev(Ev::Scroll(dx, dy));
            }
            Action::Key { key } => {
                let keys = combo(&key)?;
                for &k in &keys {
                    self.ev(Ev::Key(k, true));
                }
                for &k in keys.iter().rev() {
                    self.ev(Ev::Key(k, false));
                }
            }
            Action::KeyDown { key } => {
                for k in combo(&key)? {
                    self.ev(Ev::Key(k, true));
                }
            }
            Action::KeyUp { key } => {
                for k in combo(&key)?.into_iter().rev() {
                    self.ev(Ev::Key(k, false));
                }
            }
            Action::Type { text } => {
                self.text += text.chars().count();
                if self.text > MAX_TEXT {
                    return Err(format!("at most {MAX_TEXT} characters of text per request"));
                }
                // "\r\n" types one Enter.
                for c in text.chars().filter(|&c| c != '\r') {
                    let k = char_keysym(c).ok_or_else(|| format!("can't type {c:?}"))?;
                    self.ev(Ev::Key(k, true));
                    self.ev(Ev::Key(k, false));
                }
            }
            Action::Pause { ms } => {
                let d = Duration::from_millis(ms);
                self.paused += d;
                if d > MAX_PAUSE || self.paused > MAX_TOTAL_PAUSE {
                    return Err(format!(
                        "pauses are at most {} ms each and {} ms per request",
                        MAX_PAUSE.as_millis(),
                        MAX_TOTAL_PAUSE.as_millis()
                    ));
                }
                self.steps.push(Step::Pause(d));
            }
        }
        Ok(())
    }
}

/// The whole request, checked before any of it runs.
fn parse(body: &[u8]) -> Result<Vec<Step>, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("invalid JSON: {e}"))?;
    let items = match v {
        Value::Array(items) => items,
        v => vec![v],
    };
    if items.is_empty() || items.len() > MAX_ACTIONS {
        return Err(format!("send 1 to {MAX_ACTIONS} actions"));
    }
    let mut plan = Plan::default();
    for (i, item) in items.into_iter().enumerate() {
        serde_json::from_value(item)
            .map_err(|e| e.to_string())
            .and_then(|a| plan.add(a))
            .map_err(|e| format!("action {i}: {e}"))?;
    }
    Ok(plan.steps)
}

fn reply(code: StatusCode, body: Value) -> Response {
    (
        code,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body.to_string(),
    )
        .into_response()
}

fn error(code: StatusCode, msg: &str) -> Response {
    reply(code, json!({ "error": msg }))
}

/// Once the input thread has injected everything sent so far.
async fn synced(tx: &std::sync::mpsc::Sender<InputEvent>) -> Option<()> {
    let (done, rx) = tokio::sync::oneshot::channel();
    tx.send(InputEvent::Synced(done)).ok()?;
    rx.await.ok()
}

pub async fn input(State(s): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if s.read_only {
        return reply(
            StatusCode::FORBIDDEN,
            json!({ "error": "this server is read-only (--read-only): input is refused", "read_only": true }),
        );
    }
    // A JSON body also means a browser on another site can't send this without a CORS
    // preflight, which starfold never answers.
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "send the actions as Content-Type: application/json",
        );
    }
    let steps = match parse(&body) {
        Ok(steps) => steps,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e),
    };
    let Some(tx) = s.hub.input.lock().clone() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "input is unavailable");
    };
    let seq = s.hub.activity.seq();
    let hub = s.hub.clone();
    let run = tokio::spawn(async move {
        let mut events = 0u64;
        for step in steps {
            match step {
                Step::Event(e) => {
                    let ev = e.event();
                    hub.note_input(&ev);
                    tx.send(ev).ok()?;
                    events += 1;
                }
                Step::Pause(d) => {
                    synced(&tx).await?;
                    tokio::time::sleep(d).await;
                }
            }
        }
        synced(&tx).await?;
        Some(events)
    });
    match run.await {
        Ok(Some(events)) => reply(StatusCode::OK, json!({ "seq": seq, "events": events })),
        _ => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the input thread has stopped",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn events(body: &str) -> Vec<Ev> {
        parse(body.as_bytes())
            .unwrap()
            .into_iter()
            .filter_map(|s| match s {
                Step::Event(e) => Some(e),
                Step::Pause(_) => None,
            })
            .collect()
    }

    fn refused(body: &str) -> String {
        parse(body.as_bytes()).expect_err(body)
    }

    #[test]
    fn mouse_actions() {
        assert_eq!(events(r#"{"type":"move","x":3,"y":4}"#), [Ev::Move(3, 4)]);
        assert_eq!(
            events(r#"{"type":"click","x":1,"y":2,"button":"right","count":2}"#),
            [
                Ev::Move(1, 2),
                Ev::Button(3, true),
                Ev::Button(3, false),
                Ev::Button(3, true),
                Ev::Button(3, false)
            ]
        );
        assert_eq!(
            events(
                r#"[{"type":"down","x":5,"y":5},{"type":"move","x":9,"y":9},{"type":"up","button":1}]"#
            ),
            [
                Ev::Move(5, 5),
                Ev::Button(1, true),
                Ev::Move(9, 9),
                Ev::Button(1, false)
            ]
        );
        assert_eq!(events(r#"{"type":"scroll","dy":-3}"#), [Ev::Scroll(0, -3)]);
        assert_eq!(
            events(r#"{"type":"click","button":"Middle"}"#),
            [Ev::Button(2, true), Ev::Button(2, false)]
        );
    }

    #[test]
    fn keys_press_together_and_release_in_reverse() {
        assert_eq!(
            events(r#"{"type":"key","key":"ctrl+shift+T"}"#),
            [
                Ev::Key(0xffe3, true),
                Ev::Key(0xffe1, true),
                Ev::Key(0x54, true),
                Ev::Key(0x54, false),
                Ev::Key(0xffe1, false),
                Ev::Key(0xffe3, false)
            ]
        );
        assert_eq!(
            events(r#"[{"type":"key_down","key":"alt+Tab"},{"type":"key_up","key":"alt+Tab"}]"#),
            [
                Ev::Key(0xffe9, true),
                Ev::Key(0xff09, true),
                Ev::Key(0xff09, false),
                Ev::Key(0xffe9, false)
            ]
        );
        for (name, ks) in [
            ("Enter", 0xff0d),
            ("Return", 0xff0d),
            ("BackSpace", 0xff08),
            ("Page_Up", 0xff55),
            ("F1", 0xffbe),
            ("f24", 0xffd5),
            ("super", 0xffeb),
            ("+", 0x2b),
            ("é", 0xe9),
            ("€", 0x0100_20ac),
        ] {
            assert_eq!(combo(name).unwrap(), [ks], "{name}");
        }
        assert_eq!(combo("ctrl++").unwrap(), [0xffe3, 0x2b]);
        assert_eq!(combo("ctrl+shift+t").unwrap(), [0xffe3, 0xffe1, 0x54]);
        assert_eq!(combo("ctrl+T").unwrap(), [0xffe3, 0x74]);
        assert_eq!(combo("T").unwrap(), [0x54]);
        assert_eq!(combo("cmd+shift+4").unwrap(), [0xffeb, 0xffe1, 0x24]);
        assert_eq!(combo("shift+/").unwrap(), [0xffe1, 0x3f]);
        assert_eq!(combo("shift+\\").unwrap(), [0xffe1, 0x7c]);
        assert_eq!(combo("ctrl+4").unwrap(), [0xffe3, 0x34]);
        assert_eq!(combo("shift+Tab").unwrap(), [0xffe1, 0xff09]);
        assert_eq!(US_PLAIN.len(), US_SHIFTED.len());
        assert_eq!(combo("ctrl+plus").unwrap(), [0xffe3, 0x2b]);
    }

    #[test]
    fn text_types_each_character() {
        assert_eq!(
            events(r#"{"type":"type","text":"hi\r\n"}"#),
            [
                Ev::Key(0x68, true),
                Ev::Key(0x68, false),
                Ev::Key(0x69, true),
                Ev::Key(0x69, false),
                Ev::Key(0xff0d, true),
                Ev::Key(0xff0d, false)
            ]
        );
    }

    #[test]
    fn mistakes_are_refused_before_anything_runs() {
        for (body, why) in [
            ("nope", "invalid JSON"),
            ("[]", "1 to 1000"),
            (r#"{"type":"clik"}"#, "unknown variant"),
            (r#"{"type":"move","x":1}"#, "missing field `y`"),
            (r#"{"type":"move","x":1,"y":2,"z":3}"#, "unknown field `z`"),
            (r#"{"type":"click","x":1}"#, "both x and y"),
            (r#"{"type":"click","button":"thumb"}"#, "unknown button"),
            (r#"{"type":"click","button":16}"#, "1-15"),
            (r#"{"type":"click","count":0}"#, "count must be"),
            (r#"{"type":"scroll"}"#, "dx or dy"),
            (r#"{"type":"key","key":"hyper"}"#, "unknown key"),
            (r#"{"type":"key","key":"ctrl+"}"#, "empty part"),
            (r#"{"type":"key","key":""}"#, "empty part"),
            (
                r#"{"type":"key","key":"a+b+c+d+e+f+g+h+i"}"#,
                "at most 8 keys",
            ),
            (r#"{"type":"type","text":"a\u0007"}"#, "can't type"),
            (r#"{"type":"pause","ms":10001}"#, "pauses are at most"),
        ] {
            let e = refused(body);
            assert!(e.contains(why), "{body}: {e}");
        }
        let e = refused(r#"[{"type":"move","x":1,"y":2},{"type":"nope"}]"#);
        assert!(e.starts_with("action 1:"), "{e}");
        let many = format!("[{}]", [r#"{"type":"pause","ms":0}"#; 1001].join(","));
        assert!(refused(&many).contains("1 to 1000"));
        let pauses = format!("[{}]", [r#"{"type":"pause","ms":10000}"#; 7].join(","));
        assert!(refused(&pauses).contains("per request"));
        let chord = format!(r#"{{"type":"key","key":"{}a"}}"#, "a+".repeat(500_000));
        assert!(refused(&chord).contains("at most 8 keys"));
        let text = format!(r#"{{"type":"type","text":"{}"}}"#, "a".repeat(10_001));
        assert!(refused(&text).contains("characters"));
    }

    type Seen = Arc<parking_lot::Mutex<Vec<(Ev, std::time::Instant)>>>;

    fn evs(seen: &Seen) -> Vec<Ev> {
        seen.lock().iter().map(|&(e, _)| e).collect()
    }

    /// A server whose input thread records each event, `delay` apart, and answers syncs.
    fn state(read_only: bool, delay: Duration) -> (AppState, Seen) {
        let hub = crate::hub::Hub::new();
        let (tx, rx) = std::sync::mpsc::channel::<InputEvent>();
        *hub.input.lock() = Some(tx);
        let seen = Seen::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for ev in rx {
                std::thread::sleep(delay);
                let e = match ev {
                    InputEvent::Move { x, y } => Ev::Move(x, y),
                    InputEvent::Button { button, down } => Ev::Button(button, down),
                    InputEvent::Scroll { dx, dy } => Ev::Scroll(dx, dy),
                    InputEvent::Key { keysym, down } => Ev::Key(keysym, down),
                    InputEvent::Synced(done) => {
                        let _ = done.send(());
                        continue;
                    }
                    _ => unreachable!(),
                };
                log.lock().push((e, std::time::Instant::now()));
            }
        });
        let s = AppState {
            hub: hub.clone(),
            audio: false,
            epoch: std::time::Instant::now(),
            files: Arc::new(crate::files::Files {
                root: std::env::temp_dir(),
                upload: false,
                download: false,
                default_dir: String::new(),
                max_upload_bytes: 0,
            }),
            mic: None,
            resize: false,
            read_only,
            recorder: crate::record::Recorder::new(hub, Duration::from_secs(1)),
            index: Bytes::new(),
            favicon: Bytes::new(),
            favicon_type: "image/x-icon",
            encoders: Default::default(),
        };
        (s, seen)
    }

    async fn post(s: &AppState, content_type: &str, body: &str) -> (StatusCode, Value) {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
        let res = input(State(s.clone()), headers, Bytes::from(body.to_string())).await;
        let code = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (code, serde_json::from_slice(&body).unwrap())
    }

    const CLICK: &str = r#"{"type":"click","x":1,"y":2}"#;

    #[tokio::test]
    async fn only_keys_count_as_typing() {
        let (s, _seen) = state(false, Duration::ZERO);
        s.hub.activity.reset(64, 64);
        let pointer = r#"[{"type":"click","x":1,"y":2},{"type":"scroll","dy":3}]"#;
        assert_eq!(
            post(&s, "application/json", pointer).await.0,
            StatusCode::OK
        );
        assert!(
            s.hub.input_at.lock().is_some(),
            "pointer input went unnoted"
        );
        assert!(
            s.hub.key_at.lock().is_none(),
            "pointer input counted as typing"
        );
        let key = r#"{"type":"key","key":"a"}"#;
        assert_eq!(post(&s, "application/json", key).await.0, StatusCode::OK);
        assert!(s.hub.key_at.lock().is_some(), "a key went unnoted");
    }

    #[tokio::test]
    async fn read_only_servers_refuse_input() {
        let (s, seen) = state(true, Duration::ZERO);
        let (code, body) = post(&s, "application/json", CLICK).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert!(body["error"].as_str().unwrap().contains("read-only"));
        assert_eq!(body["read_only"], true);
        assert!(seen.lock().is_empty());
    }

    #[tokio::test]
    async fn answers_once_the_input_is_done_with_the_seq_before_it() {
        let (s, seen) = state(false, Duration::from_millis(20));
        s.hub.activity.reset(64, 64);
        let seq = s.hub.activity.seq();
        let (code, body) = post(&s, "application/json; charset=utf-8", CLICK).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body, json!({ "seq": seq, "events": 3 }));
        assert_eq!(
            evs(&seen),
            [Ev::Move(1, 2), Ev::Button(1, true), Ev::Button(1, false)]
        );
    }

    #[tokio::test]
    async fn a_pause_starts_once_the_events_before_it_are_done() {
        let (s, seen) = state(false, Duration::from_millis(30));
        let body = r#"[{"type":"type","text":"abc"},{"type":"pause","ms":50},{"type":"key","key":"Enter"}]"#;
        let (code, _) = post(&s, "application/json", body).await;
        assert_eq!(code, StatusCode::OK);
        let seen = seen.lock();
        assert_eq!(seen.len(), 8);
        // Without waiting for the typing, the pause would have run out while the input
        // thread was still at it, and Enter would follow the last key 30 ms later.
        let gap = seen[6].1 - seen[5].1;
        assert!(gap >= Duration::from_millis(80), "{gap:?}");
    }

    #[tokio::test]
    async fn bad_requests_send_nothing() {
        let (s, seen) = state(false, Duration::ZERO);
        let (code, _) = post(&s, "text/plain", CLICK).await;
        assert_eq!(code, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let (code, body) = post(
            &s,
            "application/json",
            r#"[{"type":"move","x":1,"y":1},{"type":"key","key":"nope"}]"#,
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().starts_with("action 1:"));
        assert!(seen.lock().is_empty());
        *s.hub.input.lock() = None;
        let (code, _) = post(&s, "application/json", CLICK).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    }
}
