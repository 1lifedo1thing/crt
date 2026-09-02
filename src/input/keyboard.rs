//! Keyboard input handling
//!
//! Extracts keyboard event handling logic from main.rs for better modularity.
//! Returns actions that main.rs applies, keeping ownership/lifetime concerns there.

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use crt_core::Scroll;
use winit::event::{ElementState, KeyEvent, Modifiers};
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::config::{KeyAction, KeybindingsConfig};
use crate::window::{TabId, WindowState};

use super::{
    ShellKeyInput, TabEditResult, clear_search_cache, clear_terminal_selection,
    get_clipboard_content, get_terminal_selection_text, handle_shell_input, handle_tab_editing,
    paste_to_terminal, set_clipboard_content,
};

/// Result of keyboard event handling
#[derive(Debug)]
#[allow(dead_code)] // Variants exist for API completeness
pub enum KeyboardAction {
    /// No action needed (event fully handled)
    Handled,
    /// Event was not handled, continue default processing
    NotHandled,
    /// Close the current window
    CloseWindow,
    /// Close a specific tab
    CloseTab(TabId),
    /// Request a new window
    NewWindow,
    /// Request a new tab (main.rs handles creation with spawn options)
    NewTab,
    /// Quit the application
    Quit,
    /// Scroll the terminal
    Scroll(Scroll),
    /// Copy selection to clipboard
    Copy,
    /// Paste from clipboard
    Paste,
    /// Toggle search mode
    ToggleSearch,
    /// Navigate to next/previous search match (true = previous)
    SearchNavigate { reverse: bool },
    /// Switch to previous tab
    PrevTab,
    /// Switch to next tab
    NextTab,
    /// Select a specific tab by index (0-based)
    SelectTab(usize),
    /// Increase font size
    IncreaseFontSize,
    /// Decrease font size
    DecreaseFontSize,
    /// Reset font size to default
    ResetFontSize,
    /// Toggle fullscreen mode
    ToggleFullscreen,
    /// Open the config file in the default editor
    OpenConfig,
}

/// Read-only context for keyboard action determination.
///
/// Captures the minimal state needed to decide which action a key combination
/// should produce, without requiring access to the full `WindowState`.
#[allow(dead_code)] // some fields exist for API completeness
pub struct InputContext {
    /// Whether the context menu is currently visible
    pub context_menu_visible: bool,
    /// Whether a tab is being renamed
    pub tab_editing_active: bool,
    /// Whether the window rename dialog is active
    pub window_rename_active: bool,
    /// Whether search mode is active
    pub search_active: bool,
    /// Number of search matches (for navigation decisions)
    pub search_match_count: usize,
    /// Number of open tabs
    pub tab_count: usize,
    /// Active tab ID (if any)
    pub active_tab_id: Option<TabId>,
}

impl InputContext {
    /// Extract input context from window state
    pub fn from_state(state: &WindowState) -> Self {
        Self {
            context_menu_visible: state.ui.context_menu.visible,
            tab_editing_active: state.gpu.tab_bar.is_editing(),
            window_rename_active: state.ui.window_rename.active,
            search_active: state.ui.search.active,
            search_match_count: state.ui.search.matches.len(),
            tab_count: state.gpu.tab_bar.tab_count(),
            active_tab_id: state.gpu.tab_bar.active_tab_id(),
        }
    }
}

/// Platform-independent modifier flags relevant to bindings (Shift, Ctrl,
/// Alt, Super), stripped of any other bits.
fn relevant_mods(s: ModifiersState) -> ModifiersState {
    s & (ModifiersState::SHIFT
        | ModifiersState::CONTROL
        | ModifiersState::ALT
        | ModifiersState::SUPER)
}

/// Whether the *application* modifier chord is held.
///
/// This is the chord the hardcoded shortcuts (new window, search, ...) hang
/// off, and the chord the `"super"` binding token maps to:
/// - macOS: Cmd (Super)
/// - Linux/Windows: Ctrl+Shift, so plain Ctrl chords (`^C`, `^D`, `^Z`, ...)
///   always reach the shell.
pub fn app_modifier_held(s: ModifiersState) -> bool {
    #[cfg(target_os = "macos")]
    {
        s.super_key()
    }
    #[cfg(not(target_os = "macos"))]
    {
        s.control_key() && s.shift_key()
    }
}

/// The modifier chord the `"super"` / primary binding token resolves to.
fn primary_binding_mods() -> ModifiersState {
    #[cfg(target_os = "macos")]
    {
        ModifiersState::SUPER
    }
    #[cfg(not(target_os = "macos"))]
    {
        ModifiersState::CONTROL | ModifiersState::SHIFT
    }
}

