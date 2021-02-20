use std::{
    char, collections::HashSet, ffi::OsString, mem::MaybeUninit, os::raw::c_int,
    os::windows::ffi::OsStringExt, sync::MutexGuard,
};

use winapi::{
    shared::{
        minwindef::{HKL, LPARAM, LRESULT, UINT, WPARAM},
        windef::HWND,
    },
    um::winuser,
};

use crate::{
    event::{ElementState, KeyEvent},
    keyboard::{Key, KeyCode, KeyLocation, NativeKeyCode},
    platform::scancode::KeyCodeExtScancode,
    platform_impl::platform::{
        keyboard_layout::{get_or_insert_str, LayoutCache, WindowsModifiers, LAYOUT_CACHE},
        KeyEventExtra,
    },
};

pub fn is_msg_keyboard_related(msg: u32) -> bool {
    use winuser::{WM_KEYFIRST, WM_KEYLAST, WM_KILLFOCUS, WM_SETFOCUS};
    let is_keyboard_msg = WM_KEYFIRST <= msg && msg <= WM_KEYLAST;

    is_keyboard_msg || msg == WM_SETFOCUS || msg == WM_KILLFOCUS
}

#[derive(Debug, Copy, Clone)]
pub struct KeyLParam {
    pub scancode: u8,
    pub extended: bool,

    /// This is `previous_state XOR transition_state`. See the lParam for WM_KEYDOWN and WM_KEYUP for further details.
    pub is_repeat: bool,
}

pub type ExScancode = u16;

fn new_ex_scancode(scancode: u8, extended: bool) -> ExScancode {
    (scancode as u16) | (if extended { 0xE000 } else { 0 })
}

fn get_kbd_state() -> [u8; 256] {
    unsafe {
        let mut kbd_state: MaybeUninit<[u8; 256]> = MaybeUninit::uninit();
        winuser::GetKeyboardState(kbd_state.as_mut_ptr() as *mut u8);
        kbd_state.assume_init()
    }
}

pub struct MessageAsKeyEvent {
    pub event: KeyEvent,
    pub is_synthetic: bool,
}

/// Stores information required to make `KeyEvent`s.
///
/// A single Winit `KeyEvent` contains information which the Windows API passes to the application
/// in multiple window messages. In other words: a Winit `KeyEvent` cannot be built from a single
/// window message. Therefore, this type keeps track of certain information from previous events so
/// that a `KeyEvent` can be constructed when the last event related to a keypress is received.
///
/// `PeekMessage` is sometimes used to determine whether the next window message still belongs to the
/// current keypress. If it doesn't and the current state represents a key event waiting to be
/// dispatched, then said event is considered complete and is dispatched.
///
/// The sequence of window messages for a key press event is the following:
/// - Exactly one WM_KEYDOWN / WM_SYSKEYDOWN
/// - Zero or one WM_DEADCHAR / WM_SYSDEADCHAR
/// - Zero or more WM_CHAR / WM_SYSCHAR. These messages each come with a UTF-16 code unit which when
///   put together in the sequence they arrived in, forms the text which is the result of pressing the
///   key.
///
/// Key release messages are a bit different due to the fact that they don't contribute to
/// text input. The "sequence" only consists of one WM_KEYUP / WM_SYSKEYUP event.
pub struct KeyEventBuilder {
    event_info: Option<PartialKeyEventInfo>,

