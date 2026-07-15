//! Settings window — a separate top-level OS window hosting the settings panel.
//!
//! --- Unlike the main window (which swaps clipboard / edit views) and the ---
//! --- floating quick-paste popup, this is a normal activated, resizable ---
//! --- window dedicated to settings. It owns its OWN SettingsPanel — whose ---
//! --- InputState / focus handle are bound to THIS window — and routes ---
//! --- cross-window effects (theme change, list refresh) back to the main ---
//! --- window through WindowManagerEvent. ---

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::*;

use crate::state::app::AppState;
use crate::ui::components::confirm_dialog::ConfirmDialog;
use crate::ui::components::toast::{Toast, ToastKind};
use crate::ui::settings::hotkey;
use crate::ui::settings::{SettingsEvent, SettingsPanel};
use crate::ui::theme::ClippiTheme;
use crate::ui::window_manager::{WindowManager, WindowManagerEvent};

/// Floating panel / dialog enter animation duration.
const OVERLAY_ANIM_DURATION: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Default)]
struct OverlayTransitionState {
    visible: bool,
    generation: u64,
    started_at: Option<Instant>,
}

pub struct SettingsWindowView {
    state: Entity<AppState>,
    window_manager: Entity<WindowManager>,
    settings_panel: Entity<SettingsPanel>,
    theme: ClippiTheme,
    /// Cached at creation time so the ThemeChanged handler (which only has
    /// access to `&mut App`) can resolve the "system" theme correctly.
    window_appearance: WindowAppearance,
    overlay_transitions: HashMap<&'static str, OverlayTransitionState>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
    _appearance_subscription: Option<Subscription>,
}

