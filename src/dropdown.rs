//! Themed dropdowns. The webview draws the popups of `<select>` and `<datalist>` with the
//! system theme, which doesn't match the app, so Workbench draws its own:
//!
//! - `Dropdown` picks one of a list of choices.
//! - `ComboBox` is a text field with a list of suggestions (any text is allowed).
//!
//! Their styles are in theme.css. The open list is fixed to the window and placed under the
//! control, or above it when there is more room there.

use std::rc::Rc;

use dioxus::prelude::*;

use crate::workbench::window_size;

const MAX_LIST_H: f64 = 320.0;
const GAP: f64 = 4.0;

/// One option of a `Dropdown`: the value it reports and the text it shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

impl Choice {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self { value: value.into(), label: label.into() }
    }
}

/// Where the open list goes, in window coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Place {
    left: f64,
    min_width: f64,
    /// Distance from the window top when the list opens downwards…
    top: Option<f64>,
    /// …or from the window bottom when it opens upwards.
    bottom: Option<f64>,
    max_height: f64,
}

impl Place {
    fn style(&self) -> String {
        let vertical = match (self.top, self.bottom) {
            (Some(top), _) => format!("top: {top}px;"),
            (None, Some(bottom)) => format!("bottom: {bottom}px;"),
            (None, None) => String::new(),
        };
        format!("left: {}px; min-width: {}px; max-height: {}px; {vertical}", self.left, self.min_width, self.max_height)
    }
}

/// Places the list under the control, or above it when there's more room above.
async fn place_list(control: Rc<MountedData>) -> Option<Place> {
    let rect = control.get_client_rect().await.ok()?;
    let (_, window_h) = window_size();
    let control_top = rect.origin.y;
    let control_bottom = rect.origin.y + rect.size.height;
    let room_below = window_h - control_bottom - 2.0 * GAP;
    let room_above = control_top - 2.0 * GAP;
    let opens_down = room_below >= 200.0 || room_below >= room_above;
    Some(Place {
        left: rect.origin.x,
        min_width: rect.size.width,
        top: opens_down.then_some(control_bottom + GAP),
        bottom: (!opens_down).then_some(window_h - control_top + GAP),
        max_height: if opens_down { room_below } else { room_above }.min(MAX_LIST_H),
    })
}

