use std::io::Write;

/// Copies via OSC 52 (works over ssh and in tmux with `set-clipboard on`) and the native
/// clipboard (covers terminals without OSC 52, like VTE ones). Both are best effort.
pub struct Clipboard {
    /// Kept alive: on X11 the process owning the selection has to serve it.
    native: Option<arboard::Clipboard>,
}

impl Clipboard {
    pub fn new() -> Self {
        Self {
            native: arboard::Clipboard::new().ok(),
        }
    }

    pub fn copy(&mut self, text: &str) {
        // On macOS the process is always local, so the pasteboard suffices; also writing it via
        // OSC 52 races the terminal's own write, and AppKit logs the failure onto the screen.
        if cfg!(target_os = "macos")
            && let Some(c) = &mut self.native
            && c.set_text(text).is_ok()
        {
            return;
        }
        let mut out = std::io::stdout();
        let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
        let _ = out.flush();
        if let Some(c) = &mut self.native {
            let _ = c.set_text(text);
        }
    }
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            s.push(if i <= chunk.len() {
                ABC[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    s
}
