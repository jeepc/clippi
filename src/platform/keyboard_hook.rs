//! --- Low-level keyboard hook ---
//!
//! Feeds keystrokes to the main window while it is shown *without* taking the
//! foreground (so the target application keeps its blinking caret).
//!
//! A window that is not the foreground window never receives `WM_KEYDOWN`, so
//! the popup would be deaf. `WH_KEYBOARD_LL` sees every keystroke system-wide;
//! we re-post the interesting ones to our own HWND and swallow them so they
//! don't also land in the app the user is typing into. GPUI dispatches keyboard
//! messages to its own focus handle and never consults `GetFocus`, so posted
//! messages drive the search box and list exactly like real ones.
//!
//! Two things deliberately pass through untouched:
//! - **Alt / Win chords** — Alt+Tab, the Start menu, and the app's own Alt-based
//!   hotkeys must keep working. A low-level hook runs *before* `RegisterHotKey`,
//!   so swallowing them would kill the hotkey that opened the window.
//! - **The registered app hotkeys** (see [`set_passthrough_hotkeys`]) — same
//!   reason, for hotkeys that use no Alt (e.g. `Ctrl+Shift+V`).
//!
//! GPUI reads modifier state through `GetKeyState`, which reflects the *calling
//! thread's* input state — and posted messages never update it. The UI thread
//! must therefore call [`sync_thread_keyboard_state`] regularly while the hook
//! is installed, or Ctrl/Shift chords (and capital letters) arrive stripped of
//! their modifiers.

#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
#[cfg(target_os = "windows")]
use std::sync::Mutex;

#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, GetKeyboardState, SetKeyboardState, VK_CAPITAL, VK_CONTROL,
    VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN,
    VK_SHIFT,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, IsWindowVisible, PostMessageW,
    PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HC_ACTION,
    KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
    WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

/// HWND that receives the forwarded keystrokes. 0 = hook inactive.
#[cfg(target_os = "windows")]
static TARGET_HWND: AtomicIsize = AtomicIsize::new(0);

/// Installed hook handle (HHOOK as isize), owned by the pump thread.
#[cfg(target_os = "windows")]
static HOOK: AtomicIsize = AtomicIsize::new(0);

/// Thread id of the message pump that owns the hook.
#[cfg(target_os = "windows")]
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

#[cfg(target_os = "windows")]
static INSTALLED: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "windows")]
static PUMP_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// Key chords that must reach `RegisterHotKey` instead of our window.
/// Each entry is `(ctrl, shift, vk)`; Alt/Win chords pass through anyway.
#[cfg(target_os = "windows")]
static PASSTHROUGH: Mutex<Vec<(bool, bool, u16)>> = Mutex::new(Vec::new());

// --- ── Public API ── ---

/// Register the app hotkeys that must keep firing while the hook is installed.
///
/// A `WH_KEYBOARD_LL` hook sees keys before `RegisterHotKey` does, so any chord
/// we swallow can no longer toggle the window. Alt/Win-based hotkeys (the
/// defaults, `Alt+V` / `Alt+C`) already pass through; this covers the rest.
#[cfg(target_os = "windows")]
pub fn set_passthrough_hotkeys(hotkeys: &[String]) {
    let parsed: Vec<(bool, bool, u16)> = hotkeys.iter().filter_map(|h| parse_hotkey(h)).collect();
    if let Ok(mut guard) = PASSTHROUGH.lock() {
        *guard = parsed;
    }
}