/// Picks one of `choices`. Shows the label of the choice whose value is `value`, or
/// `placeholder` when none matches; action menus ("+ Module…") pass an empty `value` and
/// a placeholder. Keyboard: ↑/↓ to move, Enter to pick, Esc to close.
#[component]
pub fn Dropdown(
    value: String,
    choices: Vec<Choice>,
    onchange: EventHandler<String>,
    #[props(default)] placeholder: String,
    #[props(default)] disabled: bool,
    #[props(default)] title: String,
    #[props(default)] class: String,
) -> Element {
    let mut open = use_signal(|| None::<Place>);
    let mut cursor = use_signal(|| 0usize);
    let mut control = use_signal(|| None::<Rc<MountedData>>);
    let selected = choices.iter().position(|c| c.value == value);
    let label = selected.map(|i| choices[i].label.clone()).unwrap_or_else(|| placeholder.clone());
    let n = choices.len();
    let is_open = open.read().is_some();

    let mut toggle = move || {
        if open.peek().is_some() {
            open.set(None);
            return;
        }
        let Some(el) = control.peek().clone() else { return };
        cursor.set(selected.unwrap_or(0));
        spawn(async move { open.set(place_list(el).await) });
    };
    let pick = {
        let choices = choices.clone();
        move |i: usize| {
            open.set(None);
            if let Some(c) = choices.get(i) {
                onchange.call(c.value.clone());
            }
        }
    };
    let mut key_pick = pick.clone();

    rsx! {
        div { class: "dropdown {class}",
            button {
                class: if is_open { "dropdown-trigger open" } else { "dropdown-trigger" },
                disabled,
                title: "{title}",
                onmounted: move |e| control.set(Some(e.data())),
                onclick: move |_| toggle(),
                onkeydown: move |e: KeyboardEvent| {
                    let c = *cursor.peek();
                    let handled = match e.key() {
                        Key::ArrowDown if open.peek().is_none() => {
                            toggle();
                            true
                        }
                        Key::ArrowDown if n > 0 => {
                            cursor.set((c + 1) % n);
                            true
                        }
                        Key::ArrowUp if n > 0 && open.peek().is_some() => {
                            cursor.set((c + n - 1) % n);
                            true
                        }
                        Key::Enter if open.peek().is_some() => {
                            key_pick(c);
                            true
                        }
                        Key::Escape if open.peek().is_some() => {
                            open.set(None);
                            true
                        }
                        _ => false,
                    };
                    if handled {
                        e.prevent_default();
                        e.stop_propagation();
                    }
                },
                span { class: "dropdown-label", "{label}" }
                span { class: "dropdown-caret", "▾" }
            }
            if let Some(place) = *open.read() {
                div { class: "dropdown-backdrop", onmousedown: move |_| open.set(None) }
                ul { class: "dropdown-list", style: "{place.style()}",
                    if choices.is_empty() {
                        li { class: "empty", "Nothing to pick" }
                    }
                    for (i, c) in choices.iter().enumerate() {
                        li {
                            key: "{i}",
                            class: match (i == *cursor.read(), Some(i) == selected) {
                                (true, true) => "active selected",
                                (true, false) => "active",
                                (false, true) => "selected",
                                (false, false) => "",
                            },
                            title: "{c.label}",
                            onmouseenter: move |_| cursor.set(i),
                            onclick: {
                                let mut pick = pick.clone();
                                move |_| pick(i)
                            },
                            "{c.label}"
                            if Some(i) == selected {
                                span { class: "dropdown-check", "✓" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A text field with a themed list of suggestions, for values that may also be typed freely
/// (such as model names). The list shows every suggestion until the user types, then the ones
/// containing the typed text. `onchange` gets the trimmed text on Enter, on leaving the field
/// with a changed value, or on picking a suggestion; Esc puts the old value back. When `value`
/// changes from elsewhere, the text follows it.
#[component]
pub fn ComboBox(
    value: String,
    suggestions: Vec<String>,
    onchange: EventHandler<String>,
    #[props(default)] placeholder: String,
    #[props(default)] disabled: bool,
    #[props(default)] title: String,
    #[props(default)] class: String,
) -> Element {
    let initial = value.clone();
    let mut draft = use_signal(move || initial);
    let mut typed = use_signal(|| false);
    let mut open = use_signal(|| None::<Place>);
    let mut cursor = use_signal(|| None::<usize>);
    let mut field = use_signal(|| None::<Rc<MountedData>>);
    use_effect(use_reactive!(|value| draft.set(value)));

    let needle = draft.read().trim().to_lowercase();
    let shown: Vec<String> = if *typed.read() && !needle.is_empty() {
        suggestions.iter().filter(|s| s.to_lowercase().contains(&needle)).cloned().collect()
    } else {
        suggestions.clone()
    };
    let n = shown.len();

    let show_list = move || {
        if open.peek().is_some() {
            return;
        }
        let Some(el) = field.peek().clone() else { return };
        spawn(async move { open.set(place_list(el).await) });
    };
    let mut close = move || {
        open.set(None);
        cursor.set(None);
        typed.set(false);
    };
    let commit = {
        let value = value.clone();
        move |text: String| {
            let text = text.trim().to_string();
            draft.set(text.clone());
            if text != value.trim() {
                onchange.call(text);
            }
        }
    };
    let mut blur_commit = commit.clone();
    let mut key_commit = commit.clone();
    let keys_shown = shown.clone();
    let old_value = value.clone();

    rsx! {
        div { class: "combobox {class}",
            input {
                title: "{title}",
                placeholder: "{placeholder}",
                disabled,
                value: "{draft}",
                onmounted: move |e| field.set(Some(e.data())),
                onfocus: move |_| show_list(),
                onclick: move |_| show_list(),
                oninput: move |e| {
                    draft.set(e.value());
                    typed.set(true);
                    cursor.set(None);
                    show_list();
                },
                onblur: move |_| {
                    close();
                    blur_commit(draft.peek().clone());
                },
                onkeydown: move |e: KeyboardEvent| {
                    let c = *cursor.peek();
                    let handled = match e.key() {
                        Key::ArrowDown if n > 0 => {
                            show_list();
                            cursor.set(Some(c.map(|i| (i + 1) % n).unwrap_or(0)));
                            true
                        }
                        Key::ArrowUp if n > 0 => {
                            show_list();
                            cursor.set(Some(c.map(|i| (i + n - 1) % n).unwrap_or(n - 1)));
                            true
                        }
                        Key::Enter => {
                            let text = c.and_then(|i| keys_shown.get(i).cloned()).unwrap_or_else(|| draft.peek().clone());
                            close();
                            key_commit(text);
                            true
                        }
                        Key::Escape => {
                            close();
                            draft.set(old_value.clone());
                            true
                        }
                        _ => false,
                    };
                    if handled {
                        e.prevent_default();
                        e.stop_propagation();
                    }
                },
            }
            if let Some(place) = open.read().filter(|_| n > 0) {
                ul { class: "dropdown-list", style: "{place.style()}",
                    for (i, s) in shown.into_iter().enumerate() {
                        li {
                            key: "{s}",
                            class: if Some(i) == *cursor.read() { "active" } else { "" },
                            title: "{s}",
                            onmouseenter: move |_| cursor.set(Some(i)),
                            // Keep the focus in the field, so picking doesn't count as leaving it.
                            onmousedown: move |e| e.prevent_default(),
                            onclick: {
                                let mut commit = commit.clone();
                                let text = s.clone();
                                move |_| {
                                    close();
                                    commit(text.clone());
                                }
                            },
                            "{s}"
                        }
                    }
                }
            }
        }
    }
}
