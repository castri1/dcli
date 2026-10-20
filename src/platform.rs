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

pub fn open_url(url: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

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