/// Build the modifier signature a configured binding requires.
///
/// `"super"` (and its aliases) maps to the platform command chord — Cmd on
/// macOS, Ctrl+Shift elsewhere — so the default `super`-based bindings work
/// on every platform without stealing plain Ctrl chords from the shell.
/// `"ctrl"`, `"shift"` and `"alt"` are literal on every platform.
fn binding_mod_signature(mods: &[String]) -> ModifiersState {
    let mut sig = ModifiersState::empty();
    for m in mods {
        match m.to_ascii_lowercase().as_str() {
            "super" | "cmd" | "command" | "meta" | "win" | "primary" => {
                sig |= primary_binding_mods()
            }
            "ctrl" | "control" => sig |= ModifiersState::CONTROL,
            "shift" => sig |= ModifiersState::SHIFT,
            "alt" | "option" | "opt" => sig |= ModifiersState::ALT,
            _ => {}
        }
    }
    sig
}

/// Normalize a key token to a canonical lowercase form so that, e.g., `"="`,
/// `"+"` and `"equal"` all compare equal. Borrows when no change is needed.
fn normalize_key_token(token: &str) -> Cow<'_, str> {
    match token {
        "=" | "+" => Cow::Borrowed("equal"),
        "-" | "_" => Cow::Borrowed("minus"),
        "{" => Cow::Borrowed("["),
        "}" => Cow::Borrowed("]"),
        "comma" => Cow::Borrowed(","),
        other if other.bytes().any(|b| b.is_ascii_uppercase()) => {
            Cow::Owned(other.to_ascii_lowercase())
        }
        other => Cow::Borrowed(other),
    }
}

/// Static token for a function key, or `None` for other named keys.
fn function_key_token(named: &NamedKey) -> Option<&'static str> {
    Some(match named {
        NamedKey::F1 => "f1",
        NamedKey::F2 => "f2",
        NamedKey::F3 => "f3",
        NamedKey::F4 => "f4",
        NamedKey::F5 => "f5",
        NamedKey::F6 => "f6",
        NamedKey::F7 => "f7",
        NamedKey::F8 => "f8",
        NamedKey::F9 => "f9",
        NamedKey::F10 => "f10",
        NamedKey::F11 => "f11",
        NamedKey::F12 => "f12",
        NamedKey::F13 => "f13",
        NamedKey::F14 => "f14",
        NamedKey::F15 => "f15",
        NamedKey::F16 => "f16",
        NamedKey::F17 => "f17",
        NamedKey::F18 => "f18",
        NamedKey::F19 => "f19",
        NamedKey::F20 => "f20",
        NamedKey::F21 => "f21",
        NamedKey::F22 => "f22",
        NamedKey::F23 => "f23",
        NamedKey::F24 => "f24",
        _ => return None,
    })
}

/// Extract a normalized key token from a winit key, or `None` for keys that
/// can't be bound (e.g. plain modifier presses).
fn event_key_token(key: &Key) -> Option<Cow<'_, str>> {
    match key {
        Key::Character(c) => Some(normalize_key_token(c.as_str())),
        Key::Named(NamedKey::Space) => Some(Cow::Borrowed("space")),
        Key::Named(NamedKey::Tab) => Some(Cow::Borrowed("tab")),
        Key::Named(NamedKey::Enter) => Some(Cow::Borrowed("enter")),
        Key::Named(named) => function_key_token(named).map(Cow::Borrowed),
        _ => None,
    }
}

/// A binding with its key token and modifiers pre-normalized.
struct NormalizedBinding {
    token: String,
    mods: ModifiersState,
    action: KeyAction,
}

/// Normalized copy of a [`KeybindingsConfig`], rebuilt only when the config
/// changes (detected via a cheap fingerprint of its address and contents).
struct BindingCache {
    fingerprint: u64,
    bindings: Vec<NormalizedBinding>,
}

static BINDING_CACHE: Mutex<Option<BindingCache>> = Mutex::new(None);

fn binding_fingerprint(keybindings: &KeybindingsConfig) -> u64 {
    let mut h = DefaultHasher::new();
    (keybindings as *const KeybindingsConfig as usize).hash(&mut h);
    keybindings.bindings.len().hash(&mut h);
    for b in &keybindings.bindings {
        b.key.hash(&mut h);
        b.mods.hash(&mut h);
    }
    h.finish()
}

fn normalize_bindings(keybindings: &KeybindingsConfig) -> Vec<NormalizedBinding> {
    keybindings
        .bindings
        .iter()
        .map(|b| NormalizedBinding {
            token: normalize_key_token(&b.key).into_owned(),
            mods: binding_mod_signature(&b.mods),
            action: b.action.clone(),
        })
        .collect()
}

/// Run `f` against the normalized bindings for `keybindings`, rebuilding the
/// cached table if the config changed since the last call.
fn with_normalized_bindings<R>(
    keybindings: &KeybindingsConfig,
    f: impl FnOnce(&[NormalizedBinding]) -> R,
) -> R {
    let fingerprint = binding_fingerprint(keybindings);
    let mut guard = match BINDING_CACHE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let needs_rebuild = guard.as_ref().map(|c| c.fingerprint) != Some(fingerprint);
    if needs_rebuild {
        *guard = Some(BindingCache {
            fingerprint,
            bindings: normalize_bindings(keybindings),
        });
    }
    f(&guard.as_ref().expect("binding cache populated").bindings)
}

