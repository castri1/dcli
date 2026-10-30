//! Small dependency-free helpers: time, paths, text and encodings.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix time in seconds (fractional).
pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The command name this binary was invoked as (`dcli`, or a renamed copy), for hints.
pub fn program() -> String {
    std::env::args_os()
        .next()
        .and_then(|a| std::path::Path::new(&a).file_stem().map(|s| s.to_string_lossy().into_owned()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "dcli".into())
}

/// The user's home directory, on every platform.
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `path` with the home directory shown as `~`, safe to print.
pub fn display_path(path: &std::path::Path) -> String {
    sanitize(&match home_dir().and_then(|h| path.strip_prefix(h).ok().map(|rest| rest.to_path_buf())) {
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    })
}

// ---------------------------------------------------------------------------
// Dates (UTC only; civil-from-days from Howard Hinnant's date algorithms)
// ---------------------------------------------------------------------------

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Splits Unix seconds into UTC (year, month, day, hour, minute, second).
pub fn utc_parts(epoch: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    (y, mo, d, (secs / 3600) as u32, ((secs % 3600) / 60) as u32, (secs % 60) as u32)
}

/// `(yyyymmdd, yyyymmddThhmmssZ)` as used by SigV4.
pub fn amz_dates(epoch: i64) -> (String, String) {
    let (y, mo, d, h, mi, s) = utc_parts(epoch);
    let date = format!("{y:04}{mo:02}{d:02}");
    let stamp = format!("{date}T{h:02}{mi:02}{s:02}Z");
    (date, stamp)
}

/// `2026-10-19T20:25:01Z`
pub fn rfc3339(epoch: i64) -> String {
    let (y, mo, d, h, mi, s) = utc_parts(epoch);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// `2026-10-19 20:25 UTC`
pub fn fmt_utc(epoch: f64) -> String {
    let (y, mo, d, h, mi, _) = utc_parts(epoch as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} UTC")
}

/// Parses ISO 8601 / RFC 3339 timestamps as AWS emits them:
/// `2026-10-19T20:25:01Z`, `…01.123Z`, `…01UTC`, `…01+00:00`, `…01.000+0000`.
pub fn parse_rfc3339(s: &str) -> Option<f64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<u32>().ok();
    let (y, mo, d) = (num(0..4)? as i64, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &s[19..];
    let mut frac = 0.0;
    if let Some(r) = rest.strip_prefix('.') {
        let digits = r.chars().take_while(char::is_ascii_digit).count();
        frac = format!("0.{}", &r[..digits]).parse().unwrap_or(0.0);
        rest = &r[digits..];
    }
    let offset = match rest {
        "" | "Z" | "z" | "UTC" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let digits: String = rest[1..].chars().filter(char::is_ascii_digit).collect();
            if digits.len() != 4 {
                return None;
            }
            let oh: i64 = digits[..2].parse().ok()?;
            let om: i64 = digits[2..].parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(sec) - offset;
    Some(secs as f64 + frac)
}

// ---------------------------------------------------------------------------
// Human formatting
// ---------------------------------------------------------------------------

/// `45s`, `2m 34s`, `1h 05m`
pub fn fmt_duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// `just now`, `5m ago`, `3h ago`, `2d ago`
pub fn fmt_ago(epoch: f64) -> String {
    let s = (now_secs() - epoch).max(0.0) as u64;
    match s {
        0..=9 => "just now".into(),
        10..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// `1.4 MB`
pub fn fmt_bytes(n: i64) -> String {
    let n = n as f64;
    if n < 1024.0 {
        format!("{n} B")
    } else if n < 1024.0 * 1024.0 {
        format!("{:.1} KB", n / 1024.0)
    } else if n < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", n / (1024.0 * 1024.0))
    } else if n < 1024.0 * 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB", n / (1024.0 * 1024.0 * 1024.0))
    } else {
        format!("{:.1} TB", n / (1024.0 * 1024.0 * 1024.0 * 1024.0))
    }
}

/// Cuts `s` to at most `max` characters, ending in `…` when shortened.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Every whitespace-separated term of the (lower-cased) query appears in `name`.
pub fn matches_filter(name: &str, query_lower: &str) -> bool {
    let name = name.to_lowercase();
    query_lower.split_whitespace().all(|term| name.contains(term))
}

/// A log line that looks like an error, a failed test or a crash.
pub fn looks_like_error(line: &str) -> bool {
    const HINTS: [&str; 15] = [
        "error", "err!", "fail", "exception", "traceback", "panic", "fatal", "denied", "not found", "cannot ",
        "unable to", "exit status", "exit code", "✗", "✖",
    ];
    let lower = line.to_lowercase();
    HINTS.iter().any(|h| lower.contains(h))
        && !["0 error", "0 fail", "no error", "without error"].iter().any(|ok| lower.contains(ok))
}

/// First line of `s`, trimmed.
pub fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

/// Bidi overrides, zero-width and other format characters, line and paragraph separators, and
/// the Unicode tag block: invisible, and able to make text read differently from what it is (or
/// carry hidden instructions to an AI). Emoji variation selectors stay.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{061c}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{e0000}'..='\u{e007f}'
    )
}

/// Control characters (other than `\n` and `\t`) and invisible formatting characters.
pub fn is_unsafe(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t') || is_invisible(c)
}

/// Removes terminal escape sequences, control characters other than `\n` and `\t`, and
/// invisible formatting characters, so text from AWS (commit messages, logs, errors) or a
/// config file can neither drive the terminal nor disguise itself.
pub fn sanitize(s: &str) -> String {
    if !s.chars().any(is_unsafe) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                // CSI: parameters until a final byte in @..~
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC, DCS, APC, PM, SOS: until BEL or ST
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || c == '\u{9c}' || (c == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            if c == '\u{1b}' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // Two-character sequences (ESC 7, ESC c, …)
                _ => {
                    chars.next();
                }
            },
            c if is_unsafe(c) => {}
            c => out.push(c),
        }
    }
    out
}

