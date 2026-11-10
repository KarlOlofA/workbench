use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::chat::ChatPane;
use crate::code::CodePane;
use crate::graph_view::LeftPanel;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Screen {
    Graph,
    Chat,
    Code,
}

impl Screen {
    pub const ALL: [Screen; 3] = [Screen::Graph, Screen::Chat, Screen::Code];

    pub fn title(self) -> &'static str {
        match self {
            Screen::Graph => "Graph",
            Screen::Chat => "Chat",
            Screen::Code => "Code",
        }
    }

    fn pane_class(self) -> &'static str {
        match self {
            Screen::Graph => "pane-graph",
            Screen::Chat => "pane-chat",
            Screen::Code => "pane-code",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenState {
    #[default]
    Open,
    Minimized,
    Closed,
}

impl ScreenState {
    fn class(self) -> &'static str {
        match self {
            ScreenState::Open => "open",
            ScreenState::Minimized => "minimized",
            ScreenState::Closed => "closed",
        }
    }
}

pub type Layout = HashMap<Screen, ScreenState>;
pub type Sizes = HashMap<Screen, f64>;

pub const STEP: f64 = 5.0;
const MIN_PCT: f64 = 10.0;
const MAX_TOTAL: f64 = 85.0;

fn config_path(name: &str) -> Option<PathBuf> {
    crate::platform::config_dir().map(|d| d.join(name))
}

