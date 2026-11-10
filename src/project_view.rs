use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;

use crate::app::open_folder;
use crate::github;
use crate::graph_view::{View, zoom_at};
use crate::projects::{self, NOTE_GAP, NOTE_W, Project, REPO_H, REPO_W, RepoInfo};
use crate::state::{AppState, Cmd};

const NOTE_H: f64 = 96.0;
const ITEM_W: f64 = REPO_W + NOTE_GAP + NOTE_W;

#[derive(Clone, Copy)]
struct Selected(Signal<Option<String>>);

#[derive(Clone, PartialEq)]
enum Drag {
    Pan { sx: f64, sy: f64, ox: f64, oy: f64, moved: bool },
    Repo { path: String, sx: f64, sy: f64, ox: f64, oy: f64, moved: bool },
}

fn fit(p: &Project, w: f64, h: f64) -> View {
    let pos = p.positions();
    let Some((x0, y0, x1, y1)) = pos.values().fold(None, |acc: Option<(f64, f64, f64, f64)>, q| {
        let (a, b, c, d) = acc.unwrap_or((q[0], q[1], q[0] + ITEM_W, q[1] + NOTE_H));
        Some((a.min(q[0]), b.min(q[1]), c.max(q[0] + ITEM_W), d.max(q[1] + NOTE_H)))
    }) else {
        return (40.0, 40.0, 1.0);
    };
    let pad = 40.0;
    let z = ((w - 2.0 * pad) / (x1 - x0)).min((h - 2.0 * pad) / (y1 - y0)).clamp(0.3, 1.0);
    ((w - (x1 - x0) * z) / 2.0 - x0 * z, (h - (y1 - y0) * z) / 2.0 - y0 * z, z)
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

fn fill_github_description(mut st: AppState, project: String, path: String) {
    let Some(repo) = st.projects.peek().iter().find(|p| p.id == project).and_then(|p| p.repo(&path)).and_then(|r| r.github.clone()) else {
        return;
    };
    let dir = PathBuf::from(&path);
    spawn(async move {
        let Ok(Ok(info)) = tokio::task::spawn_blocking(move || github::view_repo(&dir, &repo)).await else { return };
        let Some(desc) = info.description.filter(|d| !d.trim().is_empty()) else { return };
        st.update_project(&project, |p| {
            if let Some(r) = p.repo_mut(&path).filter(|r| r.description.trim().is_empty()) {
                r.description = desc;
            }
        });
    });
}

fn add_repo(mut st: AppState, project: String, dir: PathBuf) {
    let mut added = Err(String::new());
    st.update_project(&project, |p| added = p.add_repo(&dir));
    match added {
        Ok(path) => fill_github_description(st, project, path),
        Err(e) if !e.is_empty() => st.workbench_error.set(Some(e)),
        Err(_) => {}
    }
}

fn pick_repo(st: AppState, project: String) {
    spawn(async move {
        let start = st.cwd.peek().parent().map(|p| p.to_path_buf()).unwrap_or_else(|| st.cwd.peek().clone());
        if let Some(handle) = rfd::AsyncFileDialog::new().set_directory(start).pick_folder().await {
            add_repo(st, project, handle.path().to_path_buf());
        }
    });
}

fn create_project(mut st: AppState, name: &str) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    let cwd = st.cwd.peek().clone();
    let id = projects::unique_id(&st.projects.peek(), name);
    let mut project = Project { id: id.clone(), name: name.to_string(), ..Default::default() };
    let include_cwd = cwd.is_dir() && projects::containing(&st.projects.peek(), &cwd).is_none();
    let first = include_cwd.then(|| project.add_repo(&cwd).ok()).flatten();
    st.update_projects(|list| list.push(project));
    st.project_view.set(Some(id.clone()));
    if let Some(path) = first {
        fill_github_description(st, id, path);
    }
}

