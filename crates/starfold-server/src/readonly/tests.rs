use super::*;
use axum::routing::{on, MethodFilter};
use axum::Router;
use std::collections::BTreeSet;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SERVER: &str = include_str!("../server.rs");
const LOCK: &str = include_str!("../../../../Cargo.lock");

// The source scan below reads server.rs and fails closed: every call in router() is either
// one it parses (a route, a merged local router, the layer) or one on a short list of known
// harmless calls, and every arm of handle_text's command match is a plain string literal
// or `_`. Anything else is refused, so a registration form the scan does not understand
// fails the tests instead of passing unseen.

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// A source text with its comments removed (nested block comments included), as two texts
/// of the same length: `code` keeps every literal, `blank` has each literal's contents
/// replaced by spaces, so a parenthesis, keyword or name inside a string is not code.
struct Src {
    code: String,
    blank: String,
}

/// Where a raw string (`r"…"`, `r#"…"#`, `br"…"`) starting at `i` opens and ends: the
/// length of its opening delimiter, its number of `#`s and the index just past its end.
fn raw_string(t: &[u8], i: usize) -> Option<(usize, usize, usize)> {
    let mut j = i;
    if t[j] == b'b' {
        j += 1;
    }
    if t.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let hashes_from = j;
    while t.get(j) == Some(&b'#') {
        j += 1;
    }
    if t.get(j) != Some(&b'"') {
        return None;
    }
    let hashes = j - hashes_from;
    let mut k = j + 1;
    while k < t.len() {
        if t[k] == b'"'
            && t[k + 1..]
                .iter()
                .take(hashes)
                .filter(|h| **h == b'#')
                .count()
                == hashes
        {
            return Some((j + 1 - i, hashes, k + 1 + hashes));
        }
        k += 1;
    }
    Some((j + 1 - i, hashes, t.len()))
}