/// Resolve an incoming key event to a configured [`KeyAction`], if any binding
/// matches both the key and the exact modifier combination.
///
/// `unmodified_key` is the key with all modifiers removed (from
/// `KeyEventExtModifierSupplement::key_without_modifiers`), when available.
/// It lets `Ctrl+Shift+1` (logical `!`) or `Option+f` (logical `ƒ` on macOS)
/// still match bindings written as `1` / `f`.
pub fn resolve_keybinding_with_unmodified(
    keybindings: &KeybindingsConfig,
    key: &Key,
    unmodified_key: Option<&Key>,
    modifiers: &Modifiers,
) -> Option<KeyAction> {
    let token = event_key_token(key);
    let unmodified_token = unmodified_key.and_then(event_key_token);
    if token.is_none() && unmodified_token.is_none() {
        return None;
    }
    let event_sig = relevant_mods(modifiers.state());
    let matches = |b: &NormalizedBinding| {
        b.mods == event_sig
            && (token.as_deref() == Some(b.token.as_str())
                || unmodified_token.as_deref() == Some(b.token.as_str()))
    };
    with_normalized_bindings(keybindings, |bindings| {
        bindings
            .iter()
            .find(|b| matches(b))
            .map(|b| b.action.clone())
    })
}

/// Resolve an incoming key event to a configured [`KeyAction`] using only the
/// logical key (see [`resolve_keybinding_with_unmodified`]).
#[allow(dead_code)] // logical-key-only convenience, exercised by tests
pub fn resolve_keybinding(
    keybindings: &KeybindingsConfig,
    key: &Key,
    modifiers: &Modifiers,
) -> Option<KeyAction> {
    resolve_keybinding_with_unmodified(keybindings, key, None, modifiers)
}

/// Translate a configured [`KeyAction`] into a [`KeyboardAction`], given the
/// current input context (used to decide close-tab vs. close-window).
pub fn key_action_to_keyboard_action(action: &KeyAction, ctx: &InputContext) -> KeyboardAction {
    match action {
        KeyAction::Copy => KeyboardAction::Copy,
        KeyAction::Paste => KeyboardAction::Paste,
        KeyAction::Quit => KeyboardAction::Quit,
        KeyAction::NewTab => KeyboardAction::NewTab,
        KeyAction::CloseTab => {
            if ctx.tab_count > 1
                && let Some(tab_id) = ctx.active_tab_id
            {
                return KeyboardAction::CloseTab(tab_id);
            }
            KeyboardAction::CloseWindow
        }
        KeyAction::NextTab => KeyboardAction::NextTab,
        KeyAction::PrevTab => KeyboardAction::PrevTab,
        KeyAction::SelectTab1
        | KeyAction::SelectTab2
        | KeyAction::SelectTab3
        | KeyAction::SelectTab4
        | KeyAction::SelectTab5
        | KeyAction::SelectTab6
        | KeyAction::SelectTab7
        | KeyAction::SelectTab8
        | KeyAction::SelectTab9 => KeyboardAction::SelectTab(action.tab_index().unwrap_or(0)),
        KeyAction::IncreaseFontSize => KeyboardAction::IncreaseFontSize,
        KeyAction::DecreaseFontSize => KeyboardAction::DecreaseFontSize,
        KeyAction::ResetFontSize => KeyboardAction::ResetFontSize,
        KeyAction::ToggleFullscreen => KeyboardAction::ToggleFullscreen,
        KeyAction::OpenConfig => KeyboardAction::OpenConfig,
    }
}

/// Determine a hardcoded, non-configurable command shortcut (Cmd/Ctrl + key).
///
/// These actions have no [`KeyAction`] equivalent and are always available
/// regardless of the user's keybindings: new window, search, and search
/// navigation. Configurable shortcuts are resolved separately via
/// [`resolve_keybinding`]. Pure function — no side effects.
pub fn determine_command_shortcut(
    key: &Key,
    shift_pressed: bool,
    ctx: &InputContext,
) -> Option<KeyboardAction> {
    // Compare case-insensitively: on Linux the app chord includes Shift, so
    // the logical key arrives uppercase (`Ctrl+Shift+N` → "N").
    match key {
        Key::Character(c) if c.eq_ignore_ascii_case("n") => Some(KeyboardAction::NewWindow),
        Key::Character(c) if c.eq_ignore_ascii_case("f") => Some(KeyboardAction::ToggleSearch),
        Key::Character(c) if c.eq_ignore_ascii_case("g") => {
            if ctx.search_active && ctx.search_match_count > 0 {
                Some(KeyboardAction::SearchNavigate {
                    reverse: shift_pressed,
                })
            } else {
                Some(KeyboardAction::Handled)
            }
        }
        _ => None,
    }
}

