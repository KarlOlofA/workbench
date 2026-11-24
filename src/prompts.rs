use std::path::Path;

use crate::claims::BranchClaim;
use crate::graph::{BASELINE_FILE, Change, ChangeKind, Decision, Field, Flow, GRAPH_FILE, Graph, Node, Reference, Subject};
use crate::projects::Project;
use crate::reference::{self, Refs};
use crate::rules::{self, Rule};
use crate::claude::ChatMode;
use crate::providers::{Provider, Settings};
use crate::work_order::{self, WorkOrder};

pub const QUESTIONS_MARKER: &str = "[[workbench:awaiting-answers]]";

pub fn asks_questions(text: &str) -> bool {
    text.contains(QUESTIONS_MARKER)
}

pub fn strip_marker(text: &str) -> String {
    text.replace(QUESTIONS_MARKER, "").trim_end().to_string()
}

/// An agent starts another agent with a block like
/// `[[workbench:spawn-agent name="Tests"]]Write tests for the parser[[/workbench:spawn-agent]]`.
/// With `repo="backend"` after the name, the agent starts in a new session in that repository
/// of the project instead of in the same session. With `provider="vibe" model="…"` the agent
/// runs on that provider and model instead of the ones in the settings.
const SPAWN_OPEN: &str = "[[workbench:spawn-agent ";
const SPAWN_HEADER_END: &str = "]]";
const SPAWN_CLOSE: &str = "[[/workbench:spawn-agent]]";

/// An agent that another agent's reply asks to start.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnRequest {
    pub name: String,
    /// The project repository to start it in; `None` starts it in the same session.
    pub repo: Option<String>,
    /// The provider id to run it on; `None` uses the settings.
    pub provider: Option<String>,
    /// The model to run it on; `None` uses the provider's model in the settings.
    pub model: Option<String>,
    pub task: String,
}

/// Told to every agent, so it can start other agents when the user asks for them. Agents in
/// a project also learn how to start an agent in another of its repositories. Every agent learns
/// the providers and models it can start an agent on.
pub fn spawn_agent_instructions(project: Option<&Project>, settings: &Settings) -> String {
    let mut out = format!(
        "\n\nYou are one agent in a Workbench chat session, and you can start more agents in the same session and folder. \
         Do this only when the user asks for another agent (or agents). For each one, end your reply with a block: \
         `{SPAWN_OPEN}name=\"<short name>\"{SPAWN_HEADER_END}<the complete task for that agent>{SPAWN_CLOSE}`. \
         The new agent sees only that task, so include everything it needs to know.\n"
    );
    let repos: Vec<String> = project.map(|p| p.repos.iter().map(|r| format!("`{}`", r.label())).collect()).unwrap_or_default();
    if !repos.is_empty() {
        out.push_str(&format!(
            "To start an agent in a repository of the project instead, add the repository after the name: \
             `{SPAWN_OPEN}name=\"<short name>\" repo=\"<repository>\"{SPAWN_HEADER_END}<task>{SPAWN_CLOSE}`. \
             It starts in a new chat session in that repository. The repositories are: {}.\n",
            repos.join(", ")
        ));
    }
    out.push_str(&format!(
        "When the user names a provider or model for the work (for example \"use vibe large 4 to build X\"), start an agent for it          and add the provider and model to the block: `{SPAWN_OPEN}name=\"<short name>\" provider=\"<provider id>\" model=\"<model>\"{SPAWN_HEADER_END}<task>{SPAWN_CLOSE}`          (together with `repo=` when the work is in another repository). Do this even when the work is in this folder or the provider is the one you run on:          the user's settings are only the default. Pick the listed model closest to what the user said; if none fits, use the user's name for it.          Leave `model` out to use the provider's default. The providers and their models:\n"
    ));
    for p in Provider::ALL {
        out.push_str(&format!("- `{}` ({}): {}\n", p.id(), p.label(), settings.model_choices(p).join(", ")));
    }
    out
}

/// The value of `key="value"` in a spawn block's header.
fn header_value(header: &str, key: &str) -> Option<String> {
    let start = header.find(&format!("{key}=\""))? + key.len() + 2;
    let len = header[start..].find('"')?;
    Some(header[start..start + len].trim().to_string()).filter(|v| !v.is_empty())
}

/// The reply without its spawn blocks, and every agent it asks to start.
pub fn spawn_requests(text: &str) -> (String, Vec<SpawnRequest>) {
    let mut shown = String::new();
    let mut spawns = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(SPAWN_OPEN) {
        let after_open = &rest[start + SPAWN_OPEN.len()..];
        let Some(header_end) = after_open.find(SPAWN_HEADER_END) else { break };
        let header = &after_open[..header_end];
        let body = &after_open[header_end + SPAWN_HEADER_END.len()..];
        let Some(close) = body.find(SPAWN_CLOSE) else { break };
        shown.push_str(&rest[..start]);
        spawns.push(SpawnRequest {
            name: header_value(header, "name").unwrap_or_else(|| "Agent".into()),
            repo: header_value(header, "repo"),
            provider: header_value(header, "provider"),
            model: header_value(header, "model"),
            task: body[..close].trim().to_string(),
        });
        rest = &body[close + SPAWN_CLOSE.len()..];
    }
    shown.push_str(rest);
    (shown.trim_end().to_string(), spawns)
}

/// An agent points at the node(s) responsible for a behaviour with blocks like
/// `[[workbench:focus node="scope" module="auth" confidence="high"]]`, and Workbench zooms the
/// graph to the one the user picks. `module` is left out for nodes of the main network.
const FOCUS_OPEN: &str = "[[workbench:focus ";

/// A node an agent's reply names as responsible for what the user asked about.
#[derive(Debug, Clone, PartialEq)]
pub struct FocusRequest {
    /// The module the node is in; `None` for the main network.
    pub module: Option<String>,
    pub node: String,
    /// `high`, `medium` or `low`, as the agent judged it.
    pub confidence: String,
}

/// Told to every agent with a graph, so asking about a feature zooms the graph to the node(s)
/// that implement it, quickly and without a code search first.
pub fn focus_instructions() -> String {
    format!(
        "\n\nShowing features on the graph: whenever the user asks to see, find or understand a feature or behaviour \
         (\"show me X\", \"where is X\", \"what handles X?\", \"which part does X?\"), or names a part of the app you then talk about, \
         point the graph at it. Be quick: read `{GRAPH_FILE}` and match the request against node titles, descriptions and links. \
         Don't search or read the code first, and don't ask a follow-up question; answer from the graph in a few sentences, and read code only if the user asks for more. \
         End the reply with one block per node: \
         `{FOCUS_OPEN}node=\"<node id>\" module=\"<module id, only for nodes inside a module>\" confidence=\"high|medium|low\"{SPAWN_HEADER_END}`. \
         Workbench zooms the graph to the node and makes it glow; with one block it zooms there right away. \
         When several nodes plausibly match, list them all with honest confidences, best first, so the user can pick; don't pick one for them. \
         After you change code, also add blocks for the nodes you changed so the user can see where the work landed.\n"
    )
}

/// Told to every agent with a graph: find code through the graph first (PRO-233). `command` runs
/// `query` (see `crate::query`) on the agent's folder.
pub fn query_instructions(command: &str) -> String {
    format!(
        "\n\nFind code through the graph before searching the file system, in this order: \
         1. If the user selected nodes, check first that one fits the request from its description and files. If it does, work there. If not, step out one node at a time: open a single linked node with `{command} node <id> [--module <m>]` (it prints the node with its files and the nodes it links to and from), check it, and only then try the next. \
         2. Otherwise read the node names with `{command} overview` (one line per node, `module/id: title (kind)`) or `{command} search <words>`, pick the nodes that look right and open them with `node`. \
         3. Only if no node fits, search the file system (grep, glob) and say so in your reply, naming the files the graph is missing. \
         `{command} file <path>` lists the nodes that own a file, `{command} history <id>` a node's recorded changes and `{command} modules` every module. All print JSON. \
         Workbench flags files you change that no node owns, so say which node they belong to.\n"
    )
}

/// The reply without its focus blocks, and every node they name.
pub fn focus_requests(text: &str) -> (String, Vec<FocusRequest>) {
    let mut shown = String::new();
    let mut focus = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(FOCUS_OPEN) {
        let after_open = &rest[start + FOCUS_OPEN.len()..];
        let Some(header_end) = after_open.find(SPAWN_HEADER_END) else { break };
        let header = &after_open[..header_end];
        shown.push_str(&rest[..start]);
        if let Some(node) = header_value(header, "node") {
            focus.push(FocusRequest {
                module: header_value(header, "module"),
                node,
                confidence: header_value(header, "confidence").unwrap_or_else(|| "unsure".into()),
            });
        }
        rest = &after_open[header_end + SPAWN_HEADER_END.len()..];
    }
    shown.push_str(rest);
    (shown.trim_end().to_string(), focus)
}

