//! yank target. tries the system clipboard first, then falls back to OSC 52 so
//! a yank still lands over ssh, where there is no local clipboard daemon to talk
//! to. returns a short label naming which path worked, for the status line.

use std::io::Write;
use std::process::{Command, Stdio};

/// copy `text` to the clipboard. `Err` carries something worth showing the user.
pub fn copy(text: &str) -> Result<&'static str, String> {
    // wayland, then X — whichever tool is present. a missing binary is not an
    // error, it just means we try the next path.
    for (bin, args) in [("wl-copy", &[][..]), ("xclip", &["-selection", "clipboard"])] {
        match pipe_to(bin, args, text) {
            Ok(true) => return Ok(bin),
            Ok(false) => continue, // not installed
            Err(e) => return Err(e),
        }
    }
    osc52(text).map(|_| "osc52")
}

/// spawn `bin` and write `text` to its stdin. `Ok(false)` = not installed.
fn pipe_to(bin: &str, args: &[&str], text: &str) -> Result<bool, String> {
    let child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("{bin}: {e}")),
    };
    child
        .stdin
        .take()
        .ok_or_else(|| format!("{bin}: no stdin"))?
        .write_all(text.as_bytes())
        .map_err(|e| format!("{bin}: {e}"))?;
    // wl-copy forks a server that holds the selection; don't block on it.
    Ok(true)
}

/// ask the terminal itself to set the clipboard (OSC 52). wrapped for tmux,
/// which otherwise swallows the sequence instead of passing it to the outer
/// terminal. many terminals require this to be enabled, so it is the last resort.
fn osc52(text: &str) -> Result<(), String> {
    let payload = b64(text.as_bytes());
    let seq = format!("\x1b]52;c;{payload}\x07");
    let out = if std::env::var_os("TMUX").is_some() {
        format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
    } else {
        seq
    };
    let mut stdout = std::io::stdout();
    stdout.write_all(out.as_bytes()).map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())
}

/// standard base64. tiny and self-contained — a whole crate for one call would
/// not earn its weight.
fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::b64;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foob"), "Zm9vYg==");
        assert_eq!(b64(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_non_ascii_payloads() {
        // emote names and chat are utf-8; encoding works on bytes, not chars.
        assert_eq!(b64("héllo".as_bytes()), "aMOpbGxv");
    }
}