/// Determine the scroll action for a key combination.
///
/// Pure function — returns the scroll action if the key is a scroll shortcut.
pub fn determine_scroll_action(
    key: &Key,
    mod_pressed: bool,
    shift_pressed: bool,
) -> Option<Scroll> {
    if !shift_pressed {
        return None;
    }

    match key {
        Key::Named(NamedKey::PageUp) if !mod_pressed => Some(Scroll::PageUp),
        Key::Named(NamedKey::PageDown) if !mod_pressed => Some(Scroll::PageDown),
        Key::Named(NamedKey::Home) if !mod_pressed => Some(Scroll::Top),
        Key::Named(NamedKey::End) if !mod_pressed => Some(Scroll::Bottom),
        #[cfg(target_os = "macos")]
        Key::Named(NamedKey::ArrowLeft) if mod_pressed => Some(Scroll::Top),
        #[cfg(target_os = "macos")]
        Key::Named(NamedKey::ArrowRight) if mod_pressed => Some(Scroll::Bottom),
        _ => None,
    }
}

/// Handle a full winit [`KeyEvent`].
///
/// Preferred entry point: unlike [`handle_keyboard_input`] it has access to
/// the key *without* modifiers, which is needed so that on macOS
/// `Option+f` (logical `ƒ`) sends `ESC f`, and so that bindings written as
/// `1` / `[` match `Ctrl+Shift+1` / `Ctrl+Shift+[` on Linux.
///
/// Returns the action that main.rs should take, if any.
#[allow(dead_code)] // wired from `WindowEvent::KeyboardInput` in app/handler.rs
pub fn handle_keyboard_event(
    state: &mut WindowState,
    event: &KeyEvent,
    modifiers: &Modifiers,
    keybindings: &KeybindingsConfig,
) -> KeyboardAction {
    if event.state != ElementState::Pressed {
        return KeyboardAction::NotHandled;
    }
    let unmodified = key_without_modifiers(event);
    handle_keyboard_input_inner(
        state,
        &event.logical_key,
        unmodified.as_ref(),
        event.text.as_deref(),
        modifiers,
        keybindings,
    )
}

/// The key with all modifiers removed, on platforms where winit provides it.
#[allow(dead_code)] // used by `handle_keyboard_event`
#[cfg(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd",
))]
fn key_without_modifiers(event: &KeyEvent) -> Option<Key> {
    use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
    Some(event.key_without_modifiers())
}

#[allow(dead_code)] // used by `handle_keyboard_event`
#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd",
)))]
fn key_without_modifiers(_event: &KeyEvent) -> Option<Key> {
    None
}

