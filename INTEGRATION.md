# Option A patch — scancode output via Interception instead of SendInput

**Goal:** make `tartarus_driver` keystrokes visible to World of Tanks (and any
DirectInput/Raw Input title) by emitting real keyboard **scancodes** through the
Interception kernel driver rather than `SendInput`.

**Why it's blocked today:** `SendInput` stamps every event with the OS-level
"injected" flag (`LLKHF_INJECTED`). Games reading Raw Input or DirectInput, and
anti-cheat that inspects the flag, discard those events. The upstream README
confirms this and states there is no reliable way to strip the flag.

**Why this works:** Interception is a kernel-mode filter driver in the keyboard
class stack. It deals in make/break scancodes — what physical hardware puts on
the wire — and injects *below* where the injected flag is applied. The stroke is
indistinguishable from a real keypress because at that layer it is one.

**This is not an anti-cheat bypass.** It is the same class of input Razer
Synapse itself produces, and the same path the repo already uses for D-pad /
wheel / middle-click remapping. It changes *how your own keypad's keys reach the
OS*, not what the game does with them.

---

## Files

- `interception_out.rs` — drop-in module, self-contained FFI, with tests.

## Steps

### 1. Add the module

Copy `interception_out.rs` into `tartarus_driver/src/`, then in `main.rs`:

```rust
mod interception_out;
```

### 2. Route `send_key` through it, with SendInput as fallback

Find `fn send_key(vk: VIRTUAL_KEY, up: bool)` in `main.rs`. **Rename the
existing body** to `send_key_sendinput` (unchanged), and add:

```rust
pub fn send_key(vk: VIRTUAL_KEY, up: bool) {
    // Real scancodes first: visible to DirectInput/Raw Input titles.
    if interception_out::send_key(vk, up) {
        return;
    }
    // Fallback: no Interception driver, or a VK with no scancode
    // (media/volume/browser keys). Those are shell-consumed, so the
    // injected flag is irrelevant for them.
    send_key_sendinput(vk, up);
}
```

Every existing call site (`process_key_depths`, `force_keyup_on_layer_change`,
`emulate.rs`'s cleanup loop, hypershift's modifier passthrough) keeps working
unchanged — the signature is identical.

### 3. Link the library

In `tartarus_driver/Cargo.toml` the crate already depends on Interception for
the D-pad path, so linking is likely already configured. If not, add a
`build.rs`:

```rust
fn main() {
    println!("cargo:rustc-link-search=native=./lib");
    println!("cargo:rustc-link-lib=dylib=interception");
}
```

and put `interception.lib` / `interception.dll` (x64) in `tartarus_driver/lib/`.

### 4. Do NOT "clean this up" to use the `interception` crate

An earlier draft of these notes suggested reusing `dpad.rs`'s safe
`interception` crate binding instead of raw FFI. **That is wrong, and the
reason matters**, so it is recorded here rather than silently dropped.

The crate's `Stroke::Keyboard { code, .. }` takes a **`ScanCode`**, a C-like
enum. It converts one way only — `code as u16`, see `dpad.rs:298` — and there
is no `u16 -> ScanCode`. `transmute`-ing an arbitrary `u16` into it is
undefined behaviour for any value that is not a declared variant.

This module must send whatever scancode `MapVirtualKeyW` returns for a
**user-chosen** binding, which is arbitrary by definition. Raw FFI takes a
plain `u16` and avoids the enum entirely.

`dpad.rs` gets away with the crate because it only ever *compares* incoming
scancodes against a handful of named constants and forwards strokes unmodified
— it never constructs one from a number.

Two contexts in one process are fine: `dpad`'s filters and receives, this one
only sends.

### 5. Make `explicit_scan_code` visible

`vk_to_scancode` prefers `main.rs`'s existing `explicit_scan_code()` for the six
modifier keys whose left/right variants share a scancode and differ only by the
E0 flag — its own comment calls that the hardware-verified path. It is currently
a private `fn`, so change it to:

```rust
pub(crate) fn explicit_scan_code(vk: VIRTUAL_KEY) -> Option<(u16, bool)> {
```

Everything else falls through to `MapVirtualKeyW`.

---

## Verification (do these in order)

1. `cargo test` — the conversion tests run without hardware or the driver.
2. Driver present: `interception_out::available()` logs the chosen device on
   first use. If it logs a fallback warning, the driver isn't installed —
   run `install-interception.exe /install` **as admin, then reboot**.
3. Desktop sanity: keys type into Notepad as before.
4. **Extended-key check** — the one most likely to be wrong. In Notepad or any
   text field, confirm arrows move the caret and do NOT type numpad digits.
   If Up types `8`, the E0 flag isn't being set.
5. **World of Tanks**: bind a Tartarus key to a movement key and confirm the
   tank responds. This is the actual acceptance test.

## Known risks

- **Interception's presence** is itself flagged by some aggressive anti-cheats
  (upstream README notes this). WoT's is comparatively light, and you already
  run the driver for the D-pad path, so this is likely a non-issue — but it is
  the thing to verify rather than assume.
- **Requires a reboot** after installing the driver; it will not work before
  that and the failure mode is a silent fallback to SendInput.
- **Media/volume keys keep using SendInput** by design — they have no set-1
  scancode. If you have those bound on the wheel, they will still work, just
  via the old path.

## Not done here

The repo could not be forked from this session (the GitHub token lacks fork
scope). Fork `ultramonaka/open-tartarus-driver` in the browser, then apply this
on a branch.
