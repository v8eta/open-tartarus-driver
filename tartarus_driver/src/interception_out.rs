// interception_out.rs — send keystrokes as REAL keyboard scancodes via the
// Interception kernel driver, instead of SendInput.
//
// WHY THIS EXISTS
// ---------------
// send_key() in main.rs uses SendInput, which stamps every event with the
// OS-level "injected" flag (LLKHF_INJECTED). Titles that read Raw Input or
// DirectInput — and any anti-cheat that inspects the flag — discard those
// events. World of Tanks is one of them: the driver works everywhere on the
// desktop and does nothing in-game.
//
// Interception is a kernel-mode filter driver sitting in the keyboard class
// stack. It deals in scancodes (make/break codes) — exactly what physical
// hardware puts on the wire — and injects BELOW the point where the injected
// flag is applied. A stroke sent this way is indistinguishable from a real
// keypress because, as far as the input stack is concerned, it is one.
//
// WHY RAW FFI AND NOT THE `interception` CRATE
// --------------------------------------------
// dpad.rs uses the safe `interception` crate (0.1.2), and reusing its binding
// would look tidier — but it CANNOT express this module's job. The crate's
// `Stroke::Keyboard { code, .. }` takes a `ScanCode`, a C-like enum. It
// converts one way only (`code as u16`, see dpad.rs:298); there is no
// `u16 -> ScanCode`, and `transmute`-ing an arbitrary u16 into it is undefined
// behaviour for any value that isn't a declared variant.
//
// This module must send whatever scancode `MapVirtualKeyW` returns for a
// user-chosen binding, which is arbitrary by definition. Raw FFI takes a plain
// u16 and sidesteps the enum entirely. The two contexts coexist fine — dpad's
// filters and receives, this one only sends.
//
// Requires: Interception driver installed (`install-interception.exe /install`,
// then reboot) and interception.dll + interception.lib available to the build.

#![allow(non_camel_case_types)]

use std::sync::OnceLock;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, MAPVK_VK_TO_VSC_EX, VIRTUAL_KEY,
};

// ---------------------------------------------------------------------------
// Interception FFI (mirrors interception.h)
// ---------------------------------------------------------------------------

type InterceptionContext = *mut core::ffi::c_void;
type InterceptionDevice = i32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InterceptionKeyStroke {
    code: u16,        // scancode (set 1 make code), WITHOUT the E0/E1 prefix
    state: u16,       // KEY_DOWN/KEY_UP | E0/E1
    information: u32, // passthrough, unused
}

const INTERCEPTION_KEY_DOWN: u16 = 0x00;
const INTERCEPTION_KEY_UP: u16 = 0x01;
const INTERCEPTION_KEY_E0: u16 = 0x02;

#[link(name = "interception")]
extern "C" {
    fn interception_create_context() -> InterceptionContext;
    fn interception_destroy_context(context: InterceptionContext);
    fn interception_send(
        context: InterceptionContext,
        device: InterceptionDevice,
        stroke: *const InterceptionKeyStroke,
        nstroke: u32,
    ) -> i32;
    fn interception_is_keyboard(device: InterceptionDevice) -> i32;
}

// ---------------------------------------------------------------------------
// Context + device selection
// ---------------------------------------------------------------------------

struct Ctx {
    context: InterceptionContext,
    device: InterceptionDevice,
}

// SAFETY: InterceptionContext is an opaque handle the driver owns; the API is
// documented as usable from any thread. We never mutate through the pointer
// on the Rust side.
unsafe impl Send for Ctx {}
unsafe impl Sync for Ctx {}

static CTX: OnceLock<Option<Ctx>> = OnceLock::new();

// Interception addresses devices by index. Keyboards are 1..=10
// (INTERCEPTION_KEYBOARD(n) == n + 1). We pick the FIRST index the driver
// reports as a keyboard.
//
// Note: this is an injection target, not a device we read from. Strokes are
// delivered as though they originated from that device, which is why the
// game cannot tell them apart from the real keyboard's own traffic.
fn pick_keyboard(_context: InterceptionContext) -> Option<InterceptionDevice> {
    (1..=10).find(|&d| unsafe { interception_is_keyboard(d) } != 0)
}

fn ctx() -> Option<&'static Ctx> {
    CTX.get_or_init(|| {
        let context = unsafe { interception_create_context() };
        if context.is_null() {
            eprintln!(
                "[interception] create_context failed — is the Interception driver installed? \
                 Falling back to SendInput."
            );
            return None;
        }
        match pick_keyboard(context) {
            Some(device) => {
                eprintln!("[interception] using keyboard device {device} for scancode output");
                Some(Ctx { context, device })
            }
            None => {
                eprintln!(
                    "[interception] no keyboard device found (1-10) — falling back to SendInput."
                );
                unsafe { interception_destroy_context(context) };
                None
            }
        }
    })
    .as_ref()
}

/// True when scancode output is available. Callers use this to decide whether
/// to route through here or fall back to SendInput.
pub fn available() -> bool {
    ctx().is_some()
}