fn handle_keyboard_input_inner(
    state: &mut WindowState,
    key: &Key,
    unmodified_key: Option<&Key>,
    text: Option<&str>,
    modifiers: &Modifiers,
    keybindings: &KeybindingsConfig,
) -> KeyboardAction {
    // Keep the mouse layer's view of the modifiers fresh (used for xterm
    // modifier bits and Shift-to-bypass-reporting) until the lead wires
    // `set_modifiers` into `ModifiersChanged`.
    super::set_modifiers(modifiers);

    let mods = modifiers.state();
    // The application chord: Cmd on macOS, Ctrl+Shift elsewhere.
    let mod_pressed = app_modifier_held(mods);
    let super_pressed = mods.super_key();
    let shift_pressed = mods.shift_key();
    let ctrl_pressed = mods.control_key();
    // Whether any "command-like" modifier is held: text-entry widgets (tab
    // rename, search box) must not treat such chords as typed characters.
    let chord_pressed = mod_pressed || super_pressed || ctrl_pressed;
    // Shift beyond the app chord (macOS Cmd+Shift+G = previous match). On
    // Linux the chord already includes Shift, so there is no "extra" Shift;
    // Shift+Enter in the search box navigates backwards there.
    let shift_extra = shift_pressed && !primary_binding_mods().shift_key();

    // Handle scroll shortcuts (Shift+PageUp/PageDown/Home/End)
    if let Some(action) = handle_scroll_shortcuts(state, key, mod_pressed, shift_pressed) {
        return action;
    }

    // Handle context menu keyboard navigation
    if state.ui.context_menu.visible {
        match key {
            Key::Named(NamedKey::Escape) => {
                // Escape leaves the submenu first, then closes the menu
                if !state.ui.context_menu.leave_submenu() {
                    state.ui.context_menu.hide();
                }
                state.render.dirty = true;
                state.window.request_redraw();
                return KeyboardAction::Handled;
            }
            Key::Named(NamedKey::ArrowRight) => {
                if state.ui.context_menu.enter_submenu() {
                    state.render.dirty = true;
                    state.window.request_redraw();
                }
                return KeyboardAction::Handled;
            }
            Key::Named(NamedKey::ArrowLeft) => {
                if state.ui.context_menu.leave_submenu() {
                    state.render.dirty = true;
                    state.window.request_redraw();
                }
                return KeyboardAction::Handled;
            }
            Key::Named(NamedKey::ArrowDown) => {
                state.ui.context_menu.focus_next();
                state.render.dirty = true;
                state.window.request_redraw();
                return KeyboardAction::Handled;
            }
            Key::Named(NamedKey::ArrowUp) => {
                state.ui.context_menu.focus_prev();
                state.render.dirty = true;
                state.window.request_redraw();
                return KeyboardAction::Handled;
            }
            Key::Named(NamedKey::Enter) => {
                // Enter on "Themes" opens the submenu (returns None); on any
                // other item it activates it.
                if let Some(item) = state.ui.context_menu.activate_focused() {
                    super::mouse::handle_context_menu_action(state, item);
                    state.ui.context_menu.hide();
                }
                state.render.dirty = true;
                state.window.request_redraw();
                return KeyboardAction::Handled;
            }
            _ => {}
        }
    }

    // Handle tab editing
    if let TabEditResult::Handled = handle_tab_editing(state, key, chord_pressed) {
        return KeyboardAction::Handled;
    }

    // Handle window rename input
    if state.ui.window_rename.active {
        match key {
            Key::Named(NamedKey::Escape) => {
                state.ui.window_rename.cancel();
                state.render.dirty = true;
                state.window.request_redraw();
                return KeyboardAction::Handled;
            }
            Key::Named(NamedKey::Enter) => {
                if let Some(new_title) = state.ui.window_rename.confirm() {
                    state.custom_title = Some(new_title.clone());
                    state.window.set_title(&new_title);
                } else {
                    // Empty = reset to default
                    state.custom_title = None;
                    state.window.set_title("CRT Terminal");
                }
                state.render.dirty = true;
                state.window.request_redraw();
                return KeyboardAction::Handled;
            }
            Key::Named(NamedKey::Backspace) => {
                state.ui.window_rename.input.pop();
                state.render.dirty = true;
                state.window.request_redraw();
                return KeyboardAction::Handled;
            }
            Key::Character(c) => {
                for ch in c.chars() {
                    if !ch.is_control() {
                        state.ui.window_rename.input.push(ch);
                    }
                }
                state.render.dirty = true;
                state.window.request_redraw();
                return KeyboardAction::Handled;
            }
            Key::Named(NamedKey::Space) => {
                state.ui.window_rename.input.push(' ');
                state.render.dirty = true;
                state.window.request_redraw();
                return KeyboardAction::Handled;
            }
            _ => return KeyboardAction::Handled, // Consume all keys in rename mode
        }
    }

    // Handle search input when search is active
    if let Some(action) = handle_search_input(state, key, chord_pressed, shift_pressed) {
        return action;
    }

    // Resolve user-configurable keybindings first (these may include
    // no-modifier bindings such as F11 for fullscreen). A binding claims the
    // exact chord; anything unclaimed falls through to the shell.
    if let Some(action) =
        handle_configured_keybinding(state, key, unmodified_key, modifiers, keybindings)
    {
        return action;
    }

    // Handle hardcoded, non-configurable shortcuts (app chord + key)
    if mod_pressed {
        let shortcut_key = unmodified_key.unwrap_or(key);
        if let Some(action) = handle_command_shortcuts(state, shortcut_key, shift_extra) {
            return action;
        }
    }

    // Send to shell (clears selection on input). Super chords are never
    // forwarded; Ctrl chords are, unless a binding claimed them above.
    let input = ShellKeyInput {
        key,
        unmodified_key,
        text,
        modifiers: mods,
    };
    if handle_shell_input(state, &input) {
        clear_terminal_selection(state);
    }

    KeyboardAction::Handled
}

/// Handle scroll shortcuts (Shift+PageUp/PageDown/Home/End)
fn handle_scroll_shortcuts(
    state: &mut WindowState,
    key: &Key,
    mod_pressed: bool,
    shift_pressed: bool,
) -> Option<KeyboardAction> {
    let scroll = determine_scroll_action(key, mod_pressed, shift_pressed)?;

    let tab_id = state.gpu.tab_bar.active_tab_id();
    if let Some(tab_id) = tab_id
        && let Some(shell) = state.shells.get_mut(&tab_id)
    {
        shell.scroll(scroll);
        state.render.dirty = true;
        state.text_rebuild.insert(tab_id);
        state.window.request_redraw();
    }
    Some(KeyboardAction::Handled)
}

/// Handle search mode input
fn handle_search_input(
    state: &mut WindowState,
    key: &Key,
    chord_pressed: bool,
    shift_pressed: bool,
) -> Option<KeyboardAction> {
    if !state.ui.search.active {
        return None;
    }

    match key {
        Key::Named(NamedKey::Escape) => {
            // Close search
            state.ui.search.active = false;
            state.ui.search.query.clear();
            state.ui.search.matches.clear();
            state.ui.search.current_match = 0;
            clear_search_cache();
            state.force_active_tab_redraw();
            state.window.request_redraw();
            Some(KeyboardAction::Handled)
        }
        Key::Named(NamedKey::Enter) => {
            // Enter = next match, Shift+Enter = previous match
            Some(apply_keyboard_action(
                state,
                KeyboardAction::SearchNavigate {
                    reverse: shift_pressed,
                },
            ))
        }
        Key::Named(NamedKey::Backspace) => {
            // Delete last char from query
            state.ui.search.query.pop();
            super::update_search_matches(state);
            state.force_active_tab_redraw();
            state.window.request_redraw();
            Some(KeyboardAction::Handled)
        }
        Key::Character(c) if !chord_pressed => {
            // Add character to query
            state.ui.search.query.push_str(c.as_str());
            super::update_search_matches(state);
            state.force_active_tab_redraw();
            state.window.request_redraw();
            Some(KeyboardAction::Handled)
        }
        _ => None,
    }
}

