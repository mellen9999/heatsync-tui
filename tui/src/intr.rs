//! ctrl-c / SIGINT handling. in the chat ui raw mode turns ctrl-c into a key
//! event (handled like `q`); everywhere else — emote loading, `login`, the
//! one-shot commands — the terminal is cooked and ctrl-c would kill us with
//! status 130. the handler turns that into a quiet exit 0 instead. a SIGINT
//! that arrives from outside while the ui is up (`kill -INT`) only sets a flag
//! the draw loop polls, so the terminal is always restored by the normal path.

use std::sync::atomic::{AtomicU8, Ordering};

const PLAIN: u8 = 0;
const LOGIN: u8 = 1;
const TUI: u8 = 2;
const SEEN: u8 = 3;

static STATE: AtomicU8 = AtomicU8::new(PLAIN);

/// what to say when ctrl-c lands (None = nothing).
fn message(state: u8) -> Option<&'static [u8]> {
    match state {
        LOGIN => Some(b"\nlogin cancelled\n"),
        _ => None,
    }
}

#[cfg(unix)]
extern "C" fn on_sigint(_: libc::c_int) {
    // async-signal-safe only: atomics, write, _exit.
    let st = STATE.load(Ordering::SeqCst);
    if st == TUI || st == SEEN {
        STATE.store(SEEN, Ordering::SeqCst);
        return;
    }
    if let Some(m) = message(st) {
        unsafe { libc::write(2, m.as_ptr().cast(), m.len()) };
    }
    unsafe { libc::_exit(0) };
}

/// call once at startup.
pub fn install() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as usize);
    }
}

pub fn login_phase() {
    STATE.store(LOGIN, Ordering::SeqCst);
}

pub fn plain_phase() {
    STATE.store(PLAIN, Ordering::SeqCst);
}

pub fn tui_phase() {
    STATE.store(TUI, Ordering::SeqCst);
}

/// true once a SIGINT arrived while the ui was up.
pub fn requested() -> bool {
    STATE.load(Ordering::SeqCst) == SEEN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_login_speaks() {
        assert!(message(LOGIN).is_some_and(|m| m.starts_with(b"\nlogin")));
        assert!(message(PLAIN).is_none());
    }

    #[test]
    fn tui_sigint_sets_flag_not_exit() {
        tui_phase();
        assert!(!requested());
        on_sigint(2);
        assert!(requested());
        plain_phase();
    }
}