    /// The keyup event needs to call `ToUnicode` to determine what the text produced by the
    /// key with all modifiers except CTRL (the `logical_key`) is.
    ///
    /// But `ToUnicode` without the non-modifying flag (see `key_labels`), resets the dead key
    /// state which would be incorrect during every keyup event. Therefore this variable is used
    /// to determine whether the last keydown event produced a dead key.
    ///
    /// Note that this variable is not always correct because it does
    /// not track key presses outside of this window. However, the ONLY situation where this
    /// doesn't work as intended is when the user presses a dead key outside of this window, and
    /// switches to this window BEFORE releasing it then releases the dead key. In this case
    /// the `ToUnicode` function will be called, incorrectly clearing the dead key state. Having
    /// an inccorect behaviour only in this case seems acceptable.
    prev_down_was_dead: bool,
}
impl Default for KeyEventBuilder {
    fn default() -> Self {
        KeyEventBuilder {
            event_info: None,
            prev_down_was_dead: false,
        }
    }
}
impl KeyEventBuilder {
    /// Call this function for every window message.
    /// Returns Some() if this window message completes a KeyEvent.
    /// Returns None otherwise.
    pub fn process_message(
        &mut self,
        hwnd: HWND,
        msg_kind: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        retval: &mut Option<LRESULT>,
    ) -> Vec<MessageAsKeyEvent> {
        match msg_kind {
            winuser::WM_SETFOCUS => {
                // synthesize keydown events
                let key_events = self.synthesize_kbd_state(ElementState::Pressed);
                if !key_events.is_empty() {
                    return key_events;
                }
            }
            winuser::WM_KILLFOCUS => {
                // sythesize keyup events
                let key_events = self.synthesize_kbd_state(ElementState::Released);
                if !key_events.is_empty() {
                    return key_events;
                }
            }
            winuser::WM_KEYDOWN | winuser::WM_SYSKEYDOWN => {
                if msg_kind == winuser::WM_SYSKEYDOWN && wparam as i32 == winuser::VK_F4 {
                    // Don't dispatch Alt+F4 to the application.
                    // This is handled in `event_loop.rs`
                    return vec![];
                }
                self.prev_down_was_dead = false;

                let mut layouts = LAYOUT_CACHE.lock().unwrap();
                let event_info = PartialKeyEventInfo::from_message(
                    wparam,
                    lparam,
                    ElementState::Pressed,
                    &mut layouts,
                );
                let mut event_info = Some(event_info);

                let mut next_msg = MaybeUninit::uninit();
                let peek_retval = unsafe {
                    winuser::PeekMessageW(
                        next_msg.as_mut_ptr(),
                        hwnd,
                        winuser::WM_KEYFIRST,
                        winuser::WM_KEYLAST,
                        winuser::PM_NOREMOVE,
                    )
                };
                let has_next_key_message = peek_retval != 0;
                *retval = Some(0);
                self.event_info = None;
                if has_next_key_message {
                    let next_msg = unsafe { next_msg.assume_init().message };
                    let is_next_keydown =
                        next_msg == winuser::WM_KEYDOWN || next_msg == winuser::WM_SYSKEYDOWN;
                    if !is_next_keydown {
                        self.event_info = event_info.take();
                    }
                }
                if let Some(event_info) = event_info {
                    let ev = event_info.finalize(&mut layouts.strings);
                    return vec![MessageAsKeyEvent {
                        event: ev,
                        is_synthetic: false,
                    }];
                }
            }
            winuser::WM_DEADCHAR | winuser::WM_SYSDEADCHAR => {
                self.prev_down_was_dead = true;
                // At this point, we know that there isn't going to be any more events related to
                // this key press
                let event_info = self.event_info.take().unwrap();
                *retval = Some(0);
                let mut layouts = LAYOUT_CACHE.lock().unwrap();
                let ev = event_info.finalize(&mut layouts.strings);
                return vec![MessageAsKeyEvent {
                    event: ev,
                    is_synthetic: false,
                }];
            }
            winuser::WM_CHAR | winuser::WM_SYSCHAR => {
                *retval = Some(0);
                let is_high_surrogate = 0xD800 <= wparam && wparam <= 0xDBFF;
                let is_low_surrogate = 0xDC00 <= wparam && wparam <= 0xDFFF;

                let is_utf16 = is_high_surrogate || is_low_surrogate;

                let more_char_coming;
                unsafe {
                    let mut next_msg = MaybeUninit::uninit();
                    let has_message = winuser::PeekMessageW(
                        next_msg.as_mut_ptr(),
                        hwnd,
                        winuser::WM_KEYFIRST,
                        winuser::WM_KEYLAST,
                        winuser::PM_NOREMOVE,
                    );
                    let has_message = has_message != 0;
                    if !has_message {
                        more_char_coming = false;
                    } else {
                        let next_msg = next_msg.assume_init().message;
                        if next_msg == winuser::WM_CHAR || next_msg == winuser::WM_SYSCHAR {
                            more_char_coming = true;
                        } else {
                            more_char_coming = false;
                        }
                    }
                }

                if is_utf16 {
                    self.event_info
                        .as_mut()
                        .unwrap()
                        .utf16parts
                        .push(wparam as u16);
                } else {
                    // In this case, wparam holds a UTF-32 character.
                    // Let's encode it as UTF-16 and append it to the end of `utf16parts`
                    let utf16parts = &mut self.event_info.as_mut().unwrap().utf16parts;
                    let start_offset = utf16parts.len();
                    let new_size = utf16parts.len() + 2;
                    utf16parts.resize(new_size, 0);
                    if let Some(ch) = char::from_u32(wparam as u32) {
                        let encode_len = ch.encode_utf16(&mut utf16parts[start_offset..]).len();
                        let new_size = start_offset + encode_len;
                        utf16parts.resize(new_size, 0);
                    }
                }
                if !more_char_coming {
                    let mut event_info = self.event_info.take().unwrap();

                    let mut layouts = LAYOUT_CACHE.lock().unwrap();
                    // It's okay to call `ToUnicode` here, because at this point the dead key
                    // is already consumed by the character.
                    let kbd_state = get_kbd_state();
                    let mod_state = WindowsModifiers::active_modifiers(&kbd_state);

                    let (_, layout) = layouts.get_current_layout();
                    let ctrl_on;
                    if layout.has_alt_graph {
                        let alt_on = mod_state.contains(WindowsModifiers::ALT);
                        ctrl_on = !alt_on && mod_state.contains(WindowsModifiers::CONTROL)
                    } else {
                        ctrl_on = mod_state.contains(WindowsModifiers::CONTROL)
                    }

                    // If Ctrl is not pressed, just use the text with all
                    // modifiers because that already consumed the dead key. Otherwise,
                    // we would interpret the character incorrectly, missing the dead key.
                    if !ctrl_on {
                        event_info.text = PartialText::System(event_info.utf16parts.clone());
                    } else {
                        let mod_no_ctrl = mod_state.remove_only_ctrl();
                        let num_lock_on = kbd_state[winuser::VK_NUMLOCK as usize] & 1 != 0;
                        let vkey = event_info.vkey;
                        let scancode = event_info.scancode;
                        let keycode = event_info.code;
                        let key = layout.get_key(mod_no_ctrl, num_lock_on, vkey, scancode, keycode);
                        event_info.text = PartialText::Text(key.to_text());
                    }
                    let ev = event_info.finalize(&mut layouts.strings);
                    return vec![MessageAsKeyEvent {
                        event: ev,
                        is_synthetic: false,
                    }];
                }
            }
            winuser::WM_KEYUP | winuser::WM_SYSKEYUP => {
                *retval = Some(0);

                let mut layouts = LAYOUT_CACHE.lock().unwrap();
                let event_info = PartialKeyEventInfo::from_message(
                    wparam,
                    lparam,
                    ElementState::Released,
                    &mut layouts,
                );
                let event = event_info.finalize(&mut layouts.strings);
                return vec![MessageAsKeyEvent {
                    event,
                    is_synthetic: false,
                }];
            }
            _ => (),
        }

        Vec::new()
    }