/// Resolve and dispatch a user-configurable keybinding.
///
/// Returns `Some` if a binding matched the key event (applying any local side
/// effects), or `None` if no binding matched.
fn handle_configured_keybinding(
    state: &mut WindowState,
    key: &Key,
    unmodified_key: Option<&Key>,
    modifiers: &Modifiers,
    keybindings: &KeybindingsConfig,
) -> Option<KeyboardAction> {
    let key_action =
        resolve_keybinding_with_unmodified(keybindings, key, unmodified_key, modifiers)?;

    // Confirm any tab editing in progress before acting on the shortcut.
    if state.gpu.tab_bar.is_editing() {
        state.gpu.tab_bar.confirm_editing();
        state.render.dirty = true;
    }

    let ctx = InputContext::from_state(state);
    let action = key_action_to_keyboard_action(&key_action, &ctx);
    Some(apply_keyboard_action(state, action))
}

/// Handle hardcoded, non-configurable command shortcuts (Cmd/Ctrl + key).
fn handle_command_shortcuts(
    state: &mut WindowState,
    key: &Key,
    shift_pressed: bool,
) -> Option<KeyboardAction> {
    // Confirm any tab editing in progress
    if state.gpu.tab_bar.is_editing() {
        state.gpu.tab_bar.confirm_editing();
        state.render.dirty = true;
    }

    let ctx = InputContext::from_state(state);
    let action = determine_command_shortcut(key, shift_pressed, &ctx)?;
    Some(apply_keyboard_action(state, action))
}

