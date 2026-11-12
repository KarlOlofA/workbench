//! The chat in a window of its own. `Layout::chat_detached` decides where the chat lives:
//! when set, a second OS window shows it (and the in-app chat steps aside); when cleared,
//! the window closes and the chat floats inside the app again. The setting is saved, so a
//! detached chat reopens in its own window on the next launch.
//!
//! The window runs its own VirtualDom but shares the app's `AppState` and session
//! coroutine, so both windows read and write the same chat. Dioxus re-renders each window
//! through its own runtime when a shared signal changes.

use dioxus::desktop::tao::event::Event;
use dioxus::desktop::tao::window::{ResizeDirection, WindowId};
use dioxus::desktop::{Config, LogicalSize, WindowBuilder, WindowEvent, use_wry_event_handler, window};
use dioxus::prelude::*;

use crate::chat::{ChatPane, ChatTabs};
use crate::state::AppState;

const LINUX: bool = cfg!(target_os = "linux");

fn config() -> Config {
    let builder = WindowBuilder::new()
        .with_title("Workbench · Chat")
        .with_inner_size(LogicalSize::new(560.0, 820.0))
        .with_min_inner_size(LogicalSize::new(320.0, 300.0))
        // Like the main window: no OS title bar on Linux. The chat's own bar moves it and
        // its edges resize it.
        .with_decorations(!LINUX);
    Config::new().with_window(builder).with_custom_head(format!("<style>{}</style>", crate::STYLE))
}

/// Opens and closes the chat window to follow `Layout::chat_detached`. Call once, from the
/// main window's root component.
pub fn use_chat_window(mut st: AppState) {
    let main_id: WindowId = use_hook(|| window().window.id());
    let mut opening = use_signal(|| false);

    use_effect(move || {
        let detached = st.layout.read().chat_detached;
        let open = *st.chat_window.read();
        match (detached, open) {
            (true, None) if !*opening.peek() => {
                opening.set(true);
                let dom = VirtualDom::new(DetachedChat).with_root_context(st);
                let pending = window().new_window(dom, config());
                spawn(async move {
                    let ctx = pending.await;
                    st.chat_window.set(Some(ctx.window.id()));
                    opening.set(false);
                });
            }
            (false, Some(id)) => {
                window().close_window(id);
                st.chat_window.set(None);
            }
            _ => {}
        }
    });

    use_wry_event_handler(move |event, _| {
        let Event::WindowEvent { event: WindowEvent::CloseRequested, window_id, .. } = event else { return };
        if *window_id == main_id {
            // Closing the main window quits, even with the chat window open. The chat window
            // reads the main window's state, so it can't outlive it; exiting here (rather than
            // letting the chat window close later) keeps it from rendering dropped state.
            // `chat_detached` stays set, so the chat reopens detached next time.
            if st.chat_window.peek().is_some() {
                std::process::exit(0);
            }
        } else if Some(*window_id) == *st.chat_window.peek() {
            // Closed from the OS (e.g. Alt+F4): put the chat back in the app.
            st.chat_window.set(None);
            st.set_layout(|l| l.chat_detached = false);
        }
    });
}

const EDGES: [(ResizeDirection, &str); 8] = [
    (ResizeDirection::North, "n"),
    (ResizeDirection::South, "s"),
    (ResizeDirection::East, "e"),
    (ResizeDirection::West, "w"),
    (ResizeDirection::NorthEast, "ne"),
    (ResizeDirection::NorthWest, "nw"),
    (ResizeDirection::SouthEast, "se"),
    (ResizeDirection::SouthWest, "sw"),
];

/// The root of the chat window.
#[component]
fn DetachedChat() -> Element {
    let mut st = use_context::<AppState>();
    let cwd = st.chat().dir.read().display().to_string();
    let attach = move |_| st.set_layout(|l| l.chat_detached = false);

    rsx! {
        div { class: "chat-window",
            div {
                class: "chat-window-bar",
                title: if LINUX { "Drag to move · double-click to maximize" } else { "" },
                onmousedown: move |_| {
                    if LINUX {
                        window().drag();
                    }
                },
                ondoubleclick: move |_| {
                    if LINUX {
                        window().toggle_maximized();
                    }
                },
                span { class: "chat-grip", "⠿" }
                b { "Chat" }
                ChatTabs {}
                span { class: "cwd small-text", title: "{cwd}", "{cwd}" }
                div { class: "spacer" }
                button {
                    class: "small",
                    title: "Put the chat back inside the main window",
                    onmousedown: move |e| e.stop_propagation(),
                    onclick: attach,
                    "⇲ Attach"
                }
                if LINUX {
                    button {
                        class: "small",
                        title: "Minimize",
                        onmousedown: move |e| e.stop_propagation(),
                        onclick: move |_| window().window.set_minimized(true),
                        "–"
                    }
                    button {
                        class: "small",
                        title: "Close the window and put the chat back in the app",
                        onmousedown: move |e| e.stop_propagation(),
                        onclick: attach,
                        "✕"
                    }
                }
            }
            ChatPane {}
            if LINUX {
                for (dir, name) in EDGES {
                    div {
                        key: "{name}",
                        class: "chat-edge {name}",
                        onmousedown: move |e: MouseEvent| {
                            e.stop_propagation();
                            let _ = window().window.drag_resize_window(dir);
                        },
                    }
                }
            }
        }
    }
}