/// The system prompt of a project-level agent: it works in the folder that holds the project's
/// repositories and hands work in a repository to an agent started there.
pub fn project_agent_prompt(project: &Project, folder: &Path) -> String {
    let mut out = format!("You are the project-level agent of the project “{}”", project.name.trim());
    if !project.description.trim().is_empty() {
        out.push_str(&format!(": {}", project.description.trim()));
    }
    out.push_str(&format!(
        ".\nYou work in `{}`, the folder that holds the project's repositories. They are local clones you can read with your file tools; \
         each may have its own architecture graph at `{GRAPH_FILE}` in its folder, so read that first for an overview:\n",
        folder.display()
    ));
    for r in &project.repos {
        let gh = r.github.as_deref().map(|g| format!(" (GitHub `{g}`)")).unwrap_or_default();
        let desc = if r.description.trim().is_empty() { String::new() } else { format!(": {}", r.description.trim()) };
        out.push_str(&format!("- `{}` at `{}`{gh}{desc}\n", r.label(), r.path));
    }
    out.push_str(
        "\nAnswer questions about the project yourself. When the user wants a change made in a repository, \
         start an agent in that repository with the task (see below) instead of editing it yourself, \
         so the change is made with that repository's graph and rules.\n",
    );
    out
}

fn mode_section(mode: ChatMode, action: &str) -> String {
    match mode {
        ChatMode::Plan => format!(
            "## Mode: Plan\n\n\
             You are in plan mode: do NOT edit any files yet. Before you {action}, check whether the request is clear enough to do it well: scope, responsibilities, how it connects to what already exists, and anything that contradicts the code, the graph or the rules. \
             If anything material is unclear, reply with a short numbered list of concrete questions (suggest a sensible default for each), \
             then a brief outline of what you intend to do as you currently understand it, and end the reply with a line containing exactly `{QUESTIONS_MARKER}`. \
             The user will answer in the chat (ask again with the same marker only if something important is still unclear). \
             Once everything is clear, present your plan; the user approves it and picks how you run it before you change anything.\n\n"
        ),
        ChatMode::AcceptEdits | ChatMode::Auto | ChatMode::FullAuto => format!(
            "## Mode: {}\n\n\
             Do not stop to ask questions. Where the request is ambiguous or incomplete, fill in the gaps with the most reasonable choice consistent with the codebase, the graph and the rules, \
             then {action}. List every assumption you made in your final summary.\n\n",
            mode.label()
        ),
    }
}

/// Plan mode for providers without one of their own (everything but Workbench): Workbench
/// asks how to run the plan once the reply is in.
pub fn plan_mode_instructions() -> &'static str {
    "## Plan mode\n\n\
     You are in plan mode: do NOT edit files or run commands that change anything. \
     Find out exactly what the user wants you to work on: read the code you need, and ask concrete questions about anything unclear. \
     When everything is clear, reply with a step-by-step plan of the changes. The user approves the plan and picks how you run it; then you'll be asked to implement it.\n\n"
}

/// The instructions of a discussion chat (PRO-248): a read-only conversation next to the
/// execution chat of the agent called `agent`, for questions about what it does.
pub fn discussion_instructions(agent: &str) -> String {
    format!(
        "## Discussion chat\n\n\
         You are the discussion chat next to the execution chat of the agent “{agent}”. The user talks to you to understand \
         and discuss the work without starting anything. Each message from the user starts with an <execution_chat> block: \
         what happened in the execution chat since the user's previous message here (its messages, the agent's replies and \
         tool calls) and its current state. Use it, the graph and the code to explain what the agent did, is doing or plans \
         and why, to answer questions about the implementation and to weigh options.\n\n\
         You are read only: never edit files, run commands that change anything or start agents. When the user wants a \
         change, say what to tell the execution agent; the user sends it there. Never put the user's question back into \
         an <execution_chat> block, and don't repeat the block's contents unless asked.\n\n"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugMode {
    Trace,
    Instrument,
    Test,
}

impl DebugMode {
    pub const ALL: [DebugMode; 3] = [DebugMode::Trace, DebugMode::Instrument, DebugMode::Test];

    pub fn key(self) -> &'static str {
        match self {
            DebugMode::Trace => "trace",
            DebugMode::Instrument => "instrument",
            DebugMode::Test => "test",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DebugMode::Trace => "Trace & verify",
            DebugMode::Instrument => "Instrument with logging",
            DebugMode::Test => "Write end-to-end test",
        }
    }

    pub fn from_key(s: &str) -> Self {
        Self::ALL.into_iter().find(|m| m.key() == s).unwrap_or(DebugMode::Trace)
    }
}

const SCHEMA: &str = r#"{
  "nodes": [
    {
      "id": "kebab-case-unique-id",
      "title": "Short human name, e.g. `POST /users` or `Billing service`",
      "kind": "one of: app, api, route, handler, service, model, store, db, external, ui, screen, component, job, module, reference",
      "description": "2-4 plain sentences: what this part is responsible for and its inputs/outputs. Reasons go in `decisions`, must-hold rules in `invariants`.",
      "decisions": [{ "decision": "a choice that shaped this part", "why": "the reason, from the user, a commit, a ticket or a constraint", "instead_of": "optional: the alternative not taken", "source": "where it is recorded: `commit <sha>`, a ticket id, `chat <YYYY-MM-DD>` or `<file> comment`", "date": "YYYY-MM-DD" }],
      "invariants": ["optional: rules that must stay true of this part, e.g. `Never blocks the UI thread`"],
      "files": ["repo/relative/path.ext"],
      "links": ["ids of nodes this one calls, renders, or depends on"],
      "module": "optional: id of a module this node stands for (the node is an attachment point of that module)",
      "reference": "optional, only on `reference` nodes: { \"source\": \"local path, GitHub URL or owner/repo, website URL, or npm:/crate:/pypi:/go: package\", \"nodes\": [{ \"id\": \"node id in the referenced project's Workbench graph\", \"module\": \"optional module id there\" }] }"
    }
  ],
  "flows": [
    { "id": "kebab-id", "name": "Human name of an end-to-end path", "nodes": ["ordered", "node", "ids"] }
  ],
  "modules": [
    {
      "id": "kebab-case-module-id",
      "title": "Name of the isolated feature, e.g. `GitHub integration`",
      "description": "2-5 sentences: what the feature does and the interface the rest of the system uses.",
      "decisions": ["same decision shape as on nodes"],
      "invariants": ["same as on nodes"],
      "nodes": ["same node shape as above; ids only need to be unique inside this module"],
      "flows": ["same flow shape as above, using node ids of this module"]
    }
  ]
}"#;

const KNOWLEDGE_GUIDE: &str = "Decisions and invariants: the graph records how the app was steered, not only what each part is. \
A node's `decisions` list the non-obvious choices that shaped it, oldest first, each with its `why`, the alternative it replaced when known (`instead_of`), \
a `source` someone can check (`commit <sha>`, a ticket id such as PRO-123, `chat <YYYY-MM-DD>` for something the user asked for in a chat, or `<file> comment`) and a `date`. \
Its `invariants` are rules that must stay true of it. Only record reasons you found or were told; never invent a why. Leave both out when there is nothing to record.";

const MODULES_GUIDE: &str = "Modules: a module is an isolated feature with its own node network, stored in `modules[]`. \
It is attached to the main network (or to another module) by a node whose `module` field is the module id; one module can be attached in several places, \
and each attachment node's `description` says what that place uses the module for. Links never cross networks: \
callers link to the attachment node, and the module's own nodes only link to each other or to attachment nodes of other modules. \
A module must never be attached, directly or through other modules, inside itself.";

const REFERENCES_GUIDE: &str = "External references: a node of kind `reference` with a `reference` field points at something outside this project: \
a local folder or file, a GitHub repository (optionally a path and git ref), a website, or a package. It has no code in this project and exists to give you context. \
A link to it means the linking node uses, follows or mirrors that external thing. When the referenced project also uses Workbench, `reference.nodes` lists the nodes \
of its graph that matter here; their descriptions and files are the starting point for reading it. Read references only when you need them, never modify them, \
and never create code for a reference node itself.";