impl SettingsWindowView {
    pub fn new(
        window: &mut Window,
        state: Entity<AppState>,
        window_manager: Entity<WindowManager>,
        cx: &mut Context<Self>,
    ) -> Self {
        let window_appearance = window.appearance();
        let theme =
            ClippiTheme::from_setting(&state.read(cx).settings.theme, Some(window_appearance));

        // The settings panel MUST be created with this window so its InputState
        // and focus subscriptions bind here (not to the main window).
        let settings_panel = cx.new(|cx| {
            SettingsPanel::new(
                state.clone(),
                window_manager.clone(),
                theme.clone(),
                window,
                cx,
            )
        });

        let backend_panel = settings_panel.read(cx).backend_panel();

        // --- Forward WindowManager events into the settings panel we own. ---
        let wm_sub = cx.subscribe(
            &window_manager,
            move |this, _wm, event: &WindowManagerEvent, cx| match event {
                WindowManagerEvent::HotkeyRecordingComplete => {
                    // --- Notify SettingsPanel so it re-renders with the updated ---
                    // --- hotkey display and recording state from AppState. ---
                    this.settings_panel.update(cx, |_panel, cx| {
                        cx.notify();
                    });
                }
                WindowManagerEvent::SyncChanged => {
                    this.settings_panel.update(cx, |_panel, cx| {
                        cx.notify();
                    });
                    cx.notify();
                }
                WindowManagerEvent::PasteShortcutRecorded { app_name, shortcut } => {
                    this.settings_panel.update(cx, |panel, cx| {
                        panel.recording_paste_shortcut = None;
                        cx.emit(SettingsEvent::ShowHotkeyConfirm(
                            hotkey::HotkeyConfirmAction::AddPasteShortcut {
                                app_name: app_name.clone(),
                                shortcut: shortcut.clone(),
                            },
                        ));
                    });
                    cx.notify();
                }
                WindowManagerEvent::UpdateAvailable | WindowManagerEvent::UpdateProgress(_) => {
                    // The version tab reads update state from AppState (already
                    // set by poll_update); notify so it re-renders live.
                    if this.settings_panel.read(cx).active_tab() == 5 {
                        this.settings_panel.update(cx, |_panel, cx| {
                            cx.notify();
                        });
                    }
                }
                _ => {}
            },
        );

        // --- Own the SettingsEvent handling that used to live in RootView. ---
        let settings_sub = cx.subscribe(
            &settings_panel,
            move |this, _panel, event: &SettingsEvent, cx| match event {
                SettingsEvent::Back => {
                    this.window_manager
                        .update(cx, |wm, cx| wm.hide_settings_window(cx));
                }
                SettingsEvent::TabChanged(idx) => {
                    // When the user clicks the version tab manually, clear any
                    // existing update toast — the info is now on screen.
                    if *idx == 5 && this.state.read(cx).toast_message.is_some() {
                        this.state.update(cx, |s, _cx| s.clear_toast());
                        cx.notify();
                    }
                }
                SettingsEvent::ThemeChanged(theme_str) => {
                    // --- Use cached window_appearance from creation time so ---
                    // --- "system" theme resolves correctly even though we ---
                    // --- only have &mut App here (not WindowContext). ---
                    this.theme =
                        ClippiTheme::from_setting(theme_str, Some(this.window_appearance));
                    let theme = this.theme.clone();

                    // --- Sync gpui_component theme so that Input, Scrollbar ---
                    // --- and other gpui_component widgets follow our theme. ---
                    let is_dark = theme.bg == rgb(0x191a1b);
                    gpui_component::Theme::change(
                        if is_dark {
                            gpui_component::ThemeMode::Dark
                        } else {
                            gpui_component::ThemeMode::Light
                        },
                        None,
                        cx,
                    );
                    // --- Must restore transparent background after Theme::change ---
                    // --- resets it — otherwise the main window loses transparency. ---
                    gpui_component::Theme::global_mut(cx).background = Hsla::transparent_black();

                    this.settings_panel.update(cx, |panel, cx| {
                        panel.reload_theme(theme.clone(), cx);
                    });

                    // --- The main window's child views hold their own theme copy; ---
                    // --- ask RootView to re-apply via a WindowManager event. ---
                    this.window_manager.update(cx, |_wm, cx| {
                        cx.emit(WindowManagerEvent::SettingsThemeChanged(theme_str.clone()));
                    });
                    cx.notify();
                }
                SettingsEvent::ClipboardSettingsChanged {
                    reload_items,
                    scroll_to_top,
                } => {
                    // The list lives in the main window — route the refresh there.
                    let reload_items = *reload_items;
                    let scroll_to_top = *scroll_to_top;
                    this.window_manager.update(cx, |_wm, cx| {
                        cx.emit(WindowManagerEvent::SettingsClipboardChanged {
                            reload_items,
                            scroll_to_top,
                        });
                    });
                }
                SettingsEvent::ShowHotkeyConfirm(action) => {
                    this.settings_panel.update(cx, |panel, _cx| {
                        panel.hotkey_confirm = Some(action.clone());
                    });
                    cx.notify();
                }
                SettingsEvent::HotkeyPasteShortcut { ref action } => {
                    match action {
                        hotkey::HotkeyConfirmAction::AddPasteShortcut { app_name, shortcut } => {
                            let mut list = this.state.read(cx).settings.paste_shortcuts.clone();
                            // Remove existing entry for same app (overwrite)
                            list.retain(|e| e.app_name != *app_name);
                            list.push(crate::core::settings::PasteShortcutEntry {
                                app_name: app_name.clone(),
                                shortcut: shortcut.clone(),
                            });
                            this.state.update(cx, |s, _cx| {
                                s.settings.paste_shortcuts = list;
                                s.settings.save();
                            });
                            this.settings_panel.update(cx, |panel, cx| {
                                panel.clear_paste_shortcut_state(cx);
                                panel.clear_hotkey_confirm(cx);
                            });
                        }
                        hotkey::HotkeyConfirmAction::RemovePasteShortcut { app_name } => {
                            let mut list = this.state.read(cx).settings.paste_shortcuts.clone();
                            list.retain(|e| e.app_name != *app_name);
                            this.state.update(cx, |s, _cx| {
                                s.settings.paste_shortcuts = list;
                                s.settings.save();
                            });
                            this.settings_panel.update(cx, |panel, cx| {
                                panel.clear_paste_shortcut_state(cx);
                                panel.clear_hotkey_confirm(cx);
                            });
                        }
                        _ => {}
                    }
                    cx.notify();
                }
                SettingsEvent::DataError(msg) => {
                    this.state.update(cx, |s, _cx| {
                        s.toast_message = Some(format!(
                            "{}: {msg}",
                            crate::core::i18n_keys::I18nKey::ErrDataOp.text()
                        ));
                    });
                    cx.notify();
                }
                SettingsEvent::DataToast(msg) => {
                    this.state.update(cx, |s, _cx| {
                        s.toast_message = Some(msg.clone());
                    });
                    cx.notify();
                }
            },
        );

        let backend_observe = cx.observe(&backend_panel, |_this, _, cx| {
            cx.notify();
        });

        // --- Track OS appearance changes so the "system" theme updates live. ---
        let appearance_sub = cx.observe_window_appearance(window, move |this, win, cx| {
            this.window_appearance = win.appearance();
            if this.state.read(cx).settings.theme != "system" {
                return;
            }
            this.theme = ClippiTheme::from_setting("system", Some(this.window_appearance));
            let theme = this.theme.clone();

            let is_dark = theme.bg == rgb(0x191a1b);
            gpui_component::Theme::change(
                if is_dark {
                    gpui_component::ThemeMode::Dark
                } else {
                    gpui_component::ThemeMode::Light
                },
                None,
                cx,
            );
            gpui_component::Theme::global_mut(cx).background = Hsla::transparent_black();

            this.settings_panel
                .update(cx, |sp, cx| sp.reload_theme(theme.clone(), cx));

            // Propagate to the main window's child views.
            this.window_manager.update(cx, |_wm, cx| {
                cx.emit(WindowManagerEvent::SettingsThemeChanged("system".to_string()));
            });
            cx.notify();
        });

        Self {
            state,
            window_manager,
            settings_panel,
            theme,
            window_appearance,
            overlay_transitions: HashMap::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![wm_sub, settings_sub, backend_observe],
            _appearance_subscription: Some(appearance_sub),
        }
    }