// ---------------------------------------------------------------------------
// VK -> scancode
// ---------------------------------------------------------------------------

/// Convert a virtual-key code to (scancode, is_extended).
///
/// MAPVK_VK_TO_VSC_EX returns the scancode in the low byte and, for extended
/// keys, an 0xE0 prefix in the HIGH byte. Interception wants the bare
/// scancode plus the E0 flag in `state`, so they are split here.
///
/// Getting this wrong is the classic bug in this conversion: arrows,
/// Ins/Del/Home/End/PgUp/PgDn, right Ctrl, right Alt, numpad Enter and
/// numpad `/` are ALL extended. Drop the E0 and you send the numpad
/// equivalent instead — e.g. Up arrow arrives as numpad 8.
fn vk_to_scancode(vk: VIRTUAL_KEY) -> Option<(u16, bool)> {
    // main.rs already carries a hand-verified table for the six modifier keys
    // whose left/right variants share a scancode and differ only by the E0
    // flag (its comment calls it the "hardware-verified path"). Prefer it —
    // MapVirtualKeyW is correct for these too, but there is no reason to
    // second-guess a table someone confirmed against real hardware.
    if let Some(hit) = crate::explicit_scan_code(vk) {
        return Some(hit);
    }
    let raw = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC_EX) };
    if raw == 0 {
        return None; // no scancode for this VK (media/browser keys — see below)
    }
    let extended = (raw >> 8) as u8 == 0xE0;
    Some(((raw & 0xFF) as u16, extended))
}

// ---------------------------------------------------------------------------
// Public API — mirrors main.rs's send_key(vk, up)
// ---------------------------------------------------------------------------

/// Send one key event as a real scancode. `up == true` is a release.
///
/// Returns false if the key could not be sent this way (no context, or the VK
/// has no scancode) so the caller can fall back to SendInput. Media and
/// browser keys (VK_MEDIA_*, VK_VOLUME_*, VK_BROWSER_*) legitimately have no
/// set-1 scancode and MUST keep using SendInput — they are consumed by the
/// shell, not by games, so the injected flag is irrelevant for them.
pub fn send_key(vk: VIRTUAL_KEY, up: bool) -> bool {
    let Some(c) = ctx() else { return false };
    let Some((code, extended)) = vk_to_scancode(vk) else {
        return false;
    };

    let mut state = if up { INTERCEPTION_KEY_UP } else { INTERCEPTION_KEY_DOWN };
    if extended {
        state |= INTERCEPTION_KEY_E0;
    }

    let stroke = InterceptionKeyStroke { code, state, information: 0 };
    let sent = unsafe { interception_send(c.context, c.device, &stroke, 1) };
    sent == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VK_A, VK_LEFT, VK_MEDIA_PLAY_PAUSE, VK_NUMPAD8, VK_RCONTROL, VK_RETURN,
    };

    // Pure conversion tests — no driver, no hardware, safe in CI.
    #[test]
    fn vk_to_scancode_marks_extended_keys() {
        // Ordinary key: not extended.
        let (a, ext) = vk_to_scancode(VK_A).expect("A has a scancode");
        assert_eq!(a, 0x1E);
        assert!(!ext);

        // Main Enter: not extended.
        let (ret, ext) = vk_to_scancode(VK_RETURN).expect("Enter has a scancode");
        assert_eq!(ret, 0x1C);
        assert!(!ext);

        // MALFORMED-INPUT / boundary cases, per the estate rule that a test
        // must include input the code is not expecting:

        // Left arrow IS extended and shares its bare scancode (0x4B) with
        // numpad 4. Without the E0 flag it would arrive as numpad 4 — this
        // assertion is the whole reason vk_to_scancode returns a bool.
        let (left, ext) = vk_to_scancode(VK_LEFT).expect("Left has a scancode");
        assert!(ext, "Left arrow must be flagged extended");

        // Numpad 8 is NOT extended and must not collide with Up arrow.
        let (np8, ext8) = vk_to_scancode(VK_NUMPAD8).expect("Numpad8 has a scancode");
        assert!(!ext8);
        assert_ne!((left, ext), (np8, ext8));

        // Right Ctrl is extended; left Ctrl is not. Same bare scancode.
        let (_, rctrl_ext) = vk_to_scancode(VK_RCONTROL).expect("RCtrl has a scancode");
        assert!(rctrl_ext);

        // Media keys have NO set-1 scancode -> None, so the caller falls
        // back to SendInput rather than sending scancode 0.
        assert!(
            vk_to_scancode(VK_MEDIA_PLAY_PAUSE).is_none(),
            "media keys must report no scancode so the caller falls back"
        );

        // A nonsense VK must not panic and must not produce a bogus stroke.
        assert!(vk_to_scancode(VIRTUAL_KEY(0)).is_none());
        assert!(vk_to_scancode(VIRTUAL_KEY(0xFF)).is_none());
    }
}