pub fn load_json<T: for<'de> Deserialize<'de> + Default>(name: &str) -> T {
    config_path(name)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_json<T: Serialize>(name: &str, value: &T) -> Result<(), String> {
    let path = config_path(name).ok_or("config: no home folder (HOME is not set)")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{name}: {e}"))?;
    }
    let text = serde_json::to_string_pretty(value).map_err(|e| format!("{name}: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("{name}: {e}"))
}

pub fn load() -> Layout {
    load_json("screens.json")
}

pub fn save(layout: &Layout) -> Result<(), String> {
    save_json("screens.json", layout)
}

pub fn load_sizes() -> Sizes {
    load_json("sizes.json")
}

pub fn save_sizes(sizes: &Sizes) -> Result<(), String> {
    save_json("sizes.json", sizes)
}

pub fn state_of(layout: &Layout, screen: Screen) -> ScreenState {
    layout.get(&screen).copied().unwrap_or_default()
}

fn open_screens(layout: &Layout) -> Vec<Screen> {
    Screen::ALL.into_iter().filter(|s| state_of(layout, *s) == ScreenState::Open).collect()
}

pub fn fill_screen(layout: &Layout) -> Option<Screen> {
    open_screens(layout).last().copied()
}

pub fn size_of(layout: &Layout, sizes: &Sizes, screen: Screen) -> f64 {
    sizes.get(&screen).copied().unwrap_or(match screen {
        Screen::Graph => 34.0,
        Screen::Chat if state_of(layout, Screen::Graph) == ScreenState::Open => 30.0,
        Screen::Chat => 42.0,
        Screen::Code => 40.0,
    })
}

pub fn clamp_size(layout: &Layout, sizes: &Sizes, screen: Screen, pct: f64) -> f64 {
    let fill = fill_screen(layout);
    let others: f64 = open_screens(layout)
        .into_iter()
        .filter(|s| *s != screen && Some(*s) != fill)
        .map(|s| size_of(layout, sizes, s))
        .sum();
    pct.clamp(MIN_PCT, (MAX_TOTAL - others).max(MIN_PCT))
}

pub fn step_target(layout: &Layout, sizes: &Sizes, active: Option<Screen>, dir: f64) -> Option<(Screen, f64)> {
    let open = open_screens(layout);
    let fill = *open.last()?;
    let (target, delta) = match active {
        Some(a) if a != fill && open.contains(&a) => (a, dir * STEP),
        _ => (*open.iter().rev().nth(1)?, -dir * STEP),
    };
    let next = clamp_size(layout, sizes, target, size_of(layout, sizes, target) + delta);
    Some((target, next))
}

#[derive(Clone, Copy, PartialEq)]
struct Drag {
    screen: Screen,
    start_x: f64,
    start_pct: f64,
    width: Option<f64>,
}

#[derive(Clone, Copy)]
struct Resize {
    drag: Signal<Option<Drag>>,
    active: Signal<Option<Screen>>,
    main: Signal<Option<Rc<MountedData>>>,
}

const KEYS_JS: &str = r#"
window.addEventListener("keydown", (e) => {
  if (e.metaKey && e.altKey && !e.shiftKey && !e.ctrlKey && (e.key === "ArrowLeft" || e.key === "ArrowRight")) {
    e.preventDefault();
    e.stopPropagation();
    dioxus.send(e.key === "ArrowRight" ? 1 : -1);
  }
}, true);
await new Promise(() => {});
"#;

#[component]
pub fn Screens() -> Element {
    let mut st = use_context::<AppState>();
    let mut res = use_context_provider(|| Resize {
        drag: Signal::new(None),
        active: Signal::new(None),
        main: Signal::new(None),
    });

    use_future(move || async move {
        let mut eval = document::eval(KEYS_JS);
        while let Ok(dir) = eval.recv::<i32>().await {
            let layout = st.screens.peek().clone();
            let sizes = st.sizes.peek().clone();
            if let Some((screen, pct)) = step_target(&layout, &sizes, *res.active.peek(), dir as f64) {
                st.set_size(screen, pct);
            }
        }
    });

    let dragging = res.drag.read().is_some();
    let mut end_drag = move || {
        if res.drag.peek().is_some() {
            res.drag.set(None);
            st.save_sizes();
        }
    };

    rsx! {
        div { class: "main", onmounted: move |e| res.main.set(Some(e.data())),
            ScreenSlot { screen: Screen::Graph }
            ScreenSlot { screen: Screen::Chat }
            ScreenSlot { screen: Screen::Code }
            if dragging {
                div {
                    class: "drag-overlay",
                    onmousemove: move |e| {
                        let Some(d) = *res.drag.peek() else { return };
                        let Some(width) = d.width.filter(|w| *w > 0.0) else { return };
                        let x = e.client_coordinates().x;
                        let layout = st.screens.peek().clone();
                        let sizes = st.sizes.peek().clone();
                        let pct = clamp_size(&layout, &sizes, d.screen, d.start_pct + (x - d.start_x) / width * 100.0);
                        st.sizes.write().insert(d.screen, pct);
                    },
                    onmouseup: move |_| end_drag(),
                    onmouseleave: move |_| end_drag(),
                }
            }
        }
    }
}

#[component]
fn ScreenSlot(screen: Screen) -> Element {
    let st = use_context::<AppState>();
    let mut res = use_context::<Resize>();
    let layout = st.screens.read().clone();

    match state_of(&layout, screen) {
        ScreenState::Closed => rsx! {},
        ScreenState::Minimized => rsx! { ScreenStrip { screen } },
        ScreenState::Open => {
            let is_fill = fill_screen(&layout) == Some(screen);
            let pct = size_of(&layout, &st.sizes.read(), screen);
            let style = if is_fill { "flex: 1 1 0".to_string() } else { format!("flex: none; width: {pct:.2}%") };
            let start_drag = move |e: MouseEvent| {
                e.prevent_default();
                let start_x = e.client_coordinates().x;
                res.drag.set(Some(Drag { screen, start_x, start_pct: pct, width: None }));
                res.active.set(Some(screen));
                let Some(m) = res.main.peek().clone() else { return };
                spawn(async move {
                    if let Ok(rect) = m.get_client_rect().await
                        && let Some(d) = res.drag.write().as_mut()
                    {
                        d.width = Some(rect.size.width);
                    }
                });
            };
            rsx! {
                div {
                    class: "pane {screen.pane_class()}",
                    style: "{style}",
                    onmousedown: move |_| res.active.set(Some(screen)),
                    onfocusin: move |_| res.active.set(Some(screen)),
                    match screen {
                        Screen::Graph => rsx! { LeftPanel {} },
                        Screen::Chat => rsx! { ChatPane {} },
                        Screen::Code => rsx! { CodePane {} },
                    }
                }
                if !is_fill {
                    div {
                        class: "splitter",
                        title: "Drag to resize  (⌘⌥← / ⌘⌥→ resizes the focused pane by 5%)",
                        onmousedown: start_drag,
                    }
                }
            }
        }
    }
}

#[component]
fn ScreenStrip(screen: Screen) -> Element {
    let mut st = use_context::<AppState>();
    rsx! {
        div {
            class: "screen-strip",
            title: "Restore {screen.title()}",
            onclick: move |_| st.set_screen(screen, ScreenState::Open),
            span { "{screen.title()}" }
        }
    }
}

#[component]
pub fn ScreenControls() -> Element {
    let mut st = use_context::<AppState>();
    let layout = st.screens.read().clone();

    rsx! {
        div { class: "screen-controls",
            for s in Screen::ALL {
                {
                    let state = state_of(&layout, s);
                    rsx! {
                        span { key: "{s.title()}", class: "screen-control {state.class()}",
                            match state {
                                ScreenState::Open => rsx! {
                                    span { "{s.title()}" }
                                    button { class: "small", title: "Minimize {s.title()}", onclick: move |_| st.set_screen(s, ScreenState::Minimized), "–" }
                                    button { class: "small", title: "Close {s.title()}", onclick: move |_| st.set_screen(s, ScreenState::Closed), "✕" }
                                },
                                ScreenState::Minimized => rsx! {
                                    button { class: "small", title: "Restore {s.title()}", onclick: move |_| st.set_screen(s, ScreenState::Open), "{s.title()}" }
                                    button { class: "small", title: "Close {s.title()}", onclick: move |_| st.set_screen(s, ScreenState::Closed), "✕" }
                                },
                                ScreenState::Closed => rsx! {
                                    button { class: "small", title: "Reopen {s.title()}", onclick: move |_| st.set_screen(s, ScreenState::Open), "+ {s.title()}" }
                                },
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_round_trips_through_json() {
        let mut layout = Layout::new();
        layout.insert(Screen::Graph, ScreenState::Closed);
        layout.insert(Screen::Chat, ScreenState::Minimized);
        let text = serde_json::to_string(&layout).unwrap();
        let back: Layout = serde_json::from_str(&text).unwrap();
        assert_eq!(back, layout);
        assert_eq!(state_of(&back, Screen::Code), ScreenState::Open);
    }

    #[test]
    fn step_resizes_active_pane_or_neighbour_of_fill() {
        let layout = Layout::new();
        let sizes = Sizes::new();
        assert_eq!(step_target(&layout, &sizes, Some(Screen::Graph), 1.0), Some((Screen::Graph, 39.0)));
        assert_eq!(step_target(&layout, &sizes, Some(Screen::Chat), -1.0), Some((Screen::Chat, 25.0)));
        assert_eq!(step_target(&layout, &sizes, Some(Screen::Code), 1.0), Some((Screen::Chat, 25.0)));
        assert_eq!(step_target(&layout, &sizes, None, -1.0), Some((Screen::Chat, 35.0)));
    }

    #[test]
    fn sizes_are_clamped_so_fill_pane_keeps_room() {
        let layout = Layout::new();
        let mut sizes = Sizes::new();
        sizes.insert(Screen::Graph, 50.0);
        assert_eq!(clamp_size(&layout, &sizes, Screen::Chat, 60.0), 35.0);
        assert_eq!(clamp_size(&layout, &sizes, Screen::Chat, 2.0), 10.0);
    }

    #[test]
    fn single_open_pane_has_nothing_to_step() {
        let mut layout = Layout::new();
        layout.insert(Screen::Graph, ScreenState::Closed);
        layout.insert(Screen::Chat, ScreenState::Minimized);
        assert_eq!(step_target(&layout, &Sizes::new(), Some(Screen::Code), 1.0), None);
    }
}