#[component]
pub fn ProjectView(id: String) -> Element {
    let mut st = use_context::<AppState>();
    let claude = use_coroutine_handle::<Cmd>();
    let project = st.projects.read().iter().find(|p| p.id == id).cloned();
    let all: Vec<(String, String)> = st.projects.read().iter().map(|p| (p.id.clone(), p.name.clone())).collect();
    let mut new_name = use_signal(String::new);
    let mut creating = use_signal(|| false);
    use_context_provider(|| Selected(Signal::new(None)));
    let cwd = st.cwd.read().clone();
    let back_label = st
        .current_project()
        .and_then(|p| p.repos.iter().find(|r| r.is_at(&cwd)).map(|r| r.label()))
        .unwrap_or_else(|| cwd.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default());

    let mut create = move || {
        let name = new_name.peek().clone();
        if !name.trim().is_empty() {
            create_project(st, &name);
            new_name.set(String::new());
            creating.set(false);
        }
    };
    let new_form = rsx! {
        div { class: "inspector-actions",
            input {
                class: "project-name-input",
                placeholder: "Project name, e.g. Workbench",
                value: "{new_name}",
                onmounted: crate::chat::focus,
                oninput: move |e| new_name.set(e.value()),
                onkeydown: move |e: KeyboardEvent| match e.key() {
                    Key::Enter => create(),
                    Key::Escape => creating.set(false),
                    _ => {}
                },
            }
            button { class: "primary", disabled: new_name.read().trim().is_empty(), onclick: move |_| create(), "Create" }
            if !all.is_empty() {
                button { onclick: move |_| creating.set(false), "Cancel" }
            }
        }
    };
    let showing_form = *creating.read() || all.is_empty();

    rsx! {
        div { class: "graph-tab",
            div { class: "graph-toolbar",
                select {
                    class: "project-select",
                    value: "{id}",
                    onchange: move |e: FormEvent| match e.value().as_str() {
                        "__new__" => creating.set(true),
                        v => st.project_view.set(Some(v.to_string())),
                    },
                    if project.is_none() {
                        option { value: "", selected: true, "Choose a project…" }
                    }
                    for (pid, name) in all.clone() {
                        option { key: "{pid}", value: "{pid}", selected: pid == id, "◈ {name}" }
                    }
                    option { value: "__new__", "New project…" }
                }
                if let Some(p) = project.clone() {
                    {
                        let pid = p.id.clone();
                        let here = p.id.clone();
                        let cwd_in = p.contains(&cwd);
                        let cwd_dir = cwd.clone();
                        rsx! {
                            button {
                                title: "Add a local clone of another repository to this project",
                                onclick: move |_| pick_repo(st, pid.clone()),
                                "+ Repository"
                            }
                            if !cwd_in {
                                button {
                                    title: "Add the folder that is open now to this project",
                                    onclick: move |_| add_repo(st, here.clone(), cwd_dir.clone()),
                                    "+ Open folder"
                                }
                            }
                        }
                    }
                }
                div { class: "spacer" }
                button {
                    title: "Back to the graph of the open folder",
                    onclick: move |_| st.project_view.set(None),
                    "↓ {back_label}"
                }
            }
            div { class: "crumbs",
                match project.as_ref() {
                    Some(p) => rsx! { button { class: "crumb current project", "◈ {p.name}" } },
                    None => rsx! { button { class: "crumb current project", "◈ Projects" } },
                }
                span { class: "muted small-text", "Double-click a repository to open its graph" }
            }
            div { class: "graph-canvas-wrap",
                match project.clone() {
                    Some(p) => rsx! { ProjectCanvas { key: "{p.id}", project: p } },
                    None => rsx! { div { class: "graph-canvas" } },
                }
                if showing_form || project.is_none() {
                    div { class: "graph-empty",
                        if all.is_empty() {
                            p { "No projects yet." }
                            p { class: "muted",
                                "A project groups the repositories that make up one product, for example a frontend, a backend and a host. Each repository keeps its own graph; the project shows them side by side."
                            }
                        } else if !showing_form {
                            p { "Pick a project above, or create a new one." }
                        }
                        if showing_form {
                            {new_form}
                            p { class: "muted small-text", "The open folder is added as the first repository." }
                        }
                    }
                } else if project.as_ref().is_some_and(|p| p.repos.is_empty()) {
                    div { class: "graph-empty",
                        p { "This project has no repositories yet." }
                        p { class: "muted", "Add the local clones of its repositories with + Repository." }
                    }
                }
            }
            div { class: "graph-bottom",
                div { class: "pane-body",
                    if let Some(p) = project {
                        ProjectInspector { project: p, open: move |dir: PathBuf| open_folder(st, claude, dir) }
                    }
                }
            }
        }
    }
}