/// Apply any local (window-scoped) side effects for an action and return the
/// resulting [`KeyboardAction`]. Actions that require app-level access (quit,
/// new window/tab, font size, fullscreen) are returned unchanged for the
/// caller in `handler.rs` to process.
fn apply_keyboard_action(state: &mut WindowState, action: KeyboardAction) -> KeyboardAction {
    match action {
        KeyboardAction::Copy => {
            if let Some(text) = get_terminal_selection_text(state) {
                set_clipboard_content(&text);
                state.ui.copy_indicator.trigger();
            }
            KeyboardAction::Handled
        }
        KeyboardAction::Paste => {
            if let Some(content) = get_clipboard_content() {
                paste_to_terminal(state, &content);
            }
            KeyboardAction::Handled
        }
        KeyboardAction::CloseTab(tab_id) => {
            state.gpu.tab_bar.close_tab(tab_id);
            state.remove_shell_for_tab(tab_id);
            state.force_active_tab_redraw();
            state.window.request_redraw();
            KeyboardAction::Handled
        }
        KeyboardAction::ToggleSearch => {
            state.ui.search.active = !state.ui.search.active;
            if !state.ui.search.active {
                state.ui.search.query.clear();
                state.ui.search.matches.clear();
                state.ui.search.current_match = 0;
                clear_search_cache();
            }
            state.force_active_tab_redraw();
            state.window.request_redraw();
            KeyboardAction::Handled
        }
        KeyboardAction::SearchNavigate { reverse } => {
            if !state.ui.search.matches.is_empty() {
                if reverse {
                    if state.ui.search.current_match == 0 {
                        state.ui.search.current_match = state.ui.search.matches.len() - 1;
                    } else {
                        state.ui.search.current_match -= 1;
                    }
                } else {
                    state.ui.search.current_match =
                        (state.ui.search.current_match + 1) % state.ui.search.matches.len();
                }
                super::scroll_to_current_match(state);
                state.force_active_tab_redraw();
                state.window.request_redraw();
            }
            KeyboardAction::Handled
        }
        KeyboardAction::PrevTab => {
            state.gpu.tab_bar.prev_tab();
            state.force_active_tab_redraw();
            state.window.request_redraw();
            KeyboardAction::Handled
        }
        KeyboardAction::NextTab => {
            state.gpu.tab_bar.next_tab();
            state.force_active_tab_redraw();
            state.window.request_redraw();
            KeyboardAction::Handled
        }
        KeyboardAction::SelectTab(index) => {
            state.gpu.tab_bar.select_tab_index(index);
            state.force_active_tab_redraw();
            state.window.request_redraw();
            KeyboardAction::Handled
        }
        // Actions that don't need local side effects (handled by caller)
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_ctx() -> InputContext {
        InputContext {
            context_menu_visible: false,
            tab_editing_active: false,
            window_rename_active: false,
            search_active: false,
            search_match_count: 0,
            tab_count: 1,
            active_tab_id: Some(1),
        }
    }

    /// Build a `Modifiers` with only the platform app chord held
    /// (Cmd on macOS, Ctrl+Shift elsewhere).
    fn primary_mods() -> Modifiers {
        Modifiers::from(primary_binding_mods())
    }

    /// Build a `Modifiers` with the app chord plus shift.
    fn primary_shift_mods() -> Modifiers {
        Modifiers::from(primary_binding_mods() | ModifiersState::SHIFT)
    }

    #[test]
    fn test_default_binding_quit() {
        let kb = KeybindingsConfig::default();
        let key = Key::Character("q".into());
        assert_eq!(
            resolve_keybinding(&kb, &key, &primary_mods()),
            Some(KeyAction::Quit)
        );
    }

    #[test]
    fn test_default_binding_new_tab() {
        let kb = KeybindingsConfig::default();
        let key = Key::Character("t".into());
        assert_eq!(
            resolve_keybinding(&kb, &key, &primary_mods()),
            Some(KeyAction::NewTab)
        );
    }

    #[test]
    fn test_default_binding_copy() {
        let kb = KeybindingsConfig::default();
        let key = Key::Character("c".into());
        assert_eq!(
            resolve_keybinding(&kb, &key, &primary_mods()),
            Some(KeyAction::Copy)
        );
    }

    #[test]
    fn test_default_binding_select_tab1() {
        let kb = KeybindingsConfig::default();
        let key = Key::Character("1".into());
        assert_eq!(
            resolve_keybinding(&kb, &key, &primary_mods()),
            Some(KeyAction::SelectTab1)
        );
    }

    #[test]
    fn test_default_binding_prev_tab_needs_shift() {
        let kb = KeybindingsConfig::default();
        let key = Key::Character("[".into());
        // Shift is required for prev/next tab.
        assert_eq!(
            resolve_keybinding(&kb, &key, &primary_shift_mods()),
            Some(KeyAction::PrevTab)
        );
        // Without shift, no binding matches. (On Linux the app chord is
        // Ctrl+Shift, so "without shift" means plain Ctrl.)
        let no_shift = Modifiers::from(primary_binding_mods() & !ModifiersState::SHIFT);
        assert_eq!(resolve_keybinding(&kb, &key, &no_shift), None);
    }

    #[test]
    fn test_no_modifier_does_not_match_primary_binding() {
        let kb = KeybindingsConfig::default();
        let key = Key::Character("t".into());
        assert_eq!(resolve_keybinding(&kb, &key, &Modifiers::default()), None);
    }

    #[test]
    fn test_equal_token_normalization() {
        // A binding on "equal" matches the "=" character event.
        let kb = KeybindingsConfig::default();
        let key = Key::Character("=".into());
        assert_eq!(
            resolve_keybinding(&kb, &key, &primary_mods()),
            Some(KeyAction::IncreaseFontSize)
        );
    }

    #[test]
    fn test_custom_binding_resolves() {
        use crate::config::Keybinding;
        let kb = KeybindingsConfig {
            bindings: vec![Keybinding {
                key: "F11".to_string(),
                mods: vec![],
                action: KeyAction::ToggleFullscreen,
            }],
        };
        let key = Key::Named(NamedKey::F11);
        assert_eq!(
            resolve_keybinding(&kb, &key, &Modifiers::default()),
            Some(KeyAction::ToggleFullscreen)
        );
    }

    #[test]
    fn test_key_action_close_tab_single_tab_closes_window() {
        let ctx = InputContext {
            tab_count: 1,
            active_tab_id: Some(1),
            ..default_ctx()
        };
        let action = key_action_to_keyboard_action(&KeyAction::CloseTab, &ctx);
        assert!(matches!(action, KeyboardAction::CloseWindow));
    }

    #[test]
    fn test_key_action_close_tab_multiple_tabs_closes_tab() {
        let ctx = InputContext {
            tab_count: 3,
            active_tab_id: Some(42),
            ..default_ctx()
        };
        let action = key_action_to_keyboard_action(&KeyAction::CloseTab, &ctx);
        assert!(matches!(action, KeyboardAction::CloseTab(42)));
    }

    #[test]
    fn test_hardcoded_cmd_n_returns_new_window() {
        let ctx = default_ctx();
        let key = Key::Character("n".into());
        let result = determine_command_shortcut(&key, false, &ctx);
        assert!(matches!(result, Some(KeyboardAction::NewWindow)));
    }

    #[test]
    fn test_hardcoded_cmd_f_returns_toggle_search() {
        let ctx = default_ctx();
        let key = Key::Character("f".into());
        let result = determine_command_shortcut(&key, false, &ctx);
        assert!(matches!(result, Some(KeyboardAction::ToggleSearch)));
    }

    #[test]
    fn test_scroll_shift_pageup() {
        let key = Key::Named(NamedKey::PageUp);
        let result = determine_scroll_action(&key, false, true);
        assert!(matches!(result, Some(Scroll::PageUp)));
    }

    #[test]
    fn test_scroll_no_shift_returns_none() {
        let key = Key::Named(NamedKey::PageUp);
        let result = determine_scroll_action(&key, false, false);
        assert!(result.is_none());
    }

    // ── B1: Ctrl chords reach the shell on non-macOS ───────────────

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_plain_ctrl_is_not_the_app_modifier() {
        assert!(!app_modifier_held(ModifiersState::CONTROL));
        assert!(app_modifier_held(
            ModifiersState::CONTROL | ModifiersState::SHIFT
        ));
        assert!(!app_modifier_held(ModifiersState::SUPER));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_ctrl_c_without_shift_is_unbound_and_encodes_etx() {
        use super::super::{ShellKeyInput, shell_bytes_for_key};
        use crate::input::EncodeModes;

        let kb = KeybindingsConfig::default();
        let key = Key::Character("c".into());
        let ctrl = Modifiers::from(ModifiersState::CONTROL);
        // No default binding claims plain Ctrl+C ...
        assert_eq!(resolve_keybinding(&kb, &key, &ctrl), None);
        // ... so it takes the shell path and encodes as ETX.
        let input = ShellKeyInput {
            key: &key,
            unmodified_key: Some(&key),
            text: Some("c"),
            modifiers: ModifiersState::CONTROL,
        };
        assert_eq!(
            shell_bytes_for_key(&input, EncodeModes::default()),
            Some(vec![0x03])
        );
        // Ctrl+Shift+C is the Copy chord and never reaches the shell path.
        let ctrl_shift = Modifiers::from(ModifiersState::CONTROL | ModifiersState::SHIFT);
        assert_eq!(
            resolve_keybinding(&kb, &Key::Character("C".into()), &ctrl_shift),
            Some(KeyAction::Copy)
        );
    }

    #[test]
    fn super_chord_never_reaches_shell() {
        use super::super::{ShellKeyInput, shell_bytes_for_key};
        use crate::input::EncodeModes;
        let key = Key::Character("x".into());
        let input = ShellKeyInput {
            key: &key,
            unmodified_key: Some(&key),
            text: Some("x"),
            modifiers: ModifiersState::SUPER,
        };
        assert_eq!(shell_bytes_for_key(&input, EncodeModes::default()), None);
    }

    #[test]
    fn unmodified_key_matches_binding_when_logical_key_is_shifted() {
        // Ctrl+Shift+1 on a US layout delivers logical "!" but unmodified "1".
        let kb = KeybindingsConfig::default();
        let logical = Key::Character("!".into());
        let unmodified = Key::Character("1".into());
        assert_eq!(
            resolve_keybinding_with_unmodified(&kb, &logical, Some(&unmodified), &primary_mods()),
            Some(KeyAction::SelectTab1)
        );
    }

    #[test]
    fn explicit_ctrl_binding_matches_plain_ctrl_only() {
        use crate::config::Keybinding;
        let kb = KeybindingsConfig {
            bindings: vec![Keybinding {
                key: "y".to_string(),
                mods: vec!["ctrl".to_string()],
                action: KeyAction::ToggleFullscreen,
            }],
        };
        let key = Key::Character("y".into());
        let ctrl = Modifiers::from(ModifiersState::CONTROL);
        assert_eq!(
            resolve_keybinding(&kb, &key, &ctrl),
            Some(KeyAction::ToggleFullscreen)
        );
        let ctrl_shift = Modifiers::from(ModifiersState::CONTROL | ModifiersState::SHIFT);
        assert_eq!(resolve_keybinding(&kb, &key, &ctrl_shift), None);
    }

    #[test]
    fn binding_cache_tracks_config_changes() {
        use crate::config::Keybinding;
        let mut kb = KeybindingsConfig::default();
        let key = Key::Character("t".into());
        assert_eq!(
            resolve_keybinding(&kb, &key, &primary_mods()),
            Some(KeyAction::NewTab)
        );
        // Mutate the same config in place: the cache must notice.
        kb.bindings.retain(|b| b.action != KeyAction::NewTab);
        assert_eq!(resolve_keybinding(&kb, &key, &primary_mods()), None);
        kb.bindings.push(Keybinding {
            key: "t".to_string(),
            mods: vec!["super".to_string()],
            action: KeyAction::NewTab,
        });
        assert_eq!(
            resolve_keybinding(&kb, &key, &primary_mods()),
            Some(KeyAction::NewTab)
        );
    }

    #[test]
    fn normalize_key_token_borrows_when_unchanged() {
        assert!(matches!(normalize_key_token("t"), Cow::Borrowed("t")));
        assert_eq!(normalize_key_token("T"), "t");
        assert_eq!(normalize_key_token("+"), "equal");
        assert_eq!(normalize_key_token("{"), "[");
    }

    #[test]
    fn command_shortcut_is_case_insensitive() {
        let ctx = default_ctx();
        let key = Key::Character("N".into());
        assert!(matches!(
            determine_command_shortcut(&key, false, &ctx),
            Some(KeyboardAction::NewWindow)
        ));
    }
}