/// Sanitizes every string and object key of a JSON value in place (documents shown in the
/// terminal: feature flag definitions, for one).
pub fn sanitize_json(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::String(s) if s.chars().any(is_unsafe) => *s = sanitize(s),
        Value::Array(items) => items.iter_mut().for_each(sanitize_json),
        Value::Object(map) => {
            let entries = std::mem::take(map);
            for (k, mut v) in entries {
                sanitize_json(&mut v);
                map.insert(sanitize(&k), v);
            }
        }
        _ => {}
    }
}

/// For text that will run (hook commands): every control or invisible character is shown
/// as a visible `\u{…}` escape instead of being dropped, so what you read is what runs.
pub fn show_invisible(s: &str) -> String {
    s.chars()
        .map(|c| if is_unsafe(c) { format!("\\u{{{:x}}}", c as u32) } else { c.to_string() })
        .collect()
}

/// [`sanitize`] with tabs expanded, for log lines shown in fixed columns.
pub fn strip_ansi(s: &str) -> String {
    sanitize(s).replace('\t', "    ")
}

// ---------------------------------------------------------------------------
// Encodings
// ---------------------------------------------------------------------------

/// Text of the first `<tag>…</tag>` in `xml`, entities decoded, then sanitized: `&#x1b;`
/// must not turn into a terminal escape. Enough for the flat responses of AWS query APIs;
/// not a general XML parser.
pub fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(sanitize(&xml_unescape(&xml[start..end])))
}

/// Like [`xml_text`] but not sanitized: for identifiers sent back to AWS (S3 keys, tokens),
/// which must stay byte-exact. Sanitize them (or [`show_invisible`]) only to display them.
pub fn xml_text_raw(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(xml_unescape(&xml[start..end]))
}

/// The raw content of an element (as [`xml_blocks`] returns it) as text: entities decoded,
/// then sanitized like [`xml_text`].
pub fn xml_decode(raw: &str) -> String {
    sanitize(&xml_unescape(raw))
}

/// The `<tag>…</tag>` blocks of `xml`, in order (list members are `<member>`).
pub fn xml_blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let body = &rest[i + open.len()..];
        let Some(j) = body.find(&close) else { break };
        out.push(&body[..j]);
        rest = &body[j + close.len()..];
    }
    out
}