    fn synthesize_kbd_state(&mut self, key_state: ElementState) -> Vec<MessageAsKeyEvent> {
        let mut key_events = Vec::new();

        let mut layouts = LAYOUT_CACHE.lock().unwrap();
        let (locale_id, _) = layouts.get_current_layout();

        let kbd_state = get_kbd_state();
        macro_rules! is_key_pressed {
            ($vk:expr) => {
                kbd_state[$vk as usize] & 0x80 != 0
            };
        }

        // Is caps-lock active? Note that this is different from caps-lock
        // being held down.
        let caps_lock_on = kbd_state[winuser::VK_CAPITAL as usize] & 1 != 0;
        let num_lock_on = kbd_state[winuser::VK_NUMLOCK as usize] & 1 != 0;

        // We are synthesizing the press event for caps-lock first for the following reasons:
        // 1. If caps-lock is *not* held down but *is* active, then we have to
        //    synthesize all printable keys, respecting the caps-lock state.
        // 2. If caps-lock is held down, we could choose to sythesize its
        //    keypress after every other key, in which case all other keys *must*
        //    be sythesized as if the caps-lock state was be the opposite
        //    of what it currently is.
        // --
        // For the sake of simplicity we are choosing to always sythesize
        // caps-lock first, and always use the current caps-lock state
        // to determine the produced text
        if is_key_pressed!(winuser::VK_CAPITAL) {
            let event = self.create_synthetic(
                winuser::VK_CAPITAL,
                key_state,
                caps_lock_on,
                num_lock_on,
                locale_id as HKL,
                &mut layouts,
            );
            if let Some(event) = event {
                key_events.push(event);
            }
        }
        let do_non_modifier = |key_events: &mut Vec<_>, layouts: &mut _| {
            for vk in 0..256 {
                match vk {
                    winuser::VK_CONTROL
                    | winuser::VK_LCONTROL
                    | winuser::VK_RCONTROL
                    | winuser::VK_SHIFT
                    | winuser::VK_LSHIFT
                    | winuser::VK_RSHIFT
                    | winuser::VK_MENU
                    | winuser::VK_LMENU
                    | winuser::VK_RMENU
                    | winuser::VK_CAPITAL => continue,
                    _ => (),
                }
                if !is_key_pressed!(vk) {
                    continue;
                }
                let event = self.create_synthetic(
                    vk,
                    key_state,
                    caps_lock_on,
                    num_lock_on,
                    locale_id as HKL,
                    layouts,
                );
                if let Some(event) = event {
                    key_events.push(event);
                }
            }
        };
        let do_modifier = |key_events: &mut Vec<_>, layouts: &mut _| {
            const CLEAR_MODIFIER_VKS: [i32; 6] = [
                winuser::VK_LCONTROL,
                winuser::VK_LSHIFT,
                winuser::VK_LMENU,
                winuser::VK_RCONTROL,
                winuser::VK_RSHIFT,
                winuser::VK_RMENU,
            ];
            for vk in CLEAR_MODIFIER_VKS.iter() {
                if is_key_pressed!(*vk) {
                    let event = self.create_synthetic(
                        *vk,
                        key_state,
                        caps_lock_on,
                        num_lock_on,
                        locale_id as HKL,
                        layouts,
                    );
                    if let Some(event) = event {
                        key_events.push(event);
                    }
                }
            }
        };

        // Be cheeky and sequence modifier and non-modifier
        // key events such that non-modifier keys are not affected
        // by modifiers (except for caps-lock)
        match key_state {
            ElementState::Pressed => {
                do_non_modifier(&mut key_events, &mut layouts);
                do_modifier(&mut key_events, &mut layouts);
            }
            ElementState::Released => {
                do_modifier(&mut key_events, &mut layouts);
                do_non_modifier(&mut key_events, &mut layouts);
            }
        }

        key_events
    }