pub fn reference_context(title: &str, r: &Reference, refs: Option<&Refs>) -> String {
    let source = r.source.trim();
    if source.is_empty() {
        return format!("External reference “{title}” has no source yet.\n");
    }
    let mut p = String::new();
    let cwd = refs.map(|x| x.cwd.to_path_buf()).unwrap_or_default();
    match reference::parse(&cwd, source) {
        Some(src) => p.push_str(&format!("External reference “{title}”: `{source}` ({}). {}\n", src.label(), src.how_to_read(&cwd))),
        None => p.push_str(&format!("External reference “{title}”: `{source}` (unrecognised source).\n")),
    }
    let loaded = refs.and_then(|x| x.graph(source));
    if let Some(g) = loaded {
        p.push_str(&format!(
            "It uses Workbench: its architecture graph is `{}` ({} nodes, {} modules); its `files` are relative to `{}`.\n",
            g.location,
            g.graph.nodes.len(),
            g.graph.modules.len(),
            g.root
        ));
    }
    if r.nodes.is_empty() {
        return p;
    }
    p.push_str("Referenced nodes from its graph:\n");
    for rn in &r.nodes {
        let place = rn.module.as_deref().map(|m| format!(" in module `{m}`")).unwrap_or_default();
        match loaded.and_then(|g| reference::find(&g.graph, rn)) {
            Some(n) => {
                p.push_str(&format!("- `{}` — {} ({}){place}\n", n.id, n.title, n.kind));
                for line in n.description.trim().lines() {
                    p.push_str(&format!("  > {line}\n"));
                }
                if !n.files.is_empty() {
                    p.push_str(&format!("  Files: {}\n", n.files.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ")));
                }
            }
            None => p.push_str(&format!("- `{}`{place} (look it up in the referenced project's `{GRAPH_FILE}`)\n", rn.id)),
        }
    }
    p
}

fn references_section(graph: &Graph, refs: Option<&Refs>) -> String {
    let nodes: Vec<(&Node, &Reference)> = graph.all_nodes().into_iter().filter_map(|(_, n)| n.reference.as_ref().map(|r| (n, r))).collect();
    if nodes.is_empty() {
        return String::new();
    }
    let mut p = format!("{REFERENCES_GUIDE}\n\n## External references\n\n");
    for (n, r) in nodes {
        p.push_str(&format!("### `{}` — {}\n", n.id, n.title));
        if !n.description.trim().is_empty() {
            p.push_str(&quote(&n.description));
        }
        p.push_str(&reference_context(&n.title, r, refs));
        p.push('\n');
    }
    p
}

fn scope_label(graph: &Graph, scope: Option<&str>) -> String {
    match scope {
        None => "the main network".into(),
        Some(id) => format!("module `{id}` ({})", graph.module_title(id)),
    }
}

fn node_line(net: &Graph, root: &Graph, n: &Node) -> String {
    let module = n.module.as_deref().map(|m| format!(" [attaches module `{m}` — {}]", root.module_title(m))).unwrap_or_default();
    let external = n.reference.as_ref().map(|r| format!(" [external reference: `{}`]", r.source.trim())).unwrap_or_default();
    format!("- `{}` — {} ({}){module}{external} → {}\n", n.id, n.title, n.kind, titles(net, &n.links))
}

fn outline(graph: &Graph) -> String {
    let mut p = String::new();
    if graph.nodes.is_empty() && graph.modules.is_empty() {
        return "(the graph is empty)\n".into();
    }
    p.push_str("Main network:\n");
    for n in &graph.nodes {
        p.push_str(&node_line(graph, graph, n));
    }
    for m in &graph.modules {
        p.push_str(&format!("\nModule `{}` — {} (`modules[]` entry):\n", m.id, m.title));
        for n in &m.net.nodes {
            p.push_str(&node_line(&m.net, graph, n));
        }
    }
    p
}

fn rules_section(rules: &[&Rule], graph: &Graph) -> String {
    if rules.is_empty() {
        return String::new();
    }
    format!("## Project rules (must be followed)\n\n{}\n", rules::render(rules, graph))
}

fn quote(text: &str) -> String {
    let t = text.trim();
    if t.is_empty() {
        return "> (empty)\n".into();
    }
    t.lines().map(|l| format!("> {l}\n")).collect()
}

fn titles(graph: &Graph, ids: &[String]) -> String {
    if ids.is_empty() {
        return "none".into();
    }
    ids.iter().map(|id| format!("`{id}` ({})", graph.title_of(id))).collect::<Vec<_>>().join(", ")
}

fn decision_lines(n: &Node) -> Vec<String> {
    n.decisions.iter().map(Decision::line).collect()
}

/// The entries of `after` missing from `before`, and the entries of `before` missing from `after`.
fn added_removed<'a>(before: &'a [String], after: &'a [String]) -> (Vec<&'a String>, Vec<&'a String>) {
    let added = after.iter().filter(|x| !before.contains(x)).collect();
    let removed = before.iter().filter(|x| !after.contains(x)).collect();
    (added, removed)
}

/// A node's decisions and invariants, for prompts that work on the node. Empty when it has none.
fn knowledge(n: &Node) -> String {
    let mut out = String::new();
    if !n.decisions.is_empty() {
        out.push_str("Decisions made for it (keep to them unless the user asks otherwise):\n");
        for d in &n.decisions {
            out.push_str(&format!("- {}\n", d.line()));
        }
    }
    let invariants: Vec<&String> = n.invariants.iter().filter(|i| !i.trim().is_empty()).collect();
    if !invariants.is_empty() {
        out.push_str("Invariants (must stay true):\n");
        for i in invariants {
            out.push_str(&format!("- {i}\n"));
        }
    }
    out
}

fn files_line(n: &Node) -> String {
    if n.files.is_empty() {
        "Files: unknown — locate them by searching the codebase.\n".into()
    } else {
        format!("Files: {}\n", n.files.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", "))
    }
}

pub fn generate(existing: Option<&Graph>, rules: &[Rule]) -> String {
    let mut p = String::new();
    match existing {
        Some(g) if !g.nodes.is_empty() => {
            p.push_str(&format!(
                "Update the Workbench architecture graph in `{GRAPH_FILE}` so it accurately reflects this codebase as it is now.\n\n\
                 - Keep existing node `id`s, `pos` values and flows wherever the node still exists.\n\
                 - Keep the user's existing descriptions unless they are factually wrong about the current code; then correct them.\n\
                 - Keep existing `decisions` and `invariants`; add the ones you find while reading the code, its comments and `git log`.\n\
                 - Add nodes for parts that are missing, remove nodes whose code no longer exists, and fix `files` and `links`.\n"
            ));
        }
        _ => {
            p.push_str(&format!(
                "Explore this codebase and write an architecture graph of it to `{GRAPH_FILE}` (create the `.workbench` directory if needed).\n\n"
            ));
        }
    }
    p.push_str(&format!(
        "\nThe graph is shown in Workbench as a node diagram the user edits to drive changes, so make it useful as a map:\n\
         - One node per meaningful unit: the app/entry point, each API or server, each route/endpoint/command, services, data models, stores/databases, external integrations, UI screens. Adapt to whatever kind of project this is.\n\
         - Aim for roughly 8-40 nodes in total, and keep every single network (the main one and each module) to about 25 nodes or fewer so the diagram stays readable. Group trivial helpers into the node that owns them instead of giving them their own node.\n\
         - Leave `pos` out on new nodes; Workbench lays them out.\n\
         - `links` point from caller to callee (request flows left to right).\n\
         - Add 2-6 `flows` covering the most important end-to-end paths (e.g. a request from route to database).\n\
         - `files` are repo-relative paths.\n\
         - Keep existing `modules` and their attachment nodes. When a self-contained feature has many nodes or is used from several places, or a network would grow past about 25 nodes, put the feature in a module.\n\
         - Keep existing `reference` nodes and their `reference` field exactly as they are; they point outside this project.\n\
         - Record `decisions` and `invariants` you can source from code comments, docs and commit messages (`git log`).\n\n\
         {KNOWLEDGE_GUIDE}\n\n\
         {MODULES_GUIDE}\n\n\
         {REFERENCES_GUIDE}\n\n\
         Use exactly this JSON shape:\n\n```json\n{SCHEMA}\n```\n\n\
         Write valid JSON only to that file. Do not modify any other file and do not touch `{BASELINE_FILE}`. \
         Finish with a short summary of the structure you mapped.\n"
    ));
    let rules = rules::relevant(rules, None);
    if !rules.is_empty() {
        p.push_str(&format!(
            "\nFor context, these are the project's rules (mention any obvious violations you notice, but don't fix them now):\n\n{}",
            rules::render(&rules, existing.unwrap_or(&Graph::default()))
        ));
    }
    p
}