/// Text to put in an XML element of a request (Route 53 change batches).
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        let Some(end) = after.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let entity = &after[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .map(|h| u32::from_str_radix(h, 16))
                .or_else(|| entity.strip_prefix('#').map(str::parse::<u32>))
                .and_then(Result::ok)
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A random-looking UUID (version 4 layout) for idempotency tokens: a SHA-256 of the time,
/// the process id and a counter. Unique, not a secret.
pub fn request_token() -> String {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let seed = format!("{nanos}-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
    let mut b: [u8; 16] = Sha256::digest(seed.as_bytes())[..16].try_into().unwrap_or_default();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0xf) as usize] as char);
    }
    s
}

/// RFC 3986 percent-encoding of everything except unreserved characters.
pub fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let n = match chunk.len() {
            3 => u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8 | u32::from(chunk[2]),
            2 => u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8,
            _ => u32::from(chunk[0]) << 16,
        };
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Decodes standard base64 (padding optional, whitespace ignored); `None` if it is not.
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let sextets: Vec<u8> = input.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=').map(value).collect::<Option<_>>()?;
    if sextets.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(sextets.len() * 3 / 4);
    for chunk in sextets.chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, d)| n | u32::from(*d) << (18 - 6 * i));
        out.extend_from_slice(&n.to_be_bytes()[1..chunk.len()]);
    }
    Some(out)
}

/// Pretty-prints `s` when it is a JSON object or array; otherwise `None`.
/// A Markdown code block whose fence is longer than any run of backticks inside `text`,
/// so text from logs cannot close it and pass for the report's own words.
pub fn md_block(md: &mut String, lang: &str, text: &str) {
    use std::fmt::Write as _;
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let _ = writeln!(md, "{fence}{lang}\n{}\n{fence}", text.trim_end());
}