    fn create_synthetic(
        &self,
        vk: i32,
        key_state: ElementState,
        caps_lock_on: bool,
        num_lock_on: bool,
        locale_id: HKL,
        layouts: &mut MutexGuard<'_, LayoutCache>,
    ) -> Option<MessageAsKeyEvent> {
        let scancode = unsafe {
            winuser::MapVirtualKeyExW(vk as UINT, winuser::MAPVK_VK_TO_VSC_EX, locale_id)
        };
        if scancode == 0 {
            return None;
        }
        let scancode = scancode as ExScancode;
        let code = KeyCode::from_scancode(scancode as u32);
        let mods = if caps_lock_on {
            WindowsModifiers::CAPS_LOCK
        } else {
            WindowsModifiers::empty()
        };
        let layout = layouts.layouts.get(&(locale_id as u64)).unwrap();
        let logical_key = layout.get_key(mods, num_lock_on, vk, scancode, code);
        let key_without_modifiers =
            layout.get_key(WindowsModifiers::empty(), false, vk, scancode, code);
        let text;
        if key_state == ElementState::Pressed {
            text = logical_key.to_text();
        } else {
            text = None;
        }
        let event_info = PartialKeyEventInfo {
            vkey: vk,
            logical_key: PartialLogicalKey::This(logical_key),
            key_without_modifiers,
            key_state,
            scancode,
            is_repeat: false,
            code,
            location: get_location(scancode, locale_id),
            utf16parts: Vec::with_capacity(8),
            text: PartialText::Text(text),
        };

        let mut event = event_info.finalize(&mut layouts.strings);
        event.logical_key = logical_key;
        event.platform_specific.text_with_all_modifers = text;
        Some(MessageAsKeyEvent {
            event,
            is_synthetic: true,
        })
    }
}