/// Asks the agent to fill in the decisions and invariants of nodes from the project's history:
/// commits, comments, docs, tickets and past chats. `ids` are nodes of the `scope` network;
/// none means every node and module of the graph.
pub fn mine_decisions(root: &Graph, scope: Option<&str>, ids: &[String], transcripts: Option<&Path>) -> String {
    let target = if ids.is_empty() {
        "every node and module in the graph".to_string()
    } else {
        let net = root.net(scope).unwrap_or(root);
        let place = scope.map(|m| format!(" in module `{m}`")).unwrap_or_default();
        format!("these nodes{place}:\n{}", ids.iter().filter_map(|id| net.node(id)).map(|n| format!("- `{}` — {}\n  {}", n.id, n.title, files_line(n))).collect::<String>())
    };
    let chats = match transcripts {
        Some(dir) => format!(
            "- The past agent chats for this folder in `{}` (JSONL, one event per line). The user's own messages are the record of how they steered the app; \
             a decision from a chat has the source `chat <date of the message>`.\n",
            dir.display()
        ),
        None => String::new(),
    };
    format!(
        "Record in `{GRAPH_FILE}` how the app was steered: the decisions and invariants behind {target}\n\
         {KNOWLEDGE_GUIDE}\n\n\
         For each node, read:\n\
         - `git log --follow` on its files, the commit bodies above all.\n\
         - The comments in its files, and the tickets they name (PRO-123 style ids): read them through the Linear tools when you have them.\n\
         - `docs/`, the README and `{}`.\n\
         {chats}\n\
         Then:\n\
         - Append the `decisions` and `invariants` you can source to each node. Keep the existing ones and don't add duplicates.\n\
         - Where a node's `description` explains reasons or rules, or is wrong about the current code, rewrite it as 2-4 sentences on what the part is responsible for \
         and move the reasons into `decisions` and the rules into `invariants`. Description rewrites become proposals the user reviews.\n\
         - Change nothing else: no source code, no `files`, `links`, `pos` or flows, and never `{BASELINE_FILE}`.\n\n\
         Finish with a short list per node of what you recorded and the descriptions you proposed.\n",
        crate::ledger::LEDGER_FILE
    )
}

pub fn draft_nodes(graph: &Graph, scope: Option<&str>, request: &str, selection: Option<&str>, mode: ChatMode, rules: &[Rule]) -> String {
    let rules = rules::relevant(rules, None);
    let mut p = format!(
        "The user wants to extend the Workbench architecture graph in `{GRAPH_FILE}` from a description. \
         This is a design step: add nodes that describe behaviour that should exist. Do NOT change any source code; the user reviews the new nodes and commits them into code later.\n\n\
         ## Request\n\n{}\n",
        quote(request)
    );
    if let Some(nodes) = selection {
        p.push_str(&format!(
            "## Selected nodes\n\nThe user selected these existing nodes; the request is about them, so build on them and link the new nodes to them where it fits.\n\n{nodes}"
        ));
    }
    p.push_str(&format!("The user is looking at {}. ", scope_label(graph, scope)));
    match scope {
        Some(id) => p.push_str(&format!(
            "Add the new nodes inside that module (`modules[]` entry with id `{id}`) unless the request clearly says otherwise.\n\n"
        )),
        None => p.push_str(
            "Add the new nodes to the main network, unless the request describes an isolated feature: then create a module for it and attach it where it is used.\n\n",
        ),
    }
    p.push_str(&format!("{MODULES_GUIDE}\n\n{REFERENCES_GUIDE}\n\n## Current graph\n\n"));
    p.push_str(&outline(graph));
    p.push('\n');
    p.push_str(&rules_section(&rules, graph));
    p.push_str(&mode_section(mode, "edit the graph"));
    p.push_str(&format!(
        "## How to work\n\n\
         1. Read `{GRAPH_FILE}` and enough of the code to understand where the new behaviour belongs.\n\
         2. Add one node per meaningful unit of the requested behaviour, using the same JSON shape as the existing nodes:\n\n```json\n{SCHEMA}\n```\n\n\
         3. Write each `description` as the intended behaviour in 2-5 plain sentences, specific enough to implement from. \
            Set `files` to the repo-relative paths where the code should live (new or existing). Leave `pos` out on new nodes; Workbench places them next to the nodes they link to.\n\
         4. Add `links` between the new nodes and to existing nodes they call. You may add links from existing nodes to the new ones when the request implies they call them; do not otherwise change existing nodes.\n\
         5. Add a flow if the request describes an end-to-end path.\n\
         6. Keep the file valid JSON, keep every existing node, module, `id` and `pos` intact, edit only `{GRAPH_FILE}` and never touch `{BASELINE_FILE}`.\n\
         7. Finish with one line per added node (`id` — title, and the module it is in) and the links you added.\n"
    ));
    p
}

