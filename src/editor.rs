use dioxus::document::Eval;
use dioxus::prelude::*;
use serde::Deserialize;
use serde_json::json;

use crate::state::{AppState, Tab};

pub const BUNDLE: &str = include_str!("editor.bundle.js");

const BRIDGE_JS: &str = r#"
let ed;
try {
  const host = document.getElementById("cm-host");
  if (!host) throw new Error("editor host element #cm-host not found");
  if (!window.WB) throw new Error("editor bundle did not load");
  ed = window.WB.mount(host, (m) => dioxus.send(m));
  dioxus.send({ type: "ready", width: host.clientWidth, height: host.clientHeight });
} catch (e) {
  dioxus.send({ type: "error", message: "editor failed to start: " + (e && e.stack || e) });
}
while (ed) {
  const m = await dioxus.recv();
  try {
    if (m.type === "load") ed.load(m.path, m.text, m.keep);
    else if (m.type === "focus") setTimeout(() => { window.dispatchEvent(new Event("resize")); ed.focus(); }, 0);
  } catch (e) {
    dioxus.send({ type: "error", message: "editor error: " + (e && e.stack || e) });
  }
}
"#;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum EditorMsg {
    Change { text: String },
    Save { text: String },
    Close { saved: bool },
    Reload,
    Ready { width: f64, height: f64 },
    Error { message: String },
}

#[component]
pub fn EditorView(visible: bool) -> Element {
    let mut st = use_context::<AppState>();
    let mut bridge = use_signal(|| None::<Eval>);

    use_effect(move || {
        let _ = st.editor_load.read();
        let Some(b) = bridge.read().clone() else { return };
        let keep = st.editor_load.peek().1;
        if let Some(f) = st.open_file.peek().as_ref() {
            let _ = b.send(json!({ "type": "load", "path": f.rel, "text": f.content, "keep": keep }));
        }
    });

    use_effect(move || {
        let on_editor = *st.tab.read() == Tab::Editor;
        if on_editor && let Some(b) = bridge.read().as_ref() {
            let _ = b.send(json!({ "type": "focus" }));
        }
    });

    let mut host_ready = use_signal(|| false);

    use_effect(move || {
        let on_editor = *st.tab.read() == Tab::Editor;
        if !on_editor || !*host_ready.read() || bridge.peek().is_some() {
            return;
        }
        eprintln!("[editor] creating bridge");
        let mut eval = document::eval(&format!("if (!window.WB) {{ {BUNDLE} }}\n{BRIDGE_JS}"));
        bridge.set(Some(eval.clone()));
        spawn(async move {
            loop {
                let msg = match eval.recv::<EditorMsg>().await {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("[editor] bridge closed: {e:?}");
                        st.editor_error.set(Some(format!("editor bridge closed: {e:?}")));
                        break;
                    }
                };
                match msg {
                    EditorMsg::Ready { width, height } => eprintln!("[editor] ready {width}x{height}"),
                    EditorMsg::Error { message } => {
                        eprintln!("[editor] {message}");
                        st.editor_error.set(Some(message));
                    }
                    EditorMsg::Change { text } => {
                        if let Some(f) = st.open_file.write().as_mut() {
                            f.content = text;
                        }
                    }
                    EditorMsg::Save { text } => {
                        if let Some(f) = st.open_file.write().as_mut() {
                            f.content = text;
                        }
                        st.save();
                    }
                    EditorMsg::Close { saved } => st.close_file(saved),
                    EditorMsg::Reload => {
                        let rel = st.open_file.peek().as_ref().map(|f| f.rel.clone());
                        if let Some(rel) = rel {
                            st.open(&rel);
                        }
                    }
                }
            }
        });
    });

    let file = st.open_file.read().clone();
    let error = st.editor_error.read().clone();
    let dirty = file.as_ref().is_some_and(|f| f.dirty());

    rsx! {
        // A class, not `style: ""`: Dioxus never applied the empty style, so the editor stayed display:none.
        div { class: if visible { "editor" } else { "editor hidden" },
            div { class: "editor-bar",
                match &file {
                    Some(f) => rsx! { span { class: "path", "{f.rel}" } },
                    None => rsx! { span { class: "muted", "No file open — pick one from Files or Changes" } },
                }
                if dirty { span { class: "dirty", "modified" } }
                if let Some(e) = error { span { class: "error", "{e}" } }
                div { class: "spacer" }
                span { class: "muted hint", ":w save  ·  :q close  ·  :e reload" }
                button { disabled: !dirty, onclick: move |_| st.revert(), "Revert" }
                button {
                    class: "primary",
                    disabled: !dirty,
                    onclick: move |_| st.save(),
                    "Save"
                }
            }
            div {
                id: "cm-host",
                class: if file.is_some() { "cm-host" } else { "cm-host hidden" },
                onmounted: move |_| host_ready.set(true),
            }
        }
    }
}