enum PartialText {
    // Unicode
    System(Vec<u16>),
    Text(Option<&'static str>),
}

enum PartialLogicalKey {
    /// Use the text provided by the WM_UNICHAR messages and report that as
    /// a `Character` variant
    Text,

    /// Use the value directly provided by this variant
    This(Key<'static>),
}

struct PartialKeyEventInfo {
    vkey: c_int,
    scancode: ExScancode,
    key_state: ElementState,
    is_repeat: bool,
    code: KeyCode,
    location: KeyLocation,
    logical_key: PartialLogicalKey,

    key_without_modifiers: Key<'static>,

    /// The UTF-16 code units of the text that was produced by the keypress event.
    /// This take all modifiers into account. Including CTRL
    utf16parts: Vec<u16>,

    text: PartialText,
}

impl PartialKeyEventInfo {
    fn from_message(
        wparam: WPARAM,
        lparam: LPARAM,
        state: ElementState,
        layouts: &mut MutexGuard<'_, LayoutCache>,
    ) -> Self {
        const NO_MODS: WindowsModifiers = WindowsModifiers::empty();

        let (_, layout) = layouts.get_current_layout();
        let lparam_struct = destructure_key_lparam(lparam);
        let scancode;
        let vkey = wparam as c_int;
        if lparam_struct.scancode == 0 {
            // In some cases (often with media keys) the device reports a scancode of 0 but a
            // valid virtual key. In these cases we obtain the scancode from the virtual key.
            scancode = unsafe {
                winuser::MapVirtualKeyExW(
                    vkey as u32,
                    winuser::MAPVK_VK_TO_VSC_EX,
                    layout.hkl as HKL,
                ) as u16
            };
        } else {
            scancode = new_ex_scancode(lparam_struct.scancode, lparam_struct.extended);
        }
        let code = KeyCode::from_scancode(scancode as u32);
        let location = get_location(scancode, layout.hkl as HKL);

        let kbd_state = get_kbd_state();
        let mods = WindowsModifiers::active_modifiers(&kbd_state);
        let mods_without_ctrl = mods.remove_only_ctrl();
        let num_lock_on = kbd_state[winuser::VK_NUMLOCK as usize] & 1 != 0;

        // On Windows Ctrl+NumLock = Pause (and apparently Ctrl+Pause -> NumLock). In these cases
        // the KeyCode still stores the real key, so in the name of consistency across platforms, we
        // circumvent this mapping and force the key values to match the keycode.
        // For more on this, read the article by Raymond Chen, titled:
        // "Why does Ctrl+ScrollLock cancel dialogs?"
        // https://devblogs.microsoft.com/oldnewthing/20080211-00/?p=23503
        let code_as_key = if mods.contains(WindowsModifiers::CONTROL) {
            match code {
                KeyCode::NumLock => Some(Key::NumLock),
                KeyCode::Pause => Some(Key::Pause),
                _ => None,
            }
        } else {
            None
        };

        let preliminary_logical_key =
            layout.get_key(mods_without_ctrl, num_lock_on, vkey, scancode, code);
        let key_is_char = matches!(preliminary_logical_key, Key::Character(_));
        let is_pressed = state == ElementState::Pressed;

        let logical_key = if let Some(key) = code_as_key {
            PartialLogicalKey::This(key)
        } else if is_pressed && key_is_char && !mods.contains(WindowsModifiers::CONTROL) {
            // In some cases we want to use the UNICHAR text for logical_key in order to allow
            // dead keys to have an effect on the character reported by `logical_key`.
            PartialLogicalKey::Text
        } else {
            PartialLogicalKey::This(preliminary_logical_key)
        };
        let key_without_modifiers = if let Some(key) = code_as_key {
            key
        } else {
            match layout.get_key(NO_MODS, false, vkey, scancode, code) {
                // We convert dead keys into their character.
                // The reason for this is that `key_without_modifiers` is designed for key-bindings,
                // but the US International layout treats `'` (apostrophe) as a dead key and the
                // reguar US layout treats it a character. In order for a single binding
                // configuration to work with both layouts, we forward each dead key as a character.
                Key::Dead(k) => {
                    if let Some(ch) = k {
                        // I'm avoiding the heap allocation. I don't want to talk about it :(
                        let mut utf8 = [0; 4];
                        let s = ch.encode_utf8(&mut utf8);
                        let static_str = get_or_insert_str(&mut layouts.strings, s);
                        Key::Character(static_str)
                    } else {
                        Key::Unidentified(NativeKeyCode::Unidentified)
                    }
                }
                key => key,
            }
        };

        PartialKeyEventInfo {
            vkey,
            scancode,
            key_state: state,
            logical_key,
            key_without_modifiers,
            is_repeat: lparam_struct.is_repeat,
            code,
            location,
            utf16parts: Vec::with_capacity(8),
            text: PartialText::System(Vec::new()),
        }
    }