/// Tells the agent which nodes and files other branches are also changing, so it keeps
/// its edits to them small and reports them. `overlaps` come from `claims::overlaps`.
fn claims_section(graph: &Graph, overlaps: &[BranchClaim]) -> String {
    if overlaps.is_empty() {
        return String::new();
    }
    let mut p = String::from(
        "## Work on other branches\n\n\
         Other git branches are changing some of the same nodes or files. Touch these only as much as the changes above need, \
         and list every one you edited in your final summary so the user can sequence the merge.\n\n",
    );
    for b in overlaps {
        let nodes: Vec<String> = b.claim.nodes.iter().chain(b.claim.dependents.iter()).map(|(scope, id)| {
            graph.net(scope.as_deref()).map(|n| n.title_of(id)).unwrap_or_else(|| id.clone())
        }).collect();
        p.push_str(&format!("- `{}` by {}", b.branch, b.authors.join(", ")));
        if !nodes.is_empty() {
            p.push_str(&format!("; nodes: {}", nodes.join(", ")));
        }
        if !b.claim.files.is_empty() {
            p.push_str(&format!("; files: {}", b.claim.files.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ")));
        }
        p.push('\n');
    }
    p.push('\n');
    p
}

pub fn process(graph: &Graph, orders: &[WorkOrder], rules: &[Rule], note: &str, mode: ChatMode, refs: Option<&Refs>, overlaps: &[BranchClaim]) -> String {
    let changes = &work_order::changes(orders);
    let ids: Vec<String> = changes.iter().map(|c| c.id.clone()).collect();
    let rules = rules::relevant(rules, Some(&ids));

    let mut p = format!(
        "The user changed the architecture graph of this project in Workbench (`{GRAPH_FILE}`). Each node describes what a part of the system does; \
         `links` are what it calls or depends on. Change the code so it matches the graph again.\n\n"
    );
    p.push_str(&rules_section(&rules, graph));
    if !graph.modules.is_empty() {
        p.push_str(&format!("{MODULES_GUIDE}\n\n"));
    }
    p.push_str(&format!("## Changes ({})\n\n", changes.len()));

    let empty = Graph::default();
    let mut used_refs: Vec<(Option<String>, String)> = Vec::new();
    for c in changes {
        let n = c.node();
        let net = graph.net(c.scope.as_deref()).unwrap_or(&empty);
        let place = match (&c.scope, c.subject) {
            (Some(m), Subject::Node) => format!(" in module `{m}` ({})", graph.module_title(m)),
            _ => String::new(),
        };
        if c.subject == Subject::Module {
            p.push_str(&module_change(graph, c));
            continue;
        }
        if c.kind != ChangeKind::Removed {
            for l in &n.links {
                let key = (c.scope.clone(), l.clone());
                if net.node(l).is_some_and(|x| x.reference.is_some()) && !used_refs.contains(&key) {
                    used_refs.push(key);
                }
            }
        }
        if n.is_reference() {
            p.push_str(&reference_change(net, c, &place, refs));
            continue;
        }
        match &c.kind {
            ChangeKind::Added => {
                p.push_str(&format!("### New node `{}` — {} ({}){place}\n", n.id, n.title, n.kind));
                p.push_str(&quote(&n.description));
                p.push_str(&knowledge(n));
                if let Some(m) = &n.module {
                    p.push_str(&format!(
                        "This node attaches module `{m}` ({}): wire this place up to use that module's interface.\n",
                        graph.module_title(m)
                    ));
                }
                p.push_str(&format!("Calls/depends on: {}\n", titles(net, &n.links)));
                let from: Vec<String> = net.incoming(&n.id).iter().map(|x| x.id.clone()).collect();
                p.push_str(&format!("Called by: {}\n", titles(net, &from)));
                p.push_str("Implement this as new code that fits the project's existing structure and conventions.\n\n");
            }
            ChangeKind::Removed => {
                p.push_str(&format!("### Removed node `{}` — {}{place}\n", n.id, n.title));
                p.push_str(&files_line(n));
                p.push_str("It used to do:\n");
                p.push_str(&quote(&n.description));
                p.push_str("Remove this functionality and every reference to it. Keep callers compiling and working without it.\n\n");
            }
            ChangeKind::Modified(fields) => {
                let before = c.before.as_ref().unwrap();
                let after = c.after.as_ref().unwrap();
                p.push_str(&format!("### Changed node `{}` — {} ({}){place}\n", after.id, after.title, after.kind));
                p.push_str(&files_line(after));
                p.push_str(&knowledge(after));
                for f in fields {
                    match f {
                        Field::Module => match &after.module {
                            Some(m) => p.push_str(&format!(
                                "Now attaches module `{m}` ({}): this place should use that module instead of its own implementation.\n",
                                graph.module_title(m)
                            )),
                            None => p.push_str("No longer attaches a module: this place should stop using it.\n"),
                        },
                        Field::Reference => p.push_str("Its external reference changed (see the reference section below).\n"),
                        Field::Title => p.push_str(&format!("Renamed: \"{}\" → \"{}\" (rename the code too where the name is visible).\n", before.title, after.title)),
                        Field::Kind => p.push_str(&format!("Kind: {} → {}\n", before.kind, after.kind)),
                        Field::Description => {
                            p.push_str("Description before:\n");
                            p.push_str(&quote(&before.description));
                            p.push_str("Description now:\n");
                            p.push_str(&quote(&after.description));
                        }
                        Field::Decisions => {
                            let (old, new) = (decision_lines(before), decision_lines(after));
                            let (added, removed) = added_removed(&old, &new);
                            for d in added {
                                p.push_str(&format!("New decision to follow: {d}\n"));
                            }
                            for d in removed {
                                p.push_str(&format!("Dropped decision (the code no longer has to follow it): {d}\n"));
                            }
                        }
                        Field::Invariants => {
                            let (added, removed) = added_removed(&before.invariants, &after.invariants);
                            for i in added {
                                p.push_str(&format!("New invariant the code must keep: {i}\n"));
                            }
                            for i in removed {
                                p.push_str(&format!("Dropped invariant: {i}\n"));
                            }
                        }
                        Field::Files => p.push_str(&format!(
                            "Files re-sliced: [{}] → [{}]. Move the code so exactly these files implement this node.\n",
                            before.files.join(", "),
                            after.files.join(", ")
                        )),
                        Field::Links => {
                            let added: Vec<String> = after.links.iter().filter(|l| !before.links.contains(l)).cloned().collect();
                            let removed: Vec<String> = before.links.iter().filter(|l| !after.links.contains(l)).cloned().collect();
                            if !added.is_empty() {
                                p.push_str(&format!("Should now call/depend on: {}\n", titles(net, &added)));
                            }
                            if !removed.is_empty() {
                                p.push_str(&format!("Should no longer call/depend on: {}\n", titles(net, &removed)));
                            }
                        }
                    }
                }
                p.push('\n');
            }
        }
    }

    let used: Vec<&Node> = used_refs
        .iter()
        .filter(|(scope, id)| !changes.iter().any(|c| c.subject == Subject::Node && c.scope == *scope && c.id == *id))
        .filter_map(|(scope, id)| graph.net(scope.as_deref()).and_then(|net| net.node(id)))
        .collect();
    if !used.is_empty() {
        p.push_str(&format!("## External references the changed nodes link to\n\n{REFERENCES_GUIDE}\n\n"));
        for n in used {
            if let Some(r) = &n.reference {
                p.push_str(&format!("### `{}` — {}\n", n.id, n.title));
                p.push_str(&reference_context(&n.title, r, refs));
                p.push('\n');
            }
        }
    }

    p.push_str(&work_orders_section(orders));
    p.push_str(&claims_section(graph, overlaps));

    if !note.trim().is_empty() {
        p.push_str(&format!("## Extra instructions from the user\n\n{}\n\n", note.trim()));
    }

    p.push_str(&mode_section(mode, "change any code"));
    p.push_str(&format!(
        "## How to work\n\n\
         1. For each change, find the code that implements the node (start from its files, search if they're missing or stale) and read enough of its neighbours to change it safely.\n\
         2. Make the smallest set of code changes that makes the code do what the graph now says. Don't refactor unrelated code.\n\
         3. If the project has an obvious build, typecheck or test command, run it and fix what you broke.\n\
         4. Update `{GRAPH_FILE}`: set `files` on every affected node to the repo-relative files that now implement it (nodes of a module live in its `modules[].nodes`). \
            Do not change any existing node's `title`, `kind`, `description`, `links`, `module`, `reference` or `pos`, or any module's `title` or `description`, and do not touch `{BASELINE_FILE}`.\n\
         5. If the work required a meaningful new unit that no node describes yet (an endpoint, route, command, service, screen, component, store, data model, job or external integration, not a small helper), \
            add a node for it in the same network as the node that uses it: `id`, `title`, `kind`, a `description` of 2-5 plain sentences, `files` and `links`, with `pos` left out. \
            You may add links from existing nodes to the new node; that is the only change allowed to existing nodes besides `files`.\n\
         6. If a change contradicts a rule, follow the rule and say so.\n\
         7. Finish with one short line per node describing what you changed, plus one line per node you added.\n"
    ));
    p
}

/// The changes as typed work orders with their declared blast radius, which Workbench checks
/// the run's changed files against.
fn work_orders_section(orders: &[WorkOrder]) -> String {
    let mut p = String::from(
        "## Work orders\n\n\
         The changes above are these work orders. Each may only change the files in its `blast_radius.files` \
         and the files of the nodes in `blast_radius.dependents`, plus files of nodes you add. Workbench checks the changed files against this after the run.\n\n",
    );
    let mut kinds: Vec<work_order::Kind> = Vec::new();
    for o in orders {
        if !kinds.contains(&o.kind) {
            kinds.push(o.kind);
        }
    }
    for k in kinds {
        let name = serde_json::to_string(&k).unwrap_or_default();
        p.push_str(&format!("- {name}: {}.\n", k.instruction()));
    }
    p.push_str("\n```json\n");
    for o in orders {
        p.push_str(&serde_json::to_string(o).unwrap_or_default());
        p.push('\n');
    }
    p.push_str("```\n\n");
    p
}

fn reference_change(net: &Graph, c: &Change, place: &str, refs: Option<&Refs>) -> String {
    let n = c.node();
    let callers: Vec<String> = net.incoming(&n.id).iter().map(|x| x.id.clone()).collect();
    let mut p = String::new();
    match &c.kind {
        ChangeKind::Added => {
            p.push_str(&format!("### New external reference `{}` — {}{place}\n", n.id, n.title));
            p.push_str(&quote(&n.description));
        }
        ChangeKind::Removed => {
            p.push_str(&format!("### Removed external reference `{}` — {}{place}\n", n.id, n.title));
            p.push_str(
                "There is no code for the reference itself. Only change code if something was written specifically to follow or integrate with it and should no longer do so.\n\n",
            );
            return p;
        }
        ChangeKind::Modified(fields) => {
            let labels = fields.iter().map(|f| f.label()).collect::<Vec<_>>().join(", ");
            p.push_str(&format!("### Changed external reference `{}` — {}{place} (changed: {labels})\n", n.id, n.title));
            if let (Some(before), Some(after)) = (c.before.as_ref(), c.after.as_ref()) {
                let src = |x: &Node| x.reference.as_ref().map(|r| r.source.trim().to_string()).unwrap_or_default();
                if src(before) != src(after) {
                    p.push_str(&format!("Source: `{}` → `{}`\n", src(before), src(after)));
                }
                if fields.contains(&Field::Description) {
                    p.push_str("Description now:\n");
                    p.push_str(&quote(&after.description));
                }
            }
        }
    }
    match &n.reference {
        Some(r) => p.push_str(&reference_context(&n.title, r, refs)),
        None => p.push_str("It has no source set yet.\n"),
    }
    p.push_str(&format!("Linked from: {}\n", titles(net, &callers)));
    p.push_str(
        "Do not create code for the reference itself. Use it as context: make the nodes that link to it consistent with what it describes, and only change code where that is now inconsistent.\n\n",
    );
    p
}

fn module_change(graph: &Graph, c: &Change) -> String {
    let n = c.node();
    let mut p = String::new();
    match &c.kind {
        ChangeKind::Added => {
            p.push_str(&format!("### New module `{}` — {}\n", n.id, n.title));
            p.push_str(&quote(&n.description));
            let uses = graph.module_uses(&n.id);
            let places: Vec<String> = uses
                .iter()
                .map(|(scope, id)| {
                    let title = graph.net(scope.as_deref()).map(|net| net.title_of(id)).unwrap_or_else(|| id.clone());
                    format!("`{id}` ({title}) in {}", scope_label(graph, scope.as_deref()))
                })
                .collect();
            p.push_str(&format!("Attached at: {}\n", if places.is_empty() { "nowhere yet".into() } else { places.join(", ") }));
            p.push_str(
                "Build it as a self-contained unit (its own source module or folder) with a small interface, so every attachment point uses the same code. Its nodes are listed below.\n\n",
            );
        }
        ChangeKind::Removed => {
            p.push_str(&format!("### Removed module `{}` — {}\n", n.id, n.title));
            p.push_str(&files_line(n));
            p.push_str("It used to do:\n");
            p.push_str(&quote(&n.description));
            p.push_str("Remove the module's code and every place that used it. Keep callers compiling and working without it.\n\n");
        }
        ChangeKind::Modified(fields) => {
            let before = c.before.as_ref().unwrap();
            p.push_str(&format!("### Changed module `{}` — {}\n", n.id, n.title));
            p.push_str(&files_line(n));
            for f in fields {
                match f {
                    Field::Title => p.push_str(&format!("Renamed: \"{}\" → \"{}\"\n", before.title, n.title)),
                    _ => {
                        p.push_str("Description before:\n");
                        p.push_str(&quote(&before.description));
                        p.push_str("Description now:\n");
                        p.push_str(&quote(&n.description));
                    }
                }
            }
            p.push('\n');
        }
    }
    p
}

pub fn debug(graph: &Graph, flow: &Flow, mode: DebugMode, symptom: &str, rules: &[Rule]) -> String {
    let rules = rules::relevant(rules, Some(&flow.nodes));
    let mut p = format!(
        "Debug the flow **{}** in this project. It is a path through the Workbench architecture graph (`{GRAPH_FILE}`), in order:\n\n",
        flow.name
    );
    for (i, id) in flow.nodes.iter().enumerate() {
        match graph.node(id) {
            Some(n) => {
                p.push_str(&format!("{}. `{}` — {} ({})\n", i + 1, n.id, n.title, n.kind));
                p.push_str(&format!("   {}", files_line(n)));
                for line in n.description.trim().lines() {
                    p.push_str(&format!("   > {line}\n"));
                }
            }
            None => p.push_str(&format!("{}. `{id}` (missing from graph)\n", i + 1)),
        }
        if let Some(next) = flow.nodes.get(i + 1)
            && !graph.linked(id, next)
        {
            p.push_str("   (not directly linked to the next step in the graph — find how control gets there)\n");
        }
    }
    p.push('\n');
    if !flow.note.trim().is_empty() {
        p.push_str(&format!("Notes on this flow: {}\n\n", flow.note.trim()));
    }
    if !symptom.trim().is_empty() {
        p.push_str(&format!(
            "## Observed problem\n\n{}\n\nFind the root cause along this path, explain it, and fix it with a minimal change.\n\n",
            symptom.trim()
        ));
    }
    p.push_str(&rules_section(&rules, graph));
    p.push_str("## Task\n\n");
    p.push_str(match mode {
        DebugMode::Trace => {
            "Trace the flow through the actual code step by step: for each step name the concrete entry function/symbol, what data it receives and what it hands to the next step. \
             Verify the path by running it if there is a safe way (existing tests, a CLI invocation, a local dev server, a REPL). \
             Point out every place where the code diverges from the node descriptions, or where errors are swallowed, inputs unvalidated, or the handoff between steps is fragile.\n"
        }
        DebugMode::Instrument => {
            "Add temporary debug logging at the boundary of every step (entry, key decisions, exit, errors), each line prefixed with `[wb-debug <node-id>]` and using the project's existing logging facility if there is one. \
             Then exercise the flow (tests, CLI, or a local server) and capture the output. Report what the logs show step by step and where behaviour deviates from the descriptions. \
             Finally remove every `[wb-debug` line you added unless you were asked to keep them.\n"
        }
        DebugMode::Test => {
            "Write an automated test using the project's existing test framework and conventions that exercises this flow end to end, from the first step to the last, asserting the behaviour each node's description promises. \
             Mock only true external boundaries. Run it. If it fails because of a real bug relative to the descriptions, fix the bug; otherwise fix the test.\n"
        }
    });
    p.push_str("\nFinish with a short step-by-step report of the flow and anything you changed.\n");
    p
}

pub fn audit(graph: &Graph, rules: &[Rule]) -> String {
    let rules = rules::relevant(rules, None);
    format!(
        "Audit this codebase against the project rules below. For each rule, list concrete violations as `path:line` with a one-line explanation, \
         or say it is satisfied. Use the architecture graph in `{GRAPH_FILE}` to find the code for scoped rules. Do not change any files.\n\n{}",
        rules_section(&rules, graph)
    )
}

pub fn node_question(root: &Graph, scope: Option<&str>, n: &Node, refs: Option<&Refs>) -> String {
    let net = root.net(scope).unwrap_or(root);
    let place = scope.map(|m| format!(" in module `{m}` ({})", root.module_title(m))).unwrap_or_default();
    let module = n
        .module
        .as_deref()
        .map(|m| format!("Attaches module `{m}` ({}).\n", root.module_title(m)))
        .unwrap_or_default();
    let files = match &n.reference {
        Some(r) => reference_context(&n.title, r, refs),
        None => files_line(n),
    };
    let linked: String = n
        .links
        .iter()
        .filter_map(|l| net.node(l))
        .filter_map(|x| x.reference.as_ref().map(|r| (x, r)))
        .map(|(x, r)| format!("It links to an external reference. {}", reference_context(&x.title, r, refs)))
        .collect();
    format!(
        "About graph node `{}` — {} ({}){place}:\n{}{}{module}{files}Calls: {}\n{linked}\n",
        n.id,
        n.title,
        n.kind,
        quote(&n.description),
        knowledge(n),
        titles(net, &n.links)
    )
}

pub fn selected_nodes(root: &Graph, scope: Option<&str>, ids: &[String], refs: Option<&Refs>) -> Option<String> {
    let net = root.net(scope)?;
    let out: String = ids.iter().filter_map(|id| net.node(id)).map(|n| node_question(root, scope, n, refs)).collect();
    if out.is_empty() { None } else { Some(out) }
}

pub fn ghost_nodes(picks: &[(&str, &Graph, Option<&str>, &Node, String)]) -> Option<String> {
    if picks.is_empty() {
        return None;
    }
    let mut p = String::from(
        "## Work on other branches\n\nThe user also picked these nodes from the Workbench graph on other git branches. They are not in the working tree: they show work in progress on those branches. \
         Read their code with `git show BRANCH:PATH` or `git diff HEAD...BRANCH -- PATH`. Do not check out, merge, push or change those branches unless the user asks.\n\n",
    );
    for (branch, root, scope, node, change) in picks {
        p.push_str(&format!("From branch `{branch}` ({change}):\n"));
        p.push_str(&node_question(root, *scope, node, None));
    }
    Some(p)
}

pub fn with_selection(nodes: Option<&str>, ghosts: Option<&str>, text: &str) -> String {
    if nodes.is_none() && ghosts.is_none() {
        return text.to_string();
    }
    let mut p = String::new();
    if let Some(nodes) = nodes {
        p.push_str(&format!(
            "The user has selected these nodes in the Workbench architecture graph (`{GRAPH_FILE}`). Their message is probably about them. Before you act, check that a selected node fits the message from its description and `files`. If none fits, step out one linked node at a time and check each, before searching anywhere else.\n\n{nodes}"
        ));
    }
    if let Some(ghosts) = ghosts {
        p.push_str(ghosts);
    }
    p.push_str(&format!("## Message\n\n{text}"));
    p
}

pub fn pr_review(repo: Option<&str>, number: u64, title: &str) -> String {
    let flag = repo.map(|r| format!(" -R {r}")).unwrap_or_default();
    let which = repo.map(|r| format!("in `{r}`")).unwrap_or_else(|| "in this repository".into());
    format!(
        "Review pull request #{number} “{title}” {which}.\n\n\
         Read it with the GitHub CLI: `gh pr view {number}{flag} --comments` and `gh pr diff {number}{flag}`. \
         Use the architecture graph in `{GRAPH_FILE}` and the project rules to judge whether the change fits the design.\n\n\
         Report: a one-paragraph summary, then concrete problems as `path:line` with an explanation (bugs first, then design and rule violations, then nits), \
         and finish with a recommendation (approve, request changes or comment). \
         Do NOT post anything to GitHub, do not check out the branch and do not change any files.\n"
    )
}

pub fn system_append(graph: Option<&Graph>, rules: &[Rule], related: &[String], refs: Option<&Refs>) -> Option<String> {
    let relevant = rules::relevant(rules, None);
    let mut out = String::new();
    if graph.is_some_and(|g| !g.nodes.is_empty()) {
        out.push_str(&format!(
            "This project has an architecture graph maintained by the Workbench app in `{GRAPH_FILE}`. Keep it in step with every code change you make, \
             even when the user doesn't ask for it:\n\
             - When you move, add or remove code that a node describes, keep that node's `files` (repo-relative paths) accurate.\n\
             - When you add a meaningful new unit (an endpoint, route, command, service, screen, component, store, data model, job or external integration), \
             add a node for it in the same shape as the existing nodes: `id`, `title`, `kind`, a `description` of 2-5 plain sentences on what it does, `files` and `links` \
             (ids of nodes it calls or depends on). Leave `pos` out; Workbench places it. Add a link from each existing node that now calls it.\n\
             - When you remove such a unit, remove its node and every link and flow step that points at it. When you change what a node calls, update its `links`.\n\
             - Descriptions belong to the user: when you change what an existing node does, you may write its new `description`, but it only becomes a proposal \
             the user accepts or rejects; say in your summary which descriptions you proposed.\n\
             - Group small helpers into the node that owns them instead of giving them their own node. Put a new isolated feature with several parts in a module.\n\
             - When you change a node's logic or add a node, append to its `decisions` every non-obvious choice you made or the user asked for, with the `why` \
             (the user's reason or the constraint that forced it), `instead_of` when you rejected an alternative, a `source` (`chat <today's date>` for this conversation, \
             or the commit or ticket) and the `date`. When the user states a rule that must always hold for a part, add it to that node's `invariants`. \
             Appending is applied right away; rewriting or removing an existing decision or invariant only becomes a proposal, like a description.\n\
             - Follow the `decisions` and `invariants` of the nodes you work on. If a change has to break one, say so and why instead of doing it silently.\n\
             - {naming}\n\
             - Keep existing `id`s and `pos` values, keep the file valid JSON, and list the graph changes you made, including the decisions you recorded, in your final summary.\n\
             Never edit `{BASELINE_FILE}`.\n\n",
            naming = crate::naming::prompt_rule()
        ));
        out.push_str(&format!("{KNOWLEDGE_GUIDE}\n\n{MODULES_GUIDE}\n\n"));
    }
    if let Some(g) = graph {
        out.push_str(&references_section(g, refs));
    }
    if !related.is_empty() {
        out.push_str(&format!(
            "Related GitHub repositories the user has linked for context: {}. \
             When you need context from them, read them with the GitHub CLI (`gh repo view OWNER/REPO`, `gh api repos/OWNER/REPO/contents/PATH`, `gh search code --repo OWNER/REPO QUERY`); never push to them.\n\n",
            related.iter().map(|r| format!("`{r}`")).collect::<Vec<_>>().join(", ")
        ));
    }
    if !relevant.is_empty() {
        out.push_str("The user has defined these project rules in Workbench. Follow them in all work on this project:\n\n");
        out.push_str(&rules::render(&relevant, graph.unwrap_or(&Graph::default())));
    }
    if out.is_empty() { None } else { Some(out) }
}

pub fn project_section(project: &Project, cwd: &Path) -> Option<String> {
    let others: Vec<_> = project.repos.iter().filter(|r| !r.is_at(cwd)).collect();
    let here = project.repos.iter().find(|r| r.is_at(cwd))?;
    let mut out = format!("This repository (`{}`) is part of the project “{}”", here.label(), project.name.trim());
    if !project.description.trim().is_empty() {
        out.push_str(&format!(": {}", project.description.trim()));
    }
    out.push_str(".\n");
    if !here.description.trim().is_empty() {
        out.push_str(&format!("Its role in the project: {}\n", here.description.trim()));
    }
    if others.is_empty() {
        out.push('\n');
        return Some(out);
    }
    out.push_str(&format!(
        "The other repositories in the project are local clones you can read with your file tools. Each may have its own architecture graph at `{GRAPH_FILE}` in its folder; read that first for an overview. \
         Read them when a change here depends on them (shared APIs, contracts, deployment), and only edit them when the user asks you to:\n"
    ));
    for r in others {
        let gh = r.github.as_deref().map(|g| format!(" (GitHub `{g}`)")).unwrap_or_default();
        let desc = if r.description.trim().is_empty() { String::new() } else { format!(": {}", r.description.trim()) };
        out.push_str(&format!("- `{}` at `{}`{gh}{desc}\n", r.label(), r.path));
    }
    out.push('\n');
    Some(out)
}

pub fn with_project(base: Option<String>, project: Option<&Project>, cwd: &Path) -> Option<String> {
    let section = project.and_then(|p| project_section(p, cwd));
    match (section, base) {
        (Some(s), Some(b)) => Some(format!("{s}{b}")),
        (s, b) => s.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::diff;
    use crate::projects::ProjectRepo;

    #[test]
    fn project_section_lists_the_other_repositories() {
        let repo = |path: &str, desc: &str, gh: Option<&str>| ProjectRepo {
            path: path.into(),
            description: desc.into(),
            github: gh.map(String::from),
            ..Default::default()
        };
        let p = Project {
            id: "wb".into(),
            name: "Workbench".into(),
            description: "Codebase shaping tool".into(),
            repos: vec![
                repo("/nonexistent/wb/frontend", "Desktop UI", None),
                repo("/nonexistent/wb/backend", "Sync API", Some("codebrain/wb-backend")),
            ],
        };
        let s = project_section(&p, Path::new("/nonexistent/wb/frontend")).unwrap();
        assert!(s.contains("“Workbench”: Codebase shaping tool"));
        assert!(s.contains("Its role in the project: Desktop UI"));
        assert!(s.contains("`backend` at `/nonexistent/wb/backend` (GitHub `codebrain/wb-backend`): Sync API"));
        assert!(!s.contains("- `frontend`"));
        assert!(project_section(&p, Path::new("/elsewhere")).is_none());
        assert_eq!(with_project(Some("rest".into()), Some(&p), Path::new("/elsewhere")).as_deref(), Some("rest"));
        assert!(with_project(None, Some(&p), Path::new("/nonexistent/wb/backend")).unwrap().contains("`frontend`"));
    }

    fn g() -> Graph {
        serde_json::from_str(
            r#"{"nodes":[
                {"id":"api","title":"API","kind":"api","description":"HTTP server","links":["get-users"]},
                {"id":"get-users","title":"GET /users","kind":"route","description":"Lists users","files":["src/users.rs"],"links":["db"]},
                {"id":"db","title":"Postgres","kind":"db","description":"Stores users"}
            ],"flows":[{"id":"list","name":"List users","nodes":["api","get-users","db"]}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn process_prompt_mentions_each_change_and_rules() {
        let base = g();
        let mut cur = base.clone();
        cur.node_mut("get-users").unwrap().description = "Lists users with pagination".into();
        cur.remove_node("db");
        let rules = vec![Rule { id: "r1".into(), title: "No unwrap".into(), body: String::new(), enabled: true, scope: None }];
        let p = process(&cur, &work_order::from_changes(&base, &cur, &diff(&base, &cur), &[]), &rules, "be careful", ChatMode::Auto, None, &[]);
        assert!(p.contains("Lists users with pagination"));
        assert!(p.contains("Removed node `db`"));
        assert!(p.contains("No unwrap"));
        assert!(p.contains("be careful"));
        assert!(p.contains("`src/users.rs`"));
        assert!(p.contains("Mode: Auto"));
        assert!(p.contains("You may add links from existing nodes to the new node"));
        assert!(!p.contains(QUESTIONS_MARKER));
    }

    #[test]
    fn draft_prompt_includes_request_nodes_and_plan_marker() {
        let p = draft_nodes(&g(), None, "Add a POST /users route", None, ChatMode::Plan, &[]);
        assert!(p.contains("> Add a POST /users route"));
        assert!(p.contains("`get-users` — GET /users (route)"));
        assert!(p.contains(QUESTIONS_MARKER));
        assert!(p.contains("Do NOT change any source code"));
    }

    #[test]
    fn spawn_blocks_are_parsed_and_removed() {
        let reply = format!(
            "Starting two.\n{SPAWN_OPEN}name=\"Tests\"]]\nWrite tests\n{SPAWN_CLOSE}\n{SPAWN_OPEN}name=\"Api\" repo=\"backend\" provider=\"vibe\" model=\"mistral-large-4\"]]Add a route{SPAWN_CLOSE}"
        );
        let (shown, spawns) = spawn_requests(&reply);
        assert_eq!(shown, "Starting two.");
        assert_eq!(
            spawns,
            vec![
                SpawnRequest { name: "Tests".into(), repo: None, provider: None, model: None, task: "Write tests".into() },
                SpawnRequest {
                    name: "Api".into(),
                    repo: Some("backend".into()),
                    provider: Some("vibe".into()),
                    model: Some("mistral-large-4".into()),
                    task: "Add a route".into(),
                },
            ]
        );
        assert_eq!(spawn_requests("no blocks"), ("no blocks".into(), vec![]));
    }

    #[test]
    fn focus_blocks_are_parsed_and_removed() {
        let reply = format!(
            "The scope check.\n{FOCUS_OPEN}node=\"scope\" confidence=\"high\"]]\n{FOCUS_OPEN}node=\"login\" module=\"auth\" confidence=\"low\"]]"
        );
        let (shown, focus) = focus_requests(&reply);
        assert_eq!(shown, "The scope check.");
        assert_eq!(
            focus,
            vec![
                FocusRequest { module: None, node: "scope".into(), confidence: "high".into() },
                FocusRequest { module: Some("auth".into()), node: "login".into(), confidence: "low".into() },
            ]
        );
    }

    #[test]
    fn spawn_instructions_name_the_project_repositories() {
        let p = Project {
            name: "Workbench".into(),
            repos: vec![ProjectRepo { path: "/code/backend".into(), ..Default::default() }],
            ..Default::default()
        };
        let settings = Settings::default();
        assert!(spawn_agent_instructions(Some(&p), &settings).contains("repo=\"<repository>\""));
        assert!(spawn_agent_instructions(Some(&p), &settings).contains("`backend`"));
        assert!(!spawn_agent_instructions(None, &settings).contains("repo=\"<repository>\""));
        // Every agent learns the providers it can start an agent on, with their models.
        assert!(spawn_agent_instructions(None, &settings).contains("provider=\"<provider id>\""));
        assert!(spawn_agent_instructions(None, &settings).contains("- `vibe` (Mistral Vibe (CLI)): mistral-medium-3.5, local"));
        assert!(project_agent_prompt(&p, Path::new("/code")).contains("- `backend` at `/code/backend`"));
    }

    #[test]
    fn marker_detection_and_stripping() {
        let reply = format!("1. Which db?\n{QUESTIONS_MARKER}\n");
        assert!(asks_questions(&reply));
        assert_eq!(strip_marker(&reply), "1. Which db?");
        assert!(!asks_questions("all done"));
    }

    #[test]
    fn debug_prompt_lists_steps_in_order() {
        let graph = g();
        let p = debug(&graph, &graph.flows[0], DebugMode::Test, "500 on page 2", &[]);
        let a = p.find("1. `api`").unwrap();
        let b = p.find("2. `get-users`").unwrap();
        assert!(a < b);
        assert!(p.contains("500 on page 2"));
        assert!(!p.contains("not directly linked"));
    }

    #[test]
    fn system_append_empty_without_graph_or_rules() {
        assert!(system_append(None, &[], &[], None).is_none());
        let sys = system_append(Some(&g()), &[], &[], None).unwrap();
        assert!(sys.contains(GRAPH_FILE));
        assert!(sys.contains("add a node for it"));
        assert!(sys.contains("Modules:"));
        let related = vec!["acme/shared".to_string()];
        assert!(system_append(None, &[], &related, None).unwrap().contains("`acme/shared`"));
    }

    fn with_reference() -> Graph {
        let mut graph = g();
        graph.nodes.push(
            serde_json::from_str(
                r#"{"id":"auth-ref","title":"Shared auth","kind":"reference","description":"Our auth library",
                    "reference":{"source":"https://github.com/acme/shared/tree/main/auth","nodes":[{"id":"token","module":"core"},{"id":"login"}]}}"#,
            )
            .unwrap(),
        );
        graph
    }

    fn external() -> crate::reference::RefGraphs {
        let ext: Graph = serde_json::from_str(
            r#"{"nodes":[{"id":"login","title":"Login route","kind":"route","description":"Checks the password"}],
                "modules":[{"id":"core","title":"Core","nodes":[{"id":"token","title":"Token signer","kind":"service","description":"Signs JWTs with RS256","files":["src/token.rs"]}]}]}"#,
        )
        .unwrap();
        let mut graphs = crate::reference::RefGraphs::new();
        graphs.insert(
            "https://github.com/acme/shared/tree/main/auth".into(),
            crate::reference::RefState::Ready(Some(crate::reference::RefGraph {
                root: "acme/shared@main".into(),
                location: "github.com/acme/shared/.workbench/graph.json@main".into(),
                graph: ext,
            })),
        );
        graphs
    }

    #[test]
    fn references_inline_external_nodes_into_prompts() {
        let graph = with_reference();
        let graphs = external();
        let cwd = std::path::PathBuf::from("/tmp");
        let refs = Refs { cwd: &cwd, graphs: &graphs };
        let sys = system_append(Some(&graph), &[], &[], Some(&refs)).unwrap();
        assert!(sys.contains("## External references"));
        assert!(sys.contains("GitHub repository"));
        assert!(sys.contains("gh api repos/acme/shared/contents/auth?ref=main"));
        assert!(sys.contains("`token` — Token signer (service) in module `core`"));
        assert!(sys.contains("> Signs JWTs with RS256"));
        assert!(sys.contains("relative to `acme/shared@main`"));

        let bare = system_append(Some(&graph), &[], &[], None).unwrap();
        assert!(bare.contains("- `login` (look it up in the referenced project's"));

        let mut base = graph.clone();
        base.remove_node("auth-ref");
        let mut cur = graph.clone();
        cur.node_mut("get-users").unwrap().links.push("auth-ref".into());
        let p = process(&cur, &work_order::from_changes(&base, &cur, &diff(&base, &cur), &[]), &[], "", ChatMode::Auto, Some(&refs), &[]);
        assert!(p.contains("### New external reference `auth-ref` — Shared auth"));
        assert!(p.contains("Do not create code for the reference itself"));
        assert!(p.contains("Login route"));

        let sel = selected_nodes(&cur, None, &["get-users".into()], Some(&refs)).unwrap();
        assert!(sel.contains("It links to an external reference"));
        assert!(sel.contains("Signs JWTs"));
    }

    fn with_github_module() -> Graph {
        let mut graph = g();
        graph.modules.push(serde_json::from_str(
            r#"{"id":"github","title":"GitHub","description":"Reads and merges PRs",
                "nodes":[{"id":"gh-service","title":"gh service","kind":"service","description":"Wraps gh"}]}"#,
        )
        .unwrap());
        graph
    }

    #[test]
    fn process_prompt_describes_new_modules_and_attachments() {
        let base = g();
        let mut cur = with_github_module();
        cur.nodes.push(serde_json::from_str(r#"{"id":"gh","title":"GitHub","kind":"module","module":"github"}"#).unwrap());
        cur.node_mut("api").unwrap().links.push("gh".into());
        let p = process(&cur, &work_order::from_changes(&base, &cur, &diff(&base, &cur), &[]), &[], "", ChatMode::Auto, None, &[]);
        assert!(p.contains("### New module `github` — GitHub"));
        assert!(p.contains("Attached at: `gh` (GitHub) in the main network"));
        assert!(p.contains("New node `gh-service` — gh service (service) in module `github`"));
        assert!(p.contains("attaches module `github`"));
    }

    #[test]
    fn draft_prompt_targets_the_open_module() {
        let p = draft_nodes(&with_github_module(), Some("github"), "Add merge", None, ChatMode::Auto, &[]);
        assert!(p.contains("inside that module (`modules[]` entry with id `github`)"));
        assert!(p.contains("Module `github` — GitHub"));
        assert!(p.contains("`gh-service` — gh service (service)"));
        assert!(!p.contains("## Selected nodes"));
    }

    #[test]
    fn agents_see_and_keep_decisions_and_invariants() {
        let mut graph = g();
        let db = graph.node_mut("db").unwrap();
        db.decisions.push(Decision { decision: "Use Postgres".into(), why: "we need transactions".into(), source: "commit abc123".into(), ..Default::default() });
        db.invariants.push("Never drops rows".into());
        let q = node_question(&graph, None, graph.node("db").unwrap(), None);
        assert!(q.contains("Use Postgres — because we need transactions [commit abc123]"));
        assert!(q.contains("Invariants (must stay true):\n- Never drops rows"));
        let sys = system_append(Some(&graph), &[], &[], None).unwrap();
        assert!(sys.contains("append to its `decisions`") && sys.contains(KNOWLEDGE_GUIDE));
        let mine = mine_decisions(&graph, None, &["db".into()], Some(Path::new("/tmp/chats")));
        assert!(mine.contains("`db` — Postgres") && mine.contains("/tmp/chats") && mine.contains("git log --follow"));
    }

    #[test]
    fn selection_context_lists_selected_nodes_only() {
        let graph = g();
        assert!(selected_nodes(&graph, None, &[], None).is_none());
        assert!(selected_nodes(&graph, None, &["missing".into()], None).is_none());
        let nodes = selected_nodes(&graph, None, &["get-users".into(), "db".into()], None).unwrap();
        assert!(nodes.contains("`get-users`"));
        assert!(nodes.contains("`db`"));
        assert!(!nodes.contains("`api` —"));
        let msg = with_selection(Some(&nodes), None, "why is this slow?");
        assert!(msg.contains("## Message\n\nwhy is this slow?"));
        assert_eq!(with_selection(None, None, "hello"), "hello");
        let db = graph.node("db").unwrap();
        let ghosts = ghost_nodes(&[("feature/cache", &graph, None, db, "added on that branch".into())]).unwrap();
        assert!(ghosts.contains("From branch `feature/cache` (added on that branch)"));
        let msg = with_selection(None, Some(&ghosts), "hi");
        assert!(!msg.contains("has selected these nodes"));
        assert!(msg.contains("## Work on other branches") && msg.ends_with("## Message\n\nhi"));
        let p = draft_nodes(&graph, None, "Add caching", Some(&nodes), ChatMode::Auto, &[]);
        assert!(p.contains("## Selected nodes"));
        assert!(p.contains("About graph node `db`"));
    }
}