/// Start forwarding keystrokes to `hwnd`. Idempotent — a second call while
/// installed just re-points the hook at `hwnd`.
#[cfg(target_os = "windows")]
pub fn install(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    TARGET_HWND.store(hwnd, Ordering::SeqCst);

    if INSTALLED.swap(true, Ordering::SeqCst) {
        return; // already running
    }

    let (tx, rx) = std::sync::mpsc::sync_channel::<u32>(0);

    let handle = std::thread::spawn(move || {
        // SAFETY: `SetWindowsHookExW` with a null module handle and thread id 0
        // installs a global low-level hook owned by this thread. The callback
        // signature matches HOOKPROC. The hook is removed below, before the
        // thread exits, so the callback can never outlive the process.
        let hook = unsafe {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(ll_keyboard_proc),
                std::ptr::null_mut(),
                0,
            )
        };
        if hook.is_null() {
            log::error!("SetWindowsHookExW(WH_KEYBOARD_LL) failed");
            INSTALLED.store(false, Ordering::SeqCst);
            let _ = tx.send(0);
            return;
        }
        HOOK.store(hook as isize, Ordering::SeqCst);

        // SAFETY: `GetCurrentThreadId` is always safe.
        let tid = unsafe { GetCurrentThreadId() };
        let _ = tx.send(tid); // blocks until `install` reads it

        // A low-level hook only fires while its owning thread pumps messages.
        // `GetMessageW` blocks (no busy-wait) and still dispatches hook callbacks.
        // SAFETY: standard message pump over a stack-allocated MSG.
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                if msg.message == WM_QUIT {
                    break;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            UnhookWindowsHookEx(hook);
        }
        HOOK.store(0, Ordering::SeqCst);
    });

    let tid = rx.recv().unwrap_or(0);
    HOOK_THREAD_ID.store(tid, Ordering::SeqCst);
    if tid == 0 {
        // The hook failed to install; the thread already exited.
        let _ = handle.join();
        return;
    }
    if let Ok(mut guard) = PUMP_THREAD.lock() {
        *guard = Some(handle);
    }
}

/// Stop forwarding and remove the hook. Idempotent.
#[cfg(target_os = "windows")]
pub fn uninstall() {
    TARGET_HWND.store(0, Ordering::SeqCst);
    if !INSTALLED.swap(false, Ordering::SeqCst) {
        return;
    }

    let tid = HOOK_THREAD_ID.swap(0, Ordering::SeqCst);
    if tid != 0 {
        // SAFETY: posts WM_QUIT to the pump thread, which then unhooks and exits.
        unsafe { PostThreadMessageW(tid, WM_QUIT, 0, 0) };
    }
    let handle = PUMP_THREAD.lock().ok().and_then(|mut guard| guard.take());
    if let Some(handle) = handle {
        let _ = handle.join();
    }
}

/// Copy the real modifier state into the *calling thread's* key-state table.
///
/// Must be called on the GPUI UI thread, and often (the UI thread's table is
/// what `GetKeyState` reads). Posted key messages don't update that table, so
/// without this GPUI sees every keystroke as unmodified: no Ctrl chords, and
/// `TranslateMessage` produces lowercase characters for shifted keys.
#[cfg(target_os = "windows")]
pub fn sync_thread_keyboard_state() {
    const MODIFIER_KEYS: [u16; 11] = [
        VK_SHIFT,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_CONTROL,
        VK_LCONTROL,
        VK_RCONTROL,
        VK_MENU,
        VK_LMENU,
        VK_RMENU,
        VK_LWIN,
        VK_RWIN,
    ];

    // SAFETY: `GetKeyboardState`/`SetKeyboardState` read and write a 256-byte
    // array for the calling thread; the buffer is correctly sized.
    unsafe {
        let mut state = [0u8; 256];
        if GetKeyboardState(state.as_mut_ptr()) == 0 {
            return;
        }
        for vk in MODIFIER_KEYS {
            // High bit of the byte = key is down, matching GetKeyState's contract.
            state[vk as usize] = if is_physically_down(vk) { 0x80 } else { 0 };
        }
        // Caps Lock is a toggle: preserve the low bit, which is what
        // TranslateMessage consults for letter case.
        let caps_on = (GetKeyState(VK_CAPITAL as i32) & 1) != 0;
        state[VK_CAPITAL as usize] = (state[VK_CAPITAL as usize] & 0x80) | u8::from(caps_on);
        SetKeyboardState(state.as_ptr());
    }
}

// --- ── Hook internals ── ---