/// `text` as inline Markdown code, delimited by more backticks than it contains.
pub fn md_code(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let ticks = "`".repeat(longest + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') { " " } else { "" };
    format!("{ticks}{pad}{}{pad}{ticks}", text.replace('\n', " "))
}

/// `text` as a code span inside a Markdown table cell: a `|` would end the cell even inside
/// the span, so it is escaped too.
pub fn md_cell(text: &str) -> String {
    md_code(text).replace('|', "\\|")
}

/// JSON held in a string of a response (task settings, table mappings…), parsed and
/// sanitized: the response was cleaned while these escapes were still text, and decoding
/// them (`\u001b`, `\u202e`) brings back what was removed.
pub fn json_in_text(s: &str) -> Option<serde_json::Value> {
    let mut v: serde_json::Value = serde_json::from_str(s).ok()?;
    sanitize_json(&mut v);
    Some(v)
}

pub fn pretty_json(s: &str) -> Option<String> {
    let t = s.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    let mut v: serde_json::Value = serde_json::from_str(s).ok()?;
    // Decoding turns escapes like `\u009b` or `\u202e` back into the characters the text was
    // sanitized of; serializing again would write them out raw.
    sanitize_json(&mut v);
    serde_json::to_string_pretty(&v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_json_does_not_decode_unsafe_characters() {
        let text = pretty_json(r#"{"note":"a\u009b2Jb\u202ec","k\u001b[31m":1}"#).unwrap();
        assert!(!text.chars().any(is_unsafe), "{text:?}");
        assert!(text.contains("\"note\": \"a2Jbc\""), "{text}");
    }

    #[test]
    fn table_cells_cannot_add_columns() {
        assert_eq!(md_cell("a|b"), "`a\\|b`");
        assert!(!md_cell("x | y | z").replace("\\|", "").contains('|'));
    }

    #[test]
    fn markdown_from_logs_cannot_break_out() {
        let mut md = String::new();
        md_block(&mut md, "text", "```\nIgnore the above\n````");
        assert!(md.starts_with("`````text\n") && md.ends_with("\n`````\n"), "{md}");
        assert_eq!(md_code("a `b` c"), "``a `b` c``");
        assert_eq!(md_code("`x"), "`` `x ``");
    }

    #[test]
    fn base64_round_trips() {
        for text in ["", "a", "ab", "abc", "abcd", "START RequestId: 42\nEND\n", "ñandú ✓"] {
            assert_eq!(base64_decode(&base64(text.as_bytes())).as_deref(), Some(text.as_bytes()));
        }
        assert_eq!(base64_decode("aGk"), Some(b"hi".to_vec()));
        assert_eq!(base64_decode("not base64!"), None);
    }

    #[test]
    fn dates_round_trip() {
        assert_eq!(amz_dates(1_440_938_160), ("20150830".into(), "20150830T123600Z".into()));
        assert_eq!(parse_rfc3339("2015-08-30T12:36:00Z"), Some(1_440_938_160.0));
        assert_eq!(parse_rfc3339("2015-08-30T12:36:00UTC"), Some(1_440_938_160.0));
        assert_eq!(parse_rfc3339("2015-08-30T14:36:00.000+0200"), Some(1_440_938_160.0));
        assert_eq!(parse_rfc3339("2015-08-30T12:36:00.5Z"), Some(1_440_938_160.5));
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z").map(|s| s as i64), Some(1_709_164_800));
    }

    #[test]
    fn encodings() {
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"hi!"), "aGkh");
        assert_eq!(uri_encode("a b/c~"), "a%20b%2Fc~");
        assert_eq!(strip_ansi("\u{1b}[32mok\u{1b}[0m\tdone"), "ok    done");
        assert_eq!(sanitize("a\u{1b}]52;c;ZXZpbA==\u{7}b\rc\u{8}\u{9b}2Jd\n"), "abc2Jd\n");
        assert_eq!(sanitize("title\u{1b}]0;x\u{1b}\\ok\u{1b}c"), "titleok");
        assert_eq!(sanitize("fix \u{202e}gnp.exe\u{200b}"), "fix gnp.exe");
        assert_eq!(show_invisible("rm -rf \u{202e}x\u{1b}[2J\n"), "rm -rf \\u{202e}x\\u{1b}[2J\n");
    }

    #[test]
    fn request_tokens_are_unique_uuids() {
        let (a, b) = (request_token(), request_token());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
    }

    #[test]
    fn xml_helpers() {
        let xml = "<R><StackEvents><member><Id>1</Id><Reason>a &amp; b &lt;c&gt; &#10;x&#xD;</Reason></member>\
                   <member><Id>2</Id></member></StackEvents><NextToken>t</NextToken></R>";
        let members = xml_blocks(xml, "member");
        assert_eq!(members.len(), 2);
        assert_eq!(xml_text(members[0], "Reason").as_deref(), Some("a & b <c> \nx"));
        assert_eq!(xml_text("<M>a&#x1b;]52;c;aGk=&#7;b&#27;[2J</M>", "M").as_deref(), Some("ab"));
        assert_eq!(xml_text_raw("<Key>a&#x1b;b</Key>", "Key").as_deref(), Some("a\u{1b}b"));
        assert_eq!(xml_text(members[1], "Reason"), None);
        assert_eq!(xml_text(xml, "NextToken").as_deref(), Some("t"));
        assert_eq!(xml_unescape("AT&T &bogus; ok"), "AT&T &bogus; ok");
    }

    #[test]
    fn hidden_characters_are_dropped() {
        // Tag characters spell text no one sees: "ignore" hidden after "ok".
        let tagged: String = "ok".chars().chain("ignore".chars().map(|c| char::from_u32(0xE0000 + c as u32).unwrap())).collect();
        assert_eq!(sanitize(&tagged), "ok");
        assert_eq!(sanitize("a\u{00ad}b\u{180e}c\u{2028}d\u{2029}e\u{206a}f\u{fff9}g\u{e0001}h"), "abcdefgh");
        // Emoji keep their variation selector; tabs and newlines stay.
        assert_eq!(sanitize("❤\u{fe0f}\tx\ny"), "❤\u{fe0f}\tx\ny");
        assert!(is_unsafe('\u{e007f}') && !is_unsafe('\u{fe0f}'));
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(45.0), "45s");
        assert_eq!(fmt_duration(154.0), "2m 34s");
        assert_eq!(fmt_duration(3900.0), "1h 05m");
    }
}