    /// Switch the settings panel to a specific tab (used when opening on the
    /// version tab from the tray).
    pub fn set_active_tab(&mut self, tab: usize, cx: &mut Context<Self>) {
        self.settings_panel.update(cx, |panel, _cx| {
            panel.set_active_tab(tab);
        });
        cx.notify();
    }

    /// The settings panel's currently active tab.
    pub fn active_tab(&self, cx: &App) -> usize {
        self.settings_panel.read(cx).active_tab()
    }

    fn overlay_generation(&mut self, key: &'static str, visible: bool) -> u64 {
        let state = self.overlay_transitions.entry(key).or_default();
        if visible && !state.visible {
            state.generation = state.generation.wrapping_add(1);
            state.started_at = Some(Instant::now());
        } else if !visible {
            state.started_at = None;
        }
        state.visible = visible;
        state.generation
    }

    fn overlay_animating(&self, key: &'static str) -> bool {
        self.overlay_transitions
            .get(key)
            .and_then(|state| state.started_at)
            .is_some_and(|started_at| {
                started_at.elapsed() <= OVERLAY_ANIM_DURATION + Duration::from_millis(24)
            })
    }

    fn overlay_opacity(
        window: &mut Window,
        cx: &mut Context<Self>,
        generation: u64,
        key: &'static str,
    ) -> f32 {
        use gpui_transitions::WindowUseTransition;
        let transition = window
            .use_keyed_transition(
                (key, generation.wrapping_add(10_000)),
                cx,
                OVERLAY_ANIM_DURATION,
                move |_, _| 0.0_f32,
            )
            .with_easing(Self::ease_out);
        transition.update(cx, |value, cx| {
            *value = 1.0;
            cx.notify();
        });
        let value = *transition.evaluate(window, cx);
        value
    }

    fn ease_out(delta: f32) -> f32 {
        1.0 - (1.0 - delta).powi(3)
    }
}