/// True physical key state. Unlike `GetKeyState` this is not per-thread, so it
/// is correct even though our window is not the foreground window.
#[cfg(target_os = "windows")]
fn is_physically_down(vk: u16) -> bool {
    // SAFETY: `GetAsyncKeyState` is a read-only query, safe from any thread.
    (unsafe { GetAsyncKeyState(vk as i32) } & i16::MIN) != 0
}

#[cfg(target_os = "windows")]
fn is_passthrough_chord(vk: u16, ctrl: bool, shift: bool) -> bool {
    PASSTHROUGH
        .lock()
        .map(|chords| {
            chords
                .iter()
                .any(|&(c, s, k)| k == vk && c == ctrl && s == shift)
        })
        .unwrap_or(false)
}

/// Rebuild the `lParam` a real `WM_KEYDOWN`/`WM_KEYUP` would carry, so that
/// GPUI's `is_held` check and its `TranslateMessage` call behave as usual.
#[cfg(target_os = "windows")]
fn key_lparam(scan_code: u32, flags: u32, is_down: bool) -> isize {
    let mut lparam: u32 = 1; // repeat count
    lparam |= (scan_code & 0xFF) << 16;
    if flags & LLKHF_EXTENDED != 0 {
        lparam |= 1 << 24;
    }
    if !is_down {
        lparam |= 1 << 30; // previous state: key was down
        lparam |= 1 << 31; // transition state: key is being released
    }
    lparam as i32 as isize
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn ll_keyboard_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    let pass = |code: i32, wparam: usize, lparam: isize| -> isize {
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    };

    if code != HC_ACTION as i32 {
        return pass(code, wparam, lparam);
    }

    let hwnd = TARGET_HWND.load(Ordering::Relaxed);
    // Safety net: never eat the user's keyboard if the popup isn't actually up.
    if hwnd == 0 || IsWindowVisible(hwnd as *mut std::ffi::c_void) == 0 {
        return pass(code, wparam, lparam);
    }

    // SAFETY: for HC_ACTION, lparam points to a KBDLLHOOKSTRUCT owned by the OS
    // and valid for the duration of this callback.
    let kb = &*(lparam as *const KBDLLHOOKSTRUCT);

    // Our own paste keystrokes (SendInput in platform::paste) must not be
    // swallowed or bounced back at us.
    if kb.flags & LLKHF_INJECTED != 0 {
        return pass(code, wparam, lparam);
    }

    let vk = kb.vkCode as u16;
    let is_down = wparam == WM_KEYDOWN as usize || wparam == WM_SYSKEYDOWN as usize;
    let is_up = wparam == WM_KEYUP as usize || wparam == WM_SYSKEYUP as usize;
    if !is_down && !is_up {
        return pass(code, wparam, lparam);
    }

    let alt = is_physically_down(VK_MENU);
    let win = is_physically_down(VK_LWIN) || is_physically_down(VK_RWIN);
    let ctrl = is_physically_down(VK_CONTROL);
    let shift = is_physically_down(VK_SHIFT);

    // Alt/Win chords belong to the OS (Alt+Tab, Start menu) and to the app's own
    // Alt-based hotkeys, which a low-level hook would otherwise swallow.
    if alt || win {
        return pass(code, wparam, lparam);
    }

    // Modifier keys themselves produce no text; letting them through keeps the
    // target app's own modifier state coherent. GPUI reads them via GetKeyState,
    // which `sync_thread_keyboard_state` keeps up to date.
    if matches!(
        vk,
        VK_SHIFT
            | VK_LSHIFT
            | VK_RSHIFT
            | VK_CONTROL
            | VK_LCONTROL
            | VK_RCONTROL
            | VK_MENU
            | VK_LMENU
            | VK_RMENU
            | VK_LWIN
            | VK_RWIN
            | VK_CAPITAL
    ) {
        return pass(code, wparam, lparam);
    }

    if is_passthrough_chord(vk, ctrl, shift) {
        return pass(code, wparam, lparam);
    }

    // Forward to our (unfocused) window and swallow it, so the keystroke doesn't
    // also reach the app the user was typing in.
    let msg = if is_down { WM_KEYDOWN } else { WM_KEYUP };
    PostMessageW(
        hwnd as *mut std::ffi::c_void,
        msg,
        vk as usize,
        key_lparam(kb.scanCode, kb.flags, is_down),
    );
    1
}