#[component]
fn ProjectCanvas(project: Project) -> Element {
    let mut st = use_context::<AppState>();
    let claude = use_coroutine_handle::<Cmd>();
    let mut view = use_signal(|| (40.0f64, 40.0f64, 1.0f64));
    let mut drag = use_signal(|| None::<Drag>);
    let mut mounted = use_signal(|| None::<Rc<MountedData>>);
    let Selected(mut selected) = use_context::<Selected>();
    let pid = project.id.clone();

    let paths: Vec<String> = project.repos.iter().map(|r| r.path.clone()).collect();
    let infos = use_resource(use_reactive!(|paths| async move {
        let _ = st.repo_version.read();
        let repos: Vec<projects::ProjectRepo> = paths.iter().map(|p| projects::ProjectRepo { path: p.clone(), ..Default::default() }).collect();
        tokio::task::spawn_blocking(move || repos.iter().map(|r| (r.path.clone(), projects::info(r))).collect::<HashMap<String, RepoInfo>>())
            .await
            .unwrap_or_default()
    }));

    let fit_project = project.clone();
    let do_fit = move || {
        let Some(m) = mounted.peek().clone() else { return };
        let p = fit_project.clone();
        spawn(async move {
            if let Ok(rect) = m.get_client_rect().await {
                view.set(fit(&p, rect.size.width, rect.size.height));
            }
        });
    };
    let zoom_center = move |factor: f64| {
        let Some(m) = mounted.peek().clone() else { return };
        spawn(async move {
            if let Ok(rect) = m.get_client_rect().await {
                let v = *view.peek();
                view.set(zoom_at(v, rect.size.width / 2.0, rect.size.height / 2.0, factor));
            }
        });
    };

    let end_pid = pid.clone();
    let mut end_drag = move || {
        let d = drag.peek().clone();
        drag.set(None);
        match d {
            Some(Drag::Pan { moved: false, .. }) => selected.set(None),
            Some(Drag::Repo { moved: true, .. }) => st.update_project(&end_pid, |_| {}),
            Some(Drag::Repo { path, moved: false, .. }) => selected.set(Some(path)),
            _ => {}
        }
    };

    let move_pid = pid.clone();
    let on_move = move |e: MouseEvent| {
        let Some(d) = drag.peek().clone() else { return };
        let p = e.client_coordinates();
        match d {
            Drag::Pan { sx, sy, ox, oy, moved } => {
                let (dx, dy) = (p.x - sx, p.y - sy);
                if !moved && dx.abs() + dy.abs() < 3.0 {
                    return;
                }
                let mut v = view.write();
                v.0 = ox + dx;
                v.1 = oy + dy;
                drag.set(Some(Drag::Pan { sx, sy, ox, oy, moved: true }));
            }
            Drag::Repo { path, sx, sy, mut ox, mut oy, moved } => {
                let z = view.peek().2;
                let (dx, dy) = ((p.x - sx) / z, (p.y - sy) / z);
                if !moved && dx.abs() + dy.abs() < 3.0 / z {
                    return;
                }
                {
                    let mut list = st.projects.write();
                    let Some(proj) = list.iter_mut().find(|x| x.id == move_pid) else { return };
                    if !moved {
                        proj.pin_all();
                        if let Some(start) = proj.repo(&path).and_then(|r| r.pos) {
                            (ox, oy) = (start[0], start[1]);
                        }
                    }
                    if let Some(r) = proj.repo_mut(&path) {
                        r.pos = Some([(ox + dx).round(), (oy + dy).round()]);
                    }
                }
                drag.set(Some(Drag::Repo { path, sx, sy, ox, oy, moved: true }));
            }
        }
    };

    let positions = project.positions();
    let infos = infos.read().clone().unwrap_or_default();
    let sel = selected.read().clone();
    let cwd = st.cwd.read().clone();
    let (px, py, z) = *view.read();
    let grid = 24.0 * z;

    rsx! {
        div {
            class: "graph-canvas project-canvas",
            style: "background-size: {grid}px {grid}px; background-position: {px}px {py}px;",
            onmounted: {
                let f = do_fit.clone();
                move |e: MountedEvent| {
                    mounted.set(Some(e.data()));
                    f();
                }
            },
            onmousedown: move |e: MouseEvent| {
                if e.trigger_button() != Some(MouseButton::Primary) {
                    return;
                }
                let p = e.client_coordinates();
                let (ox, oy, _) = *view.peek();
                drag.set(Some(Drag::Pan { sx: p.x, sy: p.y, ox, oy, moved: false }));
            },
            onmousemove: on_move,
            onmouseup: {
                let mut f = end_drag.clone();
                move |_| f()
            },
            onmouseleave: move |_| end_drag(),
            onwheel: move |e: WheelEvent| {
                e.prevent_default();
                let d = e.delta().strip_units();
                let m = e.modifiers();
                if m.ctrl() || m.meta() {
                    zoom_center((-d.y * 0.01).exp());
                } else {
                    let mut v = view.write();
                    v.0 -= d.x;
                    v.1 -= d.y;
                }
            },
            div { class: "graph-world", style: "transform: translate({px}px, {py}px) scale({z});",
                for r in project.repos.clone() {
                    {
                        let [x, y] = positions.get(&r.path).copied().unwrap_or([0.0, 0.0]);
                        let info = infos.get(&r.path).cloned();
                        let missing = info.as_ref().is_some_and(|i| !i.exists);
                        let here = r.is_at(&cwd);
                        let label = r.label();
                        let meta = match &info {
                            None => "…".to_string(),
                            Some(i) if !i.exists => "folder not found".to_string(),
                            Some(i) if !i.has_graph => "no graph yet".to_string(),
                            Some(i) if i.modules > 0 => format!("◆ {} · {}", count(i.nodes, "node"), count(i.modules, "module")),
                            Some(i) => format!("◆ {}", count(i.nodes, "node")),
                        };
                        let gh = r.github.clone();
                        let classes = format!(
                            "gnode repo-node{}{}{}",
                            if sel.as_deref() == Some(r.path.as_str()) { " selected" } else { "" },
                            if here { " current" } else { "" },
                            if missing { " missing" } else { "" },
                        );
                        let down_path = r.path.clone();
                        let open_dir = r.dir();
                        let note_pid = pid.clone();
                        let note_path = r.path.clone();
                        let nx = x + REPO_W + NOTE_GAP;
                        rsx! {
                            div { key: "{r.path}", class: "repo-item",
                                div {
                                    class: "{classes}",
                                    style: "left: {x}px; top: {y}px; width: {REPO_W}px; height: {REPO_H}px; --kind: #56b6c2;",
                                    title: "{r.path}\n\nDouble-click to open this repository's graph",
                                    onmousedown: move |e: MouseEvent| {
                                        if e.trigger_button() != Some(MouseButton::Primary) {
                                            return;
                                        }
                                        e.stop_propagation();
                                        let p = e.client_coordinates();
                                        drag.set(Some(Drag::Repo { path: down_path.clone(), sx: p.x, sy: p.y, ox: x, oy: y, moved: false }));
                                    },
                                    ondoubleclick: move |e: MouseEvent| {
                                        e.stop_propagation();
                                        open_folder(st, claude, open_dir.clone());
                                    },
                                    div { class: "gnode-title", "{label}" }
                                    div { class: "gnode-meta",
                                        span { class: "gnode-kind", "{meta}" }
                                        if here {
                                            span { class: "gnode-status", "open" }
                                        }
                                    }
                                    if let Some(g) = gh {
                                        div { class: "repo-gh", "{g}" }
                                    }
                                }
                                textarea {
                                    class: "repo-note",
                                    style: "left: {nx}px; top: {y}px; width: {NOTE_W}px; height: {NOTE_H}px;",
                                    placeholder: "What does {label} do in this project?",
                                    value: "{r.description}",
                                    onmousedown: move |e| e.stop_propagation(),
                                    onwheel: move |e| e.stop_propagation(),
                                    oninput: move |e| {
                                        let v = e.value();
                                        st.update_project(&note_pid, |p| {
                                            if let Some(r) = p.repo_mut(&note_path) {
                                                r.description = v;
                                            }
                                        });
                                    },
                                }
                            }
                        }
                    }
                }
            }
            div { class: "zoom-controls", onmousedown: move |e| e.stop_propagation(),
                button { onclick: move |_| zoom_center(1.0 / 1.2), "−" }
                button { onclick: move |_| zoom_center(1.2), "+" }
                button { onclick: move |_| do_fit(), "Fit" }
            }
        }
    }
}