    fn finalize(self, strings: &mut HashSet<&'static str>) -> KeyEvent {
        let mut char_with_all_modifiers = None;
        if !self.utf16parts.is_empty() {
            let os_string = OsString::from_wide(&self.utf16parts);
            if let Ok(string) = os_string.into_string() {
                let static_str = get_or_insert_str(strings, string);
                char_with_all_modifiers = Some(static_str);
            }
        }

        // The text without Ctrl
        let mut text = None;
        match self.text {
            PartialText::System(wide) => {
                if !wide.is_empty() {
                    let os_string = OsString::from_wide(&wide);
                    if let Ok(string) = os_string.into_string() {
                        let static_str = get_or_insert_str(strings, string);
                        text = Some(static_str);
                    }
                }
            }
            PartialText::Text(s) => {
                text = s;
            }
        }

        let logical_key = match self.logical_key {
            PartialLogicalKey::Text => match text {
                Some(s) => Key::Character(s),
                None => Key::Unidentified(NativeKeyCode::Windows(self.scancode)),
            },
            PartialLogicalKey::This(v) => v,
        };

        KeyEvent {
            physical_key: self.code,
            logical_key,
            text,
            location: self.location,
            state: self.key_state,
            repeat: self.is_repeat,
            platform_specific: KeyEventExtra {
                text_with_all_modifers: char_with_all_modifiers,
                key_without_modifiers: self.key_without_modifiers,
            },
        }
    }
}

pub fn destructure_key_lparam(lparam: LPARAM) -> KeyLParam {
    let previous_state = (lparam >> 30) & 0x01;
    let transition_state = (lparam >> 31) & 0x01;
    KeyLParam {
        scancode: ((lparam >> 16) & 0xFF) as u8,
        extended: ((lparam >> 24) & 0x01) != 0,
        is_repeat: (previous_state ^ transition_state) != 0,
    }
}

fn get_location(scancode: ExScancode, hkl: HKL) -> KeyLocation {
    use winuser::*;
    const VK_ABNT_C2: c_int = 0xc2;

    let extension = 0xE000;
    let extended = (scancode & extension) == extension;
    let vkey = unsafe {
        winuser::MapVirtualKeyExW(scancode as u32, winuser::MAPVK_VSC_TO_VK_EX, hkl) as i32
    };

    // Use the native VKEY and the extended flag to cover most cases
    // This is taken from the `druid` GUI library, specifically
    // druid-shell/src/platform/windows/keyboard.rs
    match vkey {
        VK_LSHIFT | VK_LCONTROL | VK_LMENU | VK_LWIN => KeyLocation::Left,
        VK_RSHIFT | VK_RCONTROL | VK_RMENU | VK_RWIN => KeyLocation::Right,
        VK_RETURN if extended => KeyLocation::Numpad,
        VK_INSERT | VK_DELETE | VK_END | VK_DOWN | VK_NEXT | VK_LEFT | VK_CLEAR | VK_RIGHT
        | VK_HOME | VK_UP | VK_PRIOR => {
            if extended {
                KeyLocation::Standard
            } else {
                KeyLocation::Numpad
            }
        }
        VK_NUMPAD0 | VK_NUMPAD1 | VK_NUMPAD2 | VK_NUMPAD3 | VK_NUMPAD4 | VK_NUMPAD5
        | VK_NUMPAD6 | VK_NUMPAD7 | VK_NUMPAD8 | VK_NUMPAD9 | VK_DECIMAL | VK_DIVIDE
        | VK_MULTIPLY | VK_SUBTRACT | VK_ADD | VK_ABNT_C2 => KeyLocation::Numpad,
        _ => KeyLocation::Standard,
    }
}
