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
    } else {
        format!("{:.1} MB", n / (1024.0 * 1024.0))
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

/// Bidi overrides and zero-width characters: invisible, and able to make text read
/// differently from what it is.
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}')
}

fn is_unsafe(c: char) -> bool {
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

/// Pretty-prints `s` when it is a JSON object or array; otherwise `None`.
pub fn pretty_json(s: &str) -> Option<String> {
    let t = s.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    serde_json::to_string_pretty(&v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn durations() {
        assert_eq!(fmt_duration(45.0), "45s");
        assert_eq!(fmt_duration(154.0), "2m 34s");
        assert_eq!(fmt_duration(3900.0), "1h 05m");
    }
}