impl Render for SettingsWindowView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings_panel = self.settings_panel.clone();
        let backend_panel = self.settings_panel.read(cx).backend_panel();
        let backend_panel_open = backend_panel.read(cx).is_visible();
        let hotkey_confirm_open = self.settings_panel.read(cx).hotkey_confirm.is_some();
        let theme = self.theme.clone();

        let hotkey_confirm_visible = hotkey_confirm_open;
        let backend_panel_visible = backend_panel_open;

        let hotkey_confirm_gen = self.overlay_generation("hotkey-confirm", hotkey_confirm_visible);
        let backend_panel_gen = self.overlay_generation("backend-panel", backend_panel_visible);
        let backend_panel_animating = self.overlay_animating("backend-panel");
        let backend_panel_opacity = if backend_panel_visible && backend_panel_animating {
            Self::overlay_opacity(window, cx, backend_panel_gen, "backend-panel")
        } else {
            1.0
        };

        let root_focus = self.focus_handle.clone();
        let root_backend = backend_panel.clone();
        let root_wm = self.window_manager.clone();

        div()
            .relative()
            .size_full()
            .bg(theme.bg)
            .track_focus(&root_focus)
            .on_key_down(move |ev: &KeyDownEvent, _window, cx| {
                if ev.keystroke.key.as_str() != "escape" {
                    return;
                }
                // Close the backend panel popup first; otherwise hide the window.
                if root_backend.read(cx).is_visible() {
                    root_backend.update(cx, |panel, cx| panel.close(cx));
                } else {
                    root_wm.update(cx, |wm, cx| wm.hide_settings_window(cx));
                }
                cx.stop_propagation();
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .size_full()
                    .overflow_hidden()
                    .child(settings_panel),
            )
            // --- Settings hotkey blacklist ConfirmDialog ---
            .when(hotkey_confirm_visible, |root| {
                let dialog_focus = cx.focus_handle();
                let settings = self.settings_panel.clone();
                let wm = self.window_manager.clone();
                let app_state = self.state.clone();

                let action = settings.read(cx).hotkey_confirm.clone();
                let dialog_element: AnyElement = match action {
                    Some(hotkey::HotkeyConfirmAction::AddBlacklist { app_name }) => {
                        ConfirmDialog::add_blacklist(&app_name)
                            .theme(self.theme.clone())
                            .on_confirm({
                                let wm = wm.clone();
                                let app_state = app_state.clone();
                                let settings = settings.clone();
                                let app_name = app_name.clone();
                                move |_window, cx| {
                                    let app_name = app_name.clone();
                                    app_state.update(cx, |s, _cx| {
                                        if !s.settings.hotkey_blacklist.contains(&app_name) {
                                            s.settings.hotkey_blacklist.push(app_name.clone());
                                            s.settings.save();
                                        }
                                    });
                                    // --- Sync WindowManager's blacklist from settings ---
                                    let updated =
                                        app_state.read(cx).settings.hotkey_blacklist.clone();
                                    wm.update(cx, |wm, _cx| {
                                        wm.set_blacklist(updated);
                                    });
                                    settings.update(cx, |panel, cx| {
                                        panel.clear_hotkey_confirm(cx);
                                    });
                                }
                            })
                            .on_cancel({
                                let settings = settings.clone();
                                move |_window, cx| {
                                    settings.update(cx, |panel, cx| {
                                        panel.clear_hotkey_confirm(cx);
                                    });
                                }
                            })
                            .focus_handle(dialog_focus.clone())
                            .render_animated(window, cx, hotkey_confirm_gen)
                    }
                    Some(hotkey::HotkeyConfirmAction::RemoveBlacklist { app_name }) => {
                        ConfirmDialog::remove_blacklist(&app_name)
                            .theme(self.theme.clone())
                            .on_confirm({
                                let wm = wm.clone();
                                let app_state = app_state.clone();
                                let settings = settings.clone();
                                let app_name = app_name.clone();
                                move |_window, cx| {
                                    app_state.update(cx, |s, _cx| {
                                        s.settings.hotkey_blacklist.retain(|a| a != &app_name);
                                        s.settings.save();
                                    });
                                    // --- Sync WindowManager's blacklist from settings ---
                                    let updated =
                                        app_state.read(cx).settings.hotkey_blacklist.clone();
                                    wm.update(cx, |wm, _cx| {
                                        wm.set_blacklist(updated);
                                    });
                                    settings.update(cx, |panel, cx| {
                                        panel.clear_hotkey_confirm(cx);
                                    });
                                }
                            })
                            .on_cancel({
                                let settings = settings.clone();
                                move |_window, cx| {
                                    settings.update(cx, |panel, cx| {
                                        panel.clear_hotkey_confirm(cx);
                                    });
                                }
                            })
                            .focus_handle(dialog_focus.clone())
                            .render_animated(window, cx, hotkey_confirm_gen)
                    }
                    Some(hotkey::HotkeyConfirmAction::AddPasteShortcut { app_name, shortcut }) => {
                        ConfirmDialog::add_paste_shortcut(&app_name, &shortcut)
                            .theme(self.theme.clone())
                            .on_confirm({
                                let settings = settings.clone();
                                let app_name = app_name.clone();
                                let shortcut = shortcut.clone();
                                move |_window, cx| {
                                    settings.update(cx, |_panel, cx| {
                                        cx.emit(SettingsEvent::HotkeyPasteShortcut {
                                            action: hotkey::HotkeyConfirmAction::AddPasteShortcut {
                                                app_name: app_name.clone(),
                                                shortcut: shortcut.clone(),
                                            },
                                        });
                                    });
                                }
                            })
                            .on_cancel({
                                let settings = settings.clone();
                                move |_window, cx| {
                                    settings.update(cx, |panel, cx| {
                                        panel.clear_paste_shortcut_state(cx);
                                        panel.clear_hotkey_confirm(cx);
                                    });
                                }
                            })
                            .focus_handle(dialog_focus.clone())
                            .render_animated(window, cx, hotkey_confirm_gen)
                    }
                    Some(hotkey::HotkeyConfirmAction::RemovePasteShortcut { app_name }) => {
                        ConfirmDialog::remove_paste_shortcut(&app_name)
                            .theme(self.theme.clone())
                            .on_confirm({
                                let settings = settings.clone();
                                let app_name = app_name.clone();
                                move |_window, cx| {
                                    settings.update(cx, |_panel, cx| {
                                        cx.emit(SettingsEvent::HotkeyPasteShortcut {
                                            action:
                                                hotkey::HotkeyConfirmAction::RemovePasteShortcut {
                                                    app_name: app_name.clone(),
                                                },
                                        });
                                    });
                                }
                            })
                            .on_cancel({
                                let settings = settings.clone();
                                move |_window, cx| {
                                    settings.update(cx, |panel, cx| {
                                        panel.clear_paste_shortcut_state(cx);
                                        panel.clear_hotkey_confirm(cx);
                                    });
                                }
                            })
                            .focus_handle(dialog_focus.clone())
                            .render_animated(window, cx, hotkey_confirm_gen)
                    }
                    None => div().into_any_element(),
                };

                root.child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .right(px(0.))
                        .top(px(0.))
                        .bottom(px(0.))
                        .child(dialog_element),
                )
            })
            .when(backend_panel_visible, |root| {
                root.child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .right(px(0.))
                        .top(px(0.))
                        .bottom(px(0.))
                        .opacity(backend_panel_opacity)
                        .child(backend_panel),
                )
            })
            // --- Toast for settings-originated feedback (data ops, recording ---
            // --- conflict / fallback). Shares AppState.toast_message. ---
            .when(self.state.read(cx).toast_message.is_some(), |root| {
                let state = self.state.clone();
                let message = self
                    .state
                    .read(cx)
                    .toast_message
                    .clone()
                    .unwrap_or_default();
                let theme = self.theme.clone();
                let kind = if self.state.read(cx).toast_is_warning {
                    ToastKind::Warn
                } else {
                    ToastKind::Info
                };
                root.child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .right(px(0.))
                        .top(px(0.))
                        .bottom(px(0.))
                        .child(
                            div()
                                .absolute()
                                .left(px(20.))
                                .right(px(20.))
                                .bottom(px(50.))
                                .cursor(CursorStyle::PointingHand)
                                .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                                    state.update(cx, |s, _cx| s.clear_toast());
                                })
                                .child(Toast::new(message).theme(theme).kind(kind)),
                        ),
                )
            })
    }
}