/// Parse `"Ctrl+Shift+V"` into `(ctrl, shift, vk)`. Alt/Win chords are ignored —
/// the hook already passes those through.
#[cfg(target_os = "windows")]
fn parse_hotkey(hotkey: &str) -> Option<(bool, bool, u16)> {
    let mut ctrl = false;
    let mut shift = false;
    let mut key: Option<u16> = None;

    for part in hotkey.split('+') {
        let part = part.trim();
        match part.to_ascii_lowercase().as_str() {
            "" => continue,
            "ctrl" | "control" | "cmd" | "command" | "super" | "meta" => ctrl = true,
            "shift" => shift = true,
            // Alt/Win chords never reach the swallow path — nothing to record.
            "alt" | "option" | "win" | "windows" => return None,
            other => key = key_name_to_vk(other),
        }
    }

    key.map(|vk| (ctrl, shift, vk))
}

/// Map a hotkey key name to a Win32 virtual-key code.
#[cfg(target_os = "windows")]
fn key_name_to_vk(name: &str) -> Option<u16> {
    let bytes = name.as_bytes();
    if bytes.len() == 1 {
        let c = bytes[0].to_ascii_uppercase();
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c as u16); // VK_A..VK_Z / VK_0..VK_9 match ASCII
        }
    }
    if let Some(n) = name.strip_prefix('f') {
        if let Ok(n) = n.parse::<u16>() {
            if (1..=24).contains(&n) {
                return Some(0x70 + n - 1); // VK_F1..VK_F24
            }
        }
    }
    match name {
        "space" => Some(0x20),
        "tab" => Some(0x09),
        "enter" | "return" => Some(0x0D),
        "escape" | "esc" => Some(0x1B),
        "backspace" => Some(0x08),
        "delete" | "del" => Some(0x2E),
        "insert" | "ins" => Some(0x2D),
        "home" => Some(0x24),
        "end" => Some(0x23),
        "pageup" => Some(0x21),
        "pagedown" => Some(0x22),
        "up" | "arrowup" => Some(0x26),
        "down" | "arrowdown" => Some(0x28),
        "left" | "arrowleft" => Some(0x25),
        "right" | "arrowright" => Some(0x27),
        _ => None,
    }
}

// --- ── Non-Windows stubs ── ---

#[cfg(not(target_os = "windows"))]
pub fn set_passthrough_hotkeys(_hotkeys: &[String]) {}

#[cfg(not(target_os = "windows"))]
pub fn install(_hwnd: isize) {}

#[cfg(not(target_os = "windows"))]
pub fn uninstall() {}

#[cfg(not(target_os = "windows"))]
pub fn sync_thread_keyboard_state() {}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn parses_ctrl_shift_chord() {
        assert_eq!(parse_hotkey("Ctrl+Shift+V"), Some((true, true, b'V' as u16)));
    }

    #[test]
    fn parses_ctrl_chord() {
        assert_eq!(parse_hotkey("Ctrl+Space"), Some((true, false, 0x20)));
    }

    #[test]
    fn ignores_alt_and_win_chords() {
        // These already pass through the hook, so they must not be recorded.
        assert_eq!(parse_hotkey("Alt+V"), None);
        assert_eq!(parse_hotkey("Win+V"), None);
    }

    #[test]
    fn parses_function_key() {
        assert_eq!(parse_hotkey("Ctrl+F2"), Some((true, false, 0x71)));
    }

    #[test]
    fn key_lparam_marks_key_release() {
        let down = key_lparam(0x2F, 0, true);
        let up = key_lparam(0x2F, 0, false);
        assert_eq!(down & (1 << 16 | 0xFF << 16), 0x2F << 16);
        assert_eq!(down & (1 << 31), 0);
        assert_ne!(up & (1 << 31), 0);
    }
}