fn lex(text: &str) -> Src {
    let t = text.as_bytes();
    let mut code = Vec::with_capacity(t.len());
    let mut blank = Vec::with_capacity(t.len());
    let literal = |code: &mut Vec<u8>, blank: &mut Vec<u8>, a: usize, b: usize, open: usize| {
        let b = b.min(t.len());
        let close = (b - a - open).min(if b > a { 1 } else { 0 });
        code.extend_from_slice(&t[a..b]);
        blank.extend_from_slice(&t[a..a + open]);
        blank.extend(std::iter::repeat_n(b' ', b - a - open - close));
        blank.extend_from_slice(&t[b - close..b]);
    };
    let mut i = 0;
    while i < t.len() {
        let after_ident = i > 0 && is_ident(t[i - 1]);
        match t[i] {
            b'/' if t.get(i + 1) == Some(&b'/') => {
                while i < t.len() && t[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if t.get(i + 1) == Some(&b'*') => {
                let mut depth = 0;
                while i < t.len() {
                    if t[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if t[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                code.push(b' ');
                blank.push(b' ');
            }
            b'r' | b'b' if !after_ident && raw_string(t, i).is_some() => {
                let (open, hashes, end) = raw_string(t, i).unwrap();
                let end = end.min(t.len());
                code.extend_from_slice(&t[i..end]);
                blank.extend_from_slice(&t[i..i + open]);
                let inner = end.saturating_sub(i + open + 1 + hashes);
                blank.extend(std::iter::repeat_n(b' ', inner));
                blank.extend_from_slice(&t[i + open + inner..end]);
                i = end;
            }
            b'"' => {
                let mut k = i + 1;
                while k < t.len() && t[k] != b'"' {
                    k += if t[k] == b'\\' { 2 } else { 1 };
                }
                literal(&mut code, &mut blank, i, k + 1, 1);
                i = (k + 1).min(t.len());
            }
            b'\'' if t.get(i + 1) == Some(&b'\\') => {
                let mut k = i + 3;
                while k < t.len() && t[k] != b'\'' {
                    k += 1;
                }
                literal(&mut code, &mut blank, i, k + 1, 1);
                i = (k + 1).min(t.len());
            }
            b'\'' if i + 1 < t.len() => {
                let n = match t[i + 1] {
                    b if b < 0x80 => 1,
                    b if b < 0xe0 => 2,
                    b if b < 0xf0 => 3,
                    _ => 4,
                };
                if t.get(i + 1 + n) == Some(&b'\'') {
                    literal(&mut code, &mut blank, i, i + 2 + n, 1);
                    i += 2 + n;
                } else {
                    code.push(b'\'');
                    blank.push(b'\'');
                    i += 1;
                }
            }
            b => {
                code.push(b);
                blank.push(b);
                i += 1;
            }
        }
    }
    Src {
        code: String::from_utf8(code).unwrap(),
        blank: String::from_utf8(blank).unwrap(),
    }
}

fn skip_ws(s: &str, mut i: usize) -> usize {
    let b = s.as_bytes();
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// The index of the bracket that closes the one opened just before `from`.
fn closing(blank: &str, from: usize, open: u8, close: u8) -> usize {
    let mut depth = 1;
    for (i, b) in blank.bytes().enumerate().skip(from) {
        if b == open {
            depth += 1;
        } else if b == close {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
    }
    panic!("an unclosed bracket")
}

/// Every whole-word use of `word` in `blank`.
fn words<'a>(blank: &'a str, word: &'a str) -> impl Iterator<Item = usize> + 'a {
    let b = blank.as_bytes();
    blank.match_indices(word).map(|m| m.0).filter(move |&at| {
        (at == 0 || !is_ident(b[at - 1])) && !b.get(at + word.len()).is_some_and(|c| is_ident(*c))
    })
}

/// The body of the one function `sig` (`fn name(`) in `blank`: from after its `{` to its `}`.
fn body(blank: &str, sig: &str) -> (usize, usize) {
    let at: Vec<usize> = blank.match_indices(sig).map(|m| m.0).collect();
    assert_eq!(at.len(), 1, "one {sig}");
    let open = at[0] + blank[at[0]..].find('{').expect("its body");
    (open + 1, closing(blank, open + 1, b'{', b'}'))
}

/// A call in the source: `name(`, `name!(`, or `name::<` (a turbofish).
struct Call {
    at: usize,
    name: String,
    /// The whole path, `a::b::name`, without whitespace.
    path: String,
    /// Called as a method, `.name(`.
    method: bool,
    macro_call: bool,
    turbofish: bool,
    /// Just after its `(`, unless it is a turbofish.
    open: Option<usize>,
}

fn calls(blank: &str) -> Vec<Call> {
    let b = blank.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !is_ident(b[i]) || (i > 0 && is_ident(b[i - 1])) || b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < b.len() && is_ident(b[j]) {
            j += 1;
        }
        let k = skip_ws(blank, j);
        let (macro_call, turbofish, open) = match b.get(k) {
            Some(b'!') => {
                let k = skip_ws(blank, k + 1);
                (
                    true,
                    false,
                    matches!(b.get(k), Some(b'(' | b'[' | b'{')).then_some(k + 1),
                )
            }
            Some(b'(') => (false, false, Some(k + 1)),
            Some(b':')
                if blank[k..].starts_with("::") && b.get(skip_ws(blank, k + 2)) == Some(&b'<') =>
            {
                (false, true, None)
            }
            _ => {
                i = j;
                continue;
            }
        };
        if macro_call && open.is_none() {
            i = j;
            continue;
        }
        let mut start = i;
        loop {
            let before = blank[..start].trim_end();
            let Some(prefix) = before.strip_suffix("::") else {
                break;
            };
            let prefix = prefix.trim_end();
            let seg = prefix.len() - prefix.bytes().rev().take_while(|c| is_ident(*c)).count();
            if seg == prefix.len() {
                break;
            }
            start = seg;
        }
        out.push(Call {
            at: i,
            name: blank[i..j].to_string(),
            path: blank[start..j].split_whitespace().collect(),
            method: blank[..start].trim_end().ends_with('.'),
            macro_call,
            turbofish,
            open,
        });
        i = j;
    }
    out
}

/// The method a method-routing function registers; `any` and `on` take every method, which
/// no table entry matches. The bundled! and asset! macros are get. The `_service` forms
/// are not parsed: a service can be a whole method router.
fn method_of(f: &str) -> Option<&'static str> {
    Some(match f {
        "get" | "bundled!" | "asset!" => "GET",
        "head" => "HEAD",
        "post" => "POST",
        "put" => "PUT",
        "patch" => "PATCH",
        "delete" => "DELETE",
        "options" => "OPTIONS",
        "trace" => "TRACE",
        "connect" => "CONNECT",
        "any" | "on" => "ANY",
        _ => return None,
    })
}

fn is_path(s: &str) -> bool {
    !s.is_empty()
        && s.split("::")
            .all(|seg| !seg.is_empty() && seg.bytes().all(is_ident))
}

/// The methods a route's method router registers, if it is a chain of method-routing calls
/// (`get(a)`, `axum::routing::post(b).put(c)`, `bundled!("x")`) whose handlers are paths,
/// or closures and macro arguments that call nothing. None for anything else.
fn chain(blank: &str, from: usize, to: usize) -> Option<Vec<&'static str>> {
    let b = blank.as_bytes();
    let mut out = Vec::new();
    let mut i = skip_ws(blank, from);
    loop {
        let start = i;
        loop {
            let s = i;
            while i < to && is_ident(b[i]) {
                i += 1;
            }
            if i == s {
                return None;
            }
            if blank[i..to].starts_with("::") {
                i += 2;
            } else {
                break;
            }
        }
        let mut name = blank[start..i].rsplit("::").next()?.to_string();
        if b.get(i) == Some(&b'!') {
            name.push('!');
            i += 1;
        }
        i = skip_ws(blank, i);
        if i >= to || b[i] != b'(' {
            return None;
        }
        let close = closing(blank, i + 1, b'(', b')');
        let inner = blank[i + 1..close].trim();
        let calls_nothing = !inner.contains('(') && !inner.contains('!');
        let handler = if name.ends_with('!') {
            calls_nothing
        } else {
            is_path(inner)
                || ((inner.starts_with('|') || inner.starts_with("move")) && calls_nothing)
        };
        if !handler {
            return None;
        }
        out.push(method_of(&name)?);
        i = skip_ws(blank, close + 1);
        if i >= to {
            return Some(out);
        }
        if b[i] != b'.' {
            return None;
        }
        i = skip_ws(blank, i + 1);
    }
}

/// Calls that register routes, fallbacks or other routers on a router (axum 0.8).
const REGISTERING: &[&str] = &[
    "route",
    "route_service",
    "route_layer",
    "merge",
    "nest",
    "nest_service",
    "fallback",
    "fallback_service",
    "method_not_allowed_fallback",
];
/// The other calls router() may make on its routers and state.
const CHAIN: &[&str] = &["with_state", "layer", "clone"];
/// The functions router() may call outside its routes.
const PATHS: &[&str] = &[
    "Router::new",
    "axum::middleware::from_fn_with_state",
    "crate::compat::is_set",
];

/// Whole-word uses of a registering name as a method (`.route(`, `.merge::<`) or through a
/// path (`Router::route`); fields (`x.route.lock()`) and the crate's own functions and
/// variables of those names are not.
fn registering_uses(blank: &str) -> Vec<(usize, &'static str)> {
    let mut out = Vec::new();
    for name in REGISTERING {
        for at in words(blank, name) {
            let before = blank[..at].trim_end();
            let after = blank[at + name.len()..].trim_start();
            let method =
                before.ends_with('.') && (after.starts_with('(') || after.starts_with("::"));
            if method || before.ends_with("::") {
                out.push((at, *name));
            }
        }
    }
    out
}

fn snippet(code: &str, at: usize) -> String {
    let end = (at + 60).min(code.len());
    let end = (end..=code.len()).find(|e| code.is_char_boundary(*e)).unwrap();
    code[at..end].split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What router() in `text` registers, as (method, path), and every form the scan does not
/// understand, refused.
fn registrations(text: &str) -> (BTreeSet<(String, String)>, Vec<String>) {
    let Src { code, blank } = lex(text);
    let mut routes = BTreeSet::new();
    let mut refused = Vec::new();
    let (from, to) = body(&blank, "fn router(");
    let checks: Vec<usize> = blank
        .match_indices("crate::readonly::check")
        .map(|m| m.0)
        .collect();
    if checks.len() != 1 || !(from..to).contains(&checks[0]) {
        refused.push("not exactly one read-only layer, inside router()".to_string());
    }
    let layer = checks.first().copied().unwrap_or(0);
    let mut parsed_args: Vec<(usize, usize)> = Vec::new();
    let mut accepted = BTreeSet::new();
    for c in calls(&blank) {
        if parsed_args.iter().any(|(a, b)| (*a..*b).contains(&c.at)) {
            continue;
        }
        let inside = (from..to).contains(&c.at);
        let what = format!("{} at byte {}: {}", c.path, c.at, snippet(&code, c.at));
        if REGISTERING.contains(&c.name.as_str()) {
            let Some(open) = c.open.filter(|_| {
                inside && c.method && !c.turbofish && !c.macro_call && c.at < layer
            }) else {
                refused.push(format!("a registration outside router()'s chain before the read-only layer: {what}"));
                continue;
            };
            let close = closing(&blank, open, b'(', b')');
            parsed_args.push((open, close));
            match c.name.as_str() {
                "route" => {
                    let mut depth = 0;
                    let comma = (open..close).find(|&i| {
                        match blank.as_bytes()[i] {
                            b'(' | b'[' | b'{' => depth += 1,
                            b')' | b']' | b'}' => depth -= 1,
                            b',' if depth == 0 => return true,
                            _ => {}
                        }
                        false
                    });
                    let path = comma.and_then(|comma| {
                        let p = code[open..comma].trim();
                        let ok = p.len() >= 2
                            && p.starts_with('"')
                            && p.ends_with('"')
                            && !p.contains('\\');
                        ok.then(|| p[1..p.len() - 1].to_string())
                    });
                    match (path, comma.and_then(|comma| chain(&blank, comma + 1, close))) {
                        (Some(path), Some(methods)) => {
                            accepted.insert(c.at);
                            for m in methods {
                                routes.insert((m.to_string(), path.clone()));
                            }
                        }
                        _ => refused.push(format!("a route the scan cannot read: {what}")),
                    }
                }
                "merge" if is_path(blank[open..close].trim()) && !blank[open..close].contains("::") => {
                    accepted.insert(c.at);
                }
                _ => refused.push(format!("a registration the scan does not parse: {what}")),
            }
        } else if inside {
            let known = if c.turbofish || c.macro_call {
                false
            } else if c.method {
                CHAIN.contains(&c.name.as_str())
            } else {
                PATHS.contains(&c.path.as_str())
            };
            if !known {
                refused.push(format!("a call in router() the scan does not know: {what}"));
            }
        }
    }
    for (at, name) in registering_uses(&blank) {
        if !accepted.contains(&at) && !refused.iter().any(|r| r.contains(&format!("at byte {at}:"))) {
            refused.push(format!("{name} used at byte {at}: {}", snippet(&code, at)));
        }
    }
    (routes, refused)
}

/// The commands handle_text in `text` matches on, and every form the scan does not
/// understand, refused: `cmd` may only be bound, passed to readonly::command and matched
/// once, and each arm of that match is plain string literals (with `|` and a guard) or `_`.
fn commands(text: &str) -> (BTreeSet<String>, Vec<String>) {
    let Src { code, blank } = lex(text);
    let (from, to) = body(&blank, "fn handle_text(");
    let mut refused = Vec::new();
    let mut matched = None;
    for at in words(&blank, "cmd").filter(|at| (from..to).contains(at)) {
        let before = blank[from..at].trim_end();
        let after = blank[at + 3..to].trim_start();
        let bound = before.ends_with("let (") && after.starts_with(", rest)");
        let decided = before.ends_with("crate::readonly::command(") && after.starts_with(')');
        if bound || decided {
            continue;
        }
        if before.ends_with("match") && after.starts_with('{') && matched.is_none() {
            matched = Some(to - after.len());
        } else {
            refused.push(format!("cmd used at byte {at}: {}", snippet(&code, at)));
        }
    }
    let mut out = BTreeSet::new();
    let Some(open) = matched else {
        refused.push("no `match cmd`".to_string());
        return (out, refused);
    };
    let b = blank.as_bytes();
    let close = closing(&blank, open + 1, b'{', b'}');
    let mut i = open + 1;
    loop {
        while i < close && (b[i].is_ascii_whitespace() || b[i] == b',') {
            i += 1;
        }
        if i >= close {
            break;
        }
        let Some(arrow) = blank[i..close].find("=>").map(|a| i + a) else {
            refused.push(format!("an arm without =>: {}", snippet(&code, i)));
            break;
        };
        let guard = words(&blank[..arrow], "if").find(|g| *g >= i).unwrap_or(arrow);
        let mut alt = i;
        for end in (i..guard).filter(|e| b[*e] == b'|').chain([guard]) {
            let lit = code[alt..end].trim();
            let blanked = blank[alt..end].trim();
            let plain = lit.len() >= 2
                && lit.starts_with('"')
                && lit.ends_with('"')
                && blanked[1..blanked.len() - 1].bytes().all(|c| c == b' ')
                && !lit.contains('\\');
            if plain {
                out.insert(lit[1..lit.len() - 1].to_string());
            } else if lit != "_" {
                refused.push(format!("an arm the scan does not read: {}", snippet(&code, alt)));
            }
            alt = end + 1;
        }
        let mut j = skip_ws(&blank, arrow + 2);
        if b.get(j) == Some(&b'{') {
            j = closing(&blank, j + 1, b'{', b'}') + 1;
        } else {
            let mut depth = 0;
            while j < close {
                match b[j] {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => depth -= 1,
                    b',' if depth == 0 => break,
                    _ => {}
                }
                j += 1;
            }
        }
        i = j;
    }
    (out, refused)
}

/// Every source file of the crate but its tests, as (path, text).
fn sources() -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    let mut dirs = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("tests.rs") {
                out.push((p.clone(), std::fs::read_to_string(&p).unwrap()));
            }
        }
    }
    out
}

/// A router() for the scan's own tests, with `chain` added before its layer.
fn router_with(chain: &str) -> String {
    format!(
        "fn other() {{ c.route.lock(); }}
pub fn router(state: AppState) -> Router {{
    let files = Router::new().route(\"/f\", get(files::list)).with_state(state.files.clone());
    Router::new()
        .route ( \"/a\", get(a).post(b) )
        .route(\"/b\",
            axum::routing::head(h))
        // .route(\"/commented\", post(x))
        /* /* nested */ .route(\"/block\", post(x)) */
        .route(\"/c\", bundled!(\"c.js\"))
        .route(\"/s\", get(|| async {{ \".route(\\\"/zz\\\", post(x))\" }}))
        .merge(files)
        {chain}
        .layer(axum::middleware::from_fn_with_state(
            read_only,
            crate::readonly::check,
        ))
}}"
    )
}

/// handle_text for the scan's own tests, with `arms` added to its match.
fn handle_text_with(arms: &str) -> String {
    format!(
        "fn handle_text(s: &AppState, t: &str) {{
    let (cmd, rest) = t.split_once(',').unwrap_or((t, \"\"));
    if s.read_only && crate::readonly::command(cmd) != Some(Watch) {{
        return;
    }}
    match cmd {{
        \"a\" => {{
            x(\"=> | \\\"q\\\"\");
        }}
        \"b\"
        | \"c\" => y(),
        \"d\" | \"e\"
            | \"f\" if z => w(rest),
        {arms}
        _ => {{}}
    }}
}}"
    )
}

#[test]
fn the_scan_reads_what_it_knows_and_refuses_the_rest() {
    let (routes, refused) = registrations(&router_with(""));
    assert!(refused.is_empty(), "{refused:#?}");
    let want = [
        ("GET", "/a"),
        ("POST", "/a"),
        ("HEAD", "/b"),
        ("GET", "/c"),
        ("GET", "/s"),
        ("GET", "/f"),
    ]
    .map(|(m, p)| (m.to_string(), p.to_string()));
    assert_eq!(routes, BTreeSet::from(want));
    for chain in [
        ".route(\"/d\", helper())",
        ".route(\"/e\", get(a).merge(h))",
        ".route(\"/g\", get_service(s))",
        ".route(\"/h\", get(make()))",
        ".route(PATH, get(a))",
        ".merge::<Router>(x)",
        ".merge(helper())",
        ".method_not_allowed_fallback(h)",
        ". fallback (h)",
        ".nest(\"/n\", x)",
        ".route_service(\"/r\", s)",
        ".route_layer(l)",
        ".merge(Router::route(Router::new(), \"/u\", get(a)))",
        ".add_more()",
        ".layer(more_routes!())",
        ".with_state(axum::routing::head(h))",
    ] {
        let (_, refused) = registrations(&router_with(chain));
        assert!(!refused.is_empty(), "not refused: {chain}");
    }
    let late = router_with("").replace("\n}", "\n    .route(\"/late\", get(a))\n}");
    assert!(!registrations(&late).1.is_empty(), "a route after the layer");
    let local = router_with("").replace(
        "    Router::new()\n",
        "    let h = axum::routing::head(x);\n    Router::new()\n",
    );
    assert!(!registrations(&local).1.is_empty(), "a method router in a local");
    let (commands_found, refused) = commands(&handle_text_with(""));
    assert!(refused.is_empty(), "{refused:#?}");
    let want = ["a", "b", "c", "d", "e", "f"].map(String::from);
    assert_eq!(commands_found, BTreeSet::from(want));
    for arms in [
        "r\"q\" => q(),",
        "r#\"q\"# => q(),",
        "Q => q(),",
        "x @ \"q\" => q(),",
        "\"q\\x41\" => q(),",
        "_ if cmd == \"q\" => q(),",
    ] {
        let (_, refused) = commands(&handle_text_with(arms));
        assert!(!refused.is_empty(), "not refused: {arms}");
    }
}

#[test]
fn the_scan_is_for_the_axum_it_was_written_against() {
    let axum = LOCK
        .split("[[package]]")
        .find(|p| p.contains("\nname = \"axum\"\n"))
        .expect("axum in Cargo.lock");
    assert!(
        axum.contains("\nversion = \"0.8."),
        "a new axum: check REGISTERING against its Router and MethodRouter methods: {axum}"
    );
}

#[test]
fn routes_are_only_registered_where_the_table_is_checked() {
    let mut seen_server = false;
    for (path, text) in sources() {
        if path.ends_with("server.rs") {
            seen_server = true;
            let (_, refused) = registrations(&text);
            assert!(
                refused.is_empty(),
                "server.rs: forms the read-only scan does not understand; register routes as plain .route(\"/path\", method(handler)) calls in router() before the layer, with decisions in readonly.rs: {refused:#?}"
            );
            continue;
        }
        let Src { code, blank } = lex(&text);
        if let Some((at, name)) = registering_uses(&blank).first() {
            panic!(
                "{} uses {name} at byte {at} ({}): routes are added in server.rs's router(), before the read-only layer",
                path.display(),
                snippet(&code, *at)
            );
        }
        for word in ["Router", "MethodRouter"] {
            assert!(
                words(&blank, word).next().is_none(),
                "{} names {word}: routes are added in server.rs's router()",
                path.display()
            );
        }
    }
    assert!(seen_server);
}

#[test]
fn every_route_the_server_has_is_decided_once() {
    let table: Vec<(String, String)> = ROUTES
        .iter()
        .map(|(m, p, _)| (m.to_string(), p.to_string()))
        .collect();
    let decided: BTreeSet<_> = table.iter().cloned().collect();
    assert_eq!(decided.len(), table.len(), "a route decided twice");
    let (routes, refused) = registrations(SERVER);
    assert!(refused.is_empty(), "{refused:#?}");
    assert!(routes.len() >= 20, "{routes:?}");
    assert_eq!(
        routes, decided,
        "routes in server.rs vs readonly::ROUTES: a new route needs a decision"
    );
}

#[test]
fn every_command_the_server_takes_is_decided_once() {
    let decided: BTreeSet<String> = COMMANDS.iter().map(|(c, _)| c.to_string()).collect();
    assert_eq!(decided.len(), COMMANDS.len(), "a command decided twice");
    let (commands, refused) = commands(SERVER);
    assert!(refused.is_empty(), "{refused:#?}");
    assert!(commands.len() >= 10, "{commands:?}");
    assert_eq!(
        commands, decided,
        "commands in handle_text vs readonly::COMMANDS: a new command needs a decision"
    );
}

#[test]
fn what_acts_is_what_changes_something() {
    let acting: BTreeSet<&str> = ROUTES.iter().filter(|r| r.2 == Act).map(|r| r.1).collect();
    assert_eq!(
        acting,
        BTreeSet::from([
            "/input",
            "/files/upload",
            "/record",
            "/record/start",
            "/record/stop",
            "/debug/renegotiate",
            "/debug/fail-h264",
        ])
    );
    for c in ["m", "b", "s", "sp", "k", "r", "c", "mi", "sz"] {
        assert_eq!(command(c), Some(Act), "{c}");
    }
    for c in ["a", "v", "au", "kf", "po"] {
        assert_eq!(command(c), Some(Watch), "{c}");
    }
    assert_eq!(
        command("new"),
        None,
        "no decision: refused under --read-only"
    );
    for first in [None, Some(0x11), Some(0x12), Some(0x13), Some(0xff)] {
        assert_eq!(binary(first), Act);
    }
    assert_eq!(route(&Method::HEAD, "/healthz"), Some(Watch));
    assert_eq!(route(&Method::POST, "/healthz"), None);
}

/// A router with every route in the table, each answering "ran", behind the layer.
fn stub(read_only: bool) -> Router {
    let mut r = Router::new();
    for (m, p, _) in ROUTES {
        let filter = MethodFilter::try_from(Method::from_bytes(m.as_bytes()).unwrap()).unwrap();
        r = r.route(p, on(filter, || async { "ran" }));
    }
    r.fallback(|| async { "fallback ran" })
        .layer(axum::middleware::from_fn_with_state(read_only, check))
}

async fn serve(router: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    port
}

async fn ask(port: u16, method: &str, path: &str) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, text)
}

#[tokio::test]
async fn a_read_only_server_refuses_every_acting_route_and_no_watching_one() {
    let port = serve(stub(true)).await;
    for (m, p, kind) in ROUTES {
        let (status, text) = ask(port, m, p).await;
        match kind {
            Act => {
                assert_eq!(status, 403, "{m} {p}: {text}");
                assert!(text.contains("\"read_only\":true"), "{m} {p}: {text}");
                assert!(!text.ends_with("ran"), "{m} {p} ran");
            }
            Watch => assert!(text.ends_with("ran"), "{m} {p}: {text}"),
        }
    }
    let (status, _) = ask(port, "POST", "/healthz").await;
    assert_eq!(status, 403, "a method with no decision is refused too");
    for method in ["GET", "POST"] {
        let (status, text) = ask(port, method, "/no-such-route").await;
        assert_eq!(status, 404, "{method}: {text}");
        assert!(!text.contains("fallback ran"), "{method}: the fallback ran");
    }
}

#[tokio::test]
async fn a_server_that_is_not_read_only_refuses_nothing() {
    let port = serve(stub(false)).await;
    for (m, p, _) in ROUTES {
        let (_, text) = ask(port, m, p).await;
        assert!(text.ends_with("ran"), "{m} {p}: {text}");
    }
}
