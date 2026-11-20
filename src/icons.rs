//! The line icons of the app's chrome: buttons in the sidebar, screen bars, chat and graph
//! controls. The shapes are from Lucide (lucide.dev, ISC licence), drawn on a 24×24 grid with
//! round strokes in the text colour; `.icon` in theme.css sets the stroke.

use dioxus::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Plus,
    X,
    ChevronLeft,
    ChevronRight,
    ArrowLeft,
    ArrowUp,
    List,
    Settings,
    Folder,
    ZoomIn,
    ZoomOut,
    Fit,
}

impl Icon {
    fn path(self) -> &'static str {
        match self {
            Icon::Plus => "M5 12h14M12 5v14",
            Icon::X => "M18 6 6 18M6 6l12 12",
            Icon::ChevronLeft => "m15 18-6-6 6-6",
            Icon::ChevronRight => "m9 18 6-6-6-6",
            Icon::ArrowLeft => "m12 19-7-7 7-7M19 12H5",
            Icon::ArrowUp => "m5 12 7-7 7 7M12 19V5",
            Icon::List => "M3 6h.01M3 12h.01M3 18h.01M8 6h13M8 12h13M8 18h13",
            Icon::Settings => SETTINGS,
            Icon::Folder => {
                "M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"
            }
            Icon::ZoomIn => "M3 11a8 8 0 1 0 16 0 8 8 0 1 0-16 0m18 10-4.3-4.3M11 8v6M8 11h6",
            Icon::ZoomOut => "M3 11a8 8 0 1 0 16 0 8 8 0 1 0-16 0m18 10-4.3-4.3M8 11h6",
            Icon::Fit => "M8 3H5a2 2 0 0 0-2 2v3m18 0V5a2 2 0 0 0-2-2h-3m0 18h3a2 2 0 0 0 2-2v-3M3 16v3a2 2 0 0 0 2 2h3",
        }
    }
}

const SETTINGS: &str = "M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2zM9 12a3 3 0 1 0 6 0 3 3 0 1 0-6 0";

/// An icon the size of the surrounding text, in its colour.
#[component]
pub fn IconSvg(icon: Icon) -> Element {
    rsx! {
        svg { class: "icon", view_box: "0 0 24 24", "aria-hidden": "true",
            path { d: "{icon.path()}" }
        }
    }
}
