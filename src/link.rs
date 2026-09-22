//! Opening and copying URLs.
//!
//! This VM is headless — no `DISPLAY`, no `WAYLAND_DISPLAY` — so `xdg-open`
//! exists but has nothing to open into, and a browser launched here would be a
//! browser nobody can see. The useful move is to put the URL on the clipboard of
//! the terminal you are actually sitting in, which is a different machine.
//!
//! OSC 52 does exactly that: the escape travels back over the same pty as the
//! rendering, so the copy lands on the local clipboard across ssh, with no
//! X11 forwarding, no `pbcopy`/`wl-copy` in the guest, and nothing to install.
//! Requires the terminal to allow clipboard writes (ghostty: `clipboard-write`).

use std::io::Write;

/// Is there a graphical session to open a browser into?
fn has_display() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

fn opener() -> Option<&'static str> {
    ["xdg-open", "sensible-browser"]
        .into_iter()
        .find(|candidate| which(candidate))
}

fn which(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| d.join(name).is_file())
}

/// Minimal base64. A dependency for one 60-character string, in a program whose
/// whole dependency list is four crates, is not worth it.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Put `text` on the terminal's clipboard via OSC 52.
///
/// Written straight to stdout mid-render: the alternate screen is a byte stream
/// like any other and this escape draws nothing, so it does not disturb what
/// ratatui has on screen. Flushed immediately, since the next frame would
/// otherwise sit in front of it.
pub fn copy(text: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

/// What happened, for the footer to report.
pub enum Outcome {
    Opened,
    Copied,
    Failed(String),
}

/// Open the URL if there is anywhere to open it, and otherwise copy it. Copying
/// is the normal path on this machine, not the fallback it looks like.
pub fn open_or_copy(url: &str) -> Outcome {
    if has_display() {
        if let Some(cmd) = opener() {
            match std::process::Command::new(cmd)
                .arg(url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(_) => return Outcome::Opened,
                Err(e) => return Outcome::Failed(format!("{cmd}: {e}")),
            }
        }
    }
    match copy(url) {
        Ok(()) => Outcome::Copied,
        Err(e) => Outcome::Failed(format!("clipboard: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_matches_rfc4648_including_padding() {
        // The three residue cases are the only place a hand-rolled encoder
        // usually goes wrong.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(
            base64(b"https://github.com/votingworks/vxsuite/pull/9069"),
            "aHR0cHM6Ly9naXRodWIuY29tL3ZvdGluZ3dvcmtzL3Z4c3VpdGUvcHVsbC85MDY5"
        );
    }
}
