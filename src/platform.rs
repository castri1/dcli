//! OS integration without extra crates: shell commands, desktop notifications,
//! clipboard, opening URLs and private temporary files. Everything here is best-effort
//! and never blocks the UI.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// A command run through the platform shell (`sh -c` / `cmd /C`).
pub fn shell(command: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.arg("/C").raw_arg(command);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}

fn spawn_quiet(mut cmd: Command) -> bool {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok()
}

pub fn notify(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let script = format!("display notification {} with title {}", quote(body), quote(title));
        let mut cmd = Command::new("osascript");
        cmd.arg("-e").arg(script);
        spawn_quiet(cmd);
    }
    #[cfg(windows)]
    {
        let quote = |s: &str| s.replace('\'', "''");
        let script = format!(
            "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; \
             $t = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
             $x = $t.GetElementsByTagName('text'); \
             $x.Item(0).AppendChild($t.CreateTextNode('{}')) > $null; \
             $x.Item(1).AppendChild($t.CreateTextNode('{}')) > $null; \
             [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show([Windows.UI.Notifications.ToastNotification]::new($t))",
            quote(title),
            quote(body)
        );
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script]);
        spawn_quiet(cmd);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut cmd = Command::new("notify-send");
        cmd.args(["--app-name=dcli", title, body]);
        spawn_quiet(cmd);
    }
}

/// Opens `url` in the browser. Only http(s) URLs with a host: links can come from AWS
/// (an approval's ExternalEntityLink, say), and the system opener would also run files,
/// `file:` paths and other schemes.
pub fn open_url(url: &str) -> bool {
    if !is_web_url(url) {
        return false;
    }
    #[cfg(target_os = "macos")]
    let cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(windows)]
    let cmd = {
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    spawn_quiet(cmd)
}

pub fn is_web_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")) else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty() && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn pipe_to(program: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let written = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    written && child.wait().is_ok_and(|s| s.success())
}

/// Copies to the system clipboard; falls back to the OSC 52 escape sequence,
/// which most modern terminals (and SSH sessions) understand.
pub fn copy_to_clipboard(text: &str) -> bool {
    // Pasted into a terminal, escape sequences or invisible characters would act or hide.
    let text = &crate::util::sanitize(text);
    let native = if cfg!(target_os = "macos") {
        pipe_to("pbcopy", &[], text)
    } else if cfg!(windows) {
        // Read stdin as UTF-8, not the console code page, so non-ASCII text survives.
        pipe_to(
            "powershell",
            &["-NoProfile", "-Command", "[Console]::InputEncoding = [Text.Encoding]::UTF8; $input | Set-Clipboard"],
            text,
        )
    } else {
        pipe_to("wl-copy", &[], text)
            || pipe_to("xclip", &["-selection", "clipboard"], text)
            || pipe_to("xsel", &["--clipboard", "--input"], text)
    };
    if native {
        return true;
    }
    let mut out = std::io::stdout();
    write!(out, "\u{1b}]52;c;{}\u{7}", crate::util::base64(text.as_bytes())).is_ok() && out.flush().is_ok()
}

/// Writes `content` to a new file in the temp folder, named `dcli-<label>-<random>.md`.
/// The file is created exclusively (an existing file or symlink with that name is never
/// followed) and readable only by the user, since the temp folder may be shared.
pub fn write_temp_file(label: &str, content: &str) -> std::io::Result<PathBuf> {
    let label: String =
        label.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    let mut last_error = None;
    for attempt in 0..8u32 {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let suffix = format!("{:x}", std::process::id() ^ nanos.rotate_left(attempt * 4));
        let path = std::env::temp_dir().join(format!("dcli-{label}-{suffix}.md"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(content.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_error = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_error.unwrap_or_else(|| std::io::Error::other("no free file name")))
}

/// Held while one dcli changes a file ([`lock`]): an operating-system lock on a file next to
/// it, released when this goes, or by the system when the process dies (so none is ever
/// left behind or taken over).
#[derive(Debug)]
pub struct FileLock(#[allow(dead_code)] std::fs::File);

/// Waits (up to 5 s) to be the only dcli changing `path`: an exclusive lock on the file
/// `.<name>.lock` next to it, which stays in place between uses.
pub fn lock(path: &std::path::Path) -> std::io::Result<FileLock> {
    lock_within(path, std::time::Duration::from_secs(5))
}

fn lock_within(path: &std::path::Path, wait: std::time::Duration) -> std::io::Result<FileLock> {
    use std::time::{Duration, Instant};
    let dir = path.parent().ok_or_else(|| std::io::Error::other(format!("no folder for {}", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(dir.join(format!(".{name}.lock")))?;
    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(FileLock(file)),
            Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < wait => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(std::io::Error::other(format!("another dcli is changing {}: try again", path.display())));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// Replaces the file at `path` with `content`, readable only by the user: written whole to a
/// new file next to it (never following a link there), flushed to disk, then moved over the
/// old one. Each write has its own temporary name, so threads writing at once do not meet.
pub fn write_private(path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = path.parent().ok_or_else(|| std::io::Error::other(format!("no folder for {}", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.{}.{n}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&tmp).and_then(|mut f| f.write_all(content).and_then(|()| f.sync_all()));
    written.and_then(|()| std::fs::rename(&tmp, path)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_only_web_urls() {
        assert!(is_web_url("https://us-east-1.console.aws.amazon.com/codesuite/x?region=us-east-1"));
        assert!(is_web_url("http://127.0.0.1:47123/bucket/key"));
        for bad in ["file:///etc/passwd", "/Applications/Calculator.app", "smb://host/share", "https://", "javascript:alert(1)", "https://a b"] {
            assert!(!is_web_url(bad), "{bad}");
        }
    }

    #[test]
    fn locks_exclude_each_other_and_free_when_dropped() {
        let dir = std::env::temp_dir().join(format!("dcli-lock-{}", std::process::id()));
        let path = dir.join("usage.json");
        let held = lock(&path).unwrap();
        let wait = std::time::Duration::from_millis(100);
        assert!(lock_within(&path, wait).unwrap_err().to_string().contains("another dcli"));
        drop(held);
        assert!(lock_within(&path, wait).is_ok(), "free again once the guard goes");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn temp_files_are_new_and_private() {
        let a = write_temp_file("../x/app dev", "one").unwrap();
        let b = write_temp_file("../x/app dev", "two").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(std::env::temp_dir().as_path()));
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("dcli-.._x_app_dev-"));
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }
}