#[component]
fn ProjectInspector(project: Project, open: EventHandler<PathBuf>) -> Element {
    let mut st = use_context::<AppState>();
    let Selected(mut selected) = use_context::<Selected>();
    let mut confirm_delete = use_signal(|| false);
    let pid = project.id.clone();
    let sel = selected.read().clone().and_then(|path| project.repo(&path).cloned());

    match sel {
        Some(r) => {
            let title_pid = pid.clone();
            let title_path = r.path.clone();
            let remove_pid = pid.clone();
            let remove_path = r.path.clone();
            let refresh_pid = pid.clone();
            let refresh_path = r.path.clone();
            let dir = r.dir();
            let label = r.label();
            rsx! {
                div { class: "inspector",
                    div { class: "inspector-head",
                        b { "{label}" }
                        div { class: "spacer" }
                        button { class: "primary", onclick: move |_| open.call(dir.clone()), "Open graph" }
                    }
                    label { "Title" }
                    input {
                        value: "{r.title}",
                        placeholder: "{label}",
                        oninput: move |e| {
                            let v = e.value();
                            st.update_project(&title_pid, |p| {
                                if let Some(r) = p.repo_mut(&title_path) {
                                    r.title = v;
                                }
                            });
                        },
                    }
                    label { "Folder" }
                    code { class: "small-text", "{r.path}" }
                    label { "GitHub" }
                    div { class: "inspector-actions",
                        match r.github.clone() {
                            Some(g) => rsx! { code { "{g}" } },
                            None => rsx! { span { class: "muted small-text", "No GitHub remote found" } },
                        }
                        button {
                            class: "small",
                            title: "Read the origin remote again",
                            onclick: move |_| {
                                let gh = projects::github_remote(&PathBuf::from(&refresh_path));
                                st.update_project(&refresh_pid, |p| {
                                    if let Some(r) = p.repo_mut(&refresh_path) {
                                        r.github = gh;
                                    }
                                });
                            },
                            "↻"
                        }
                    }
                    div { class: "inspector-actions",
                        button {
                            onclick: move |_| {
                                let path = remove_path.clone();
                                st.update_project(&remove_pid, |p| p.repos.retain(|r| r.path != path));
                                selected.set(None);
                            },
                            "Remove from project"
                        }
                    }
                }
            }
        }
        None => {
            let name_pid = pid.clone();
            let desc_pid = pid.clone();
            let del_pid = pid.clone();
            let n = project.repos.len();
            let noun = if n == 1 { "repository" } else { "repositories" };
            rsx! {
                div { class: "inspector",
                    div { class: "inspector-head",
                        b { "◈ {project.name}" }
                        span { class: "muted small-text", "{n} {noun}" }
                    }
                    label { "Name" }
                    input {
                        value: "{project.name}",
                        oninput: move |e| {
                            let v = e.value();
                            st.update_project(&name_pid, |p| p.name = v);
                        },
                    }
                    label { "Description" }
                    textarea {
                        class: "desc",
                        placeholder: "What is this product?",
                        value: "{project.description}",
                        oninput: move |e| {
                            let v = e.value();
                            st.update_project(&desc_pid, |p| p.description = v);
                        },
                    }
                    div { class: "muted small-text", "Select a repository to edit it. Projects are stored in ~/.config/workbench/projects.json." }
                    div { class: "inspector-actions",
                        if *confirm_delete.read() {
                            span { class: "small-text", "Delete the project? The repositories are not touched." }
                            button {
                                class: "danger",
                                onclick: move |_| {
                                    let id = del_pid.clone();
                                    st.update_projects(|list| list.retain(|p| p.id != id));
                                    st.show_project(None);
                                    confirm_delete.set(false);
                                },
                                "Delete"
                            }
                            button { onclick: move |_| confirm_delete.set(false), "Cancel" }
                        } else {
                            button { onclick: move |_| confirm_delete.set(true), "Delete project" }
                        }
                    }
                }
            }
        }
    }
}
