//! `workbench query`: the intent graph for agents and scripts, as JSON on stdout. Agents use it
//! to find where work belongs (which node owns a file, what a node links to) before they search
//! the file system. The app binary runs it instead of opening a window when its first argument
//! is `query`:
//!
//! ```text
//! <binary> query <dir> node <id> [--module <m>]     the node, the nodes it links to and the nodes linking to it
//! <binary> query <dir> file <path>                  the nodes whose `files` cover the path
//! <binary> query <dir> history <id> [--module <m>]  the node's recorded changes, oldest first
//! <binary> query <dir> modules                      every module and where it is attached
//! <binary> query <dir> overview                     one line per node: `module/id: title (kind)`
//! <binary> query <dir> search <words...>            the nodes whose id, title or description match best
//! ```
//!
//! `<dir>` is the folder holding `.workbench/graph.json`. Nodes are printed as in `graph.json`
//! without `pos`. `--module` names the module whose network the node is in; leave it out for the
//! main network. Errors go to stderr with exit code 1.

use std::path::Path;

use serde_json::{Value, json};

use crate::graph::{self, GRAPH_FILE, Graph, Node};
use crate::node_history;

/// Runs the query in `args` (the words after `query`) and returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    match answer(args) {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("workbench query: {e}");
            1
        }
    }
}

/// How agents are told to call it; see `prompts`.
pub fn command_line(cwd: &Path) -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "workbench".into());
    format!("\"{exe}\" query \"{}\"", cwd.display())
}

fn answer(args: &[String]) -> Result<Value, String> {
    let usage = "usage: query <dir> (node <id> | file <path> | history <id>) [--module <m>] | query <dir> (modules | overview | search <words...>)";
    let [dir, what, rest @ ..] = args else { return Err(usage.into()) };
    let dir = Path::new(dir);
    let g = graph::load(dir, GRAPH_FILE)?.ok_or_else(|| format!("no {GRAPH_FILE} in {}", dir.display()))?;
    let module = match rest {
        [_, flag, m] if flag == "--module" => Some(m.as_str()),
        _ => None,
    };
    match (what.as_str(), rest.first()) {
        ("node", Some(id)) => node(&g, module, id),
        ("file", Some(path)) => Ok(file(&g, path)),
        ("history", Some(id)) => history(dir, module, id),
        ("modules", None) => Ok(modules(&g)),
        ("overview", None) => Ok(overview(&g)),
        ("search", Some(_)) => Ok(search(&g, &rest.join(" "))),
        _ => Err(usage.into()),
    }
}

fn shown(n: &Node) -> Value {
    let mut n = n.clone();
    n.pos = None;
    serde_json::to_value(n).unwrap_or(Value::Null)
}

fn brief(n: &Node) -> Value {
    json!({ "id": n.id, "title": n.title, "kind": n.kind })
}

fn node(g: &Graph, module: Option<&str>, id: &str) -> Result<Value, String> {
    let net = g.net(module).ok_or_else(|| format!("no module `{}`", module.unwrap_or_default()))?;
    let n = net.node(id).ok_or_else(|| format!("no node `{id}` in {}", module.map(|m| format!("module `{m}`")).unwrap_or("the main network".into())))?;
    let links_to: Vec<Value> = n.links.iter().filter_map(|l| net.node(l)).map(brief).collect();
    let linked_from: Vec<Value> = net.incoming(id).into_iter().map(brief).collect();
    Ok(json!({ "module": module, "node": shown(n), "links_to": links_to, "linked_from": linked_from }))
}

/// The nodes in any network whose `files` list the path, or a folder that holds it. This is the
/// file → node lookup over `graph.json` (PRO-240): built per query, so it never goes stale.
fn file(g: &Graph, path: &str) -> Value {
    let path = path.trim_start_matches("./");
    let covers = |f: &str| {
        let f = f.trim_start_matches("./").trim_end_matches('/');
        path == f || path.starts_with(&format!("{f}/"))
    };
    let owners: Vec<Value> = g
        .all_nodes()
        .into_iter()
        .filter(|(_, n)| n.files.iter().any(|f| covers(f)))
        .map(|(module, n)| json!({ "module": module, "id": n.id, "title": n.title, "kind": n.kind }))
        .collect();
    json!({ "path": path, "nodes": owners })
}

fn history(dir: &Path, module: Option<&str>, id: &str) -> Result<Value, String> {
    let entries: Vec<node_history::Entry> =
        node_history::load(dir)?.into_iter().filter(|e| e.id == id && e.scope.as_deref() == module && !e.module).collect();
    Ok(json!({ "module": module, "id": id, "changes": entries }))
}

fn modules(g: &Graph) -> Value {
    let list: Vec<Value> = g
        .modules
        .iter()
        .map(|m| {
            let attached: Vec<Value> = g.module_uses(&m.id).into_iter().map(|(scope, node)| json!({ "module": scope, "node": node })).collect();
            json!({ "id": m.id, "title": m.title, "description": m.description, "attached_at": attached })
        })
        .collect();
    json!({ "modules": list })
}

/// Every node as one short line, without descriptions: the cheap map an agent reads to pick
/// nodes by name when none are selected. Nodes in a module are written `module/id`.
fn overview(g: &Graph) -> Value {
    let lines: Vec<String> = g
        .all_nodes()
        .into_iter()
        .map(|(module, n)| {
            let id = match module {
                Some(m) => format!("{m}/{}", n.id),
                None => n.id.clone(),
            };
            format!("{id}: {} ({})", n.title, n.kind)
        })
        .collect();
    json!({ "nodes": lines })
}

/// How many nodes `search` returns.
const SEARCH_LIMIT: usize = 10;

/// Question words that match almost every node, so `search` leaves them out.
const IGNORED_WORDS: [&str; 20] = [
    "the", "and", "where", "which", "what", "does", "are", "for", "with", "that", "its", "from", "into", "file", "files", "code", "set", "our",
    "this", "how",
];

/// The nodes that match the most words of `text`, best first. A word in the title counts 3, in
/// the id 2 and in the description 1. Words shorter than 3 letters and `IGNORED_WORDS` are left
/// out, so question words never match everything.
fn search(g: &Graph, text: &str) -> Value {
    let words: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !IGNORED_WORDS.contains(w))
        // "links" should match a node titled "Link routing".
        .map(|w| w.strip_suffix('s').filter(|stem| stem.len() >= 3).unwrap_or(w).to_string())
        .collect();
    let mut scored: Vec<(usize, Option<&str>, &Node)> = g
        .all_nodes()
        .into_iter()
        .map(|(module, n)| {
            let (title, id, description) = (n.title.to_lowercase(), n.id.to_lowercase(), n.description.to_lowercase());
            let score: usize = words
                .iter()
                .map(|w| 3 * usize::from(title.contains(w.as_str())) + 2 * usize::from(id.contains(w.as_str())) + usize::from(description.contains(w.as_str())))
                .sum();
            (score, module, n)
        })
        .filter(|(score, _, _)| *score > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let hits: Vec<Value> = scored
        .into_iter()
        .take(SEARCH_LIMIT)
        .map(|(score, module, n)| json!({ "module": module, "id": n.id, "title": n.title, "kind": n.kind, "files": n.files, "score": score }))
        .collect();
    json!({ "query": text, "nodes": hits })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> Graph {
        let api = Node { id: "api".into(), title: "API".into(), files: vec!["src/api".into()], links: vec!["db".into()], ..Default::default() };
        let db = Node { id: "db".into(), title: "DB".into(), files: vec!["src/db.rs".into()], ..Default::default() };
        Graph { nodes: vec![api, db], ..Default::default() }
    }

    #[test]
    fn a_file_finds_the_nodes_that_own_it() {
        let ids = |v: Value| v["nodes"].as_array().unwrap().iter().map(|n| n["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(ids(file(&graph(), "src/db.rs")), vec!["db"]);
        assert_eq!(ids(file(&graph(), "./src/api/routes.rs")), vec!["api"]);
        assert!(ids(file(&graph(), "src/apiary.rs")).is_empty());
    }

    #[test]
    fn search_ranks_title_matches_first_and_skips_short_words() {
        let v = search(&graph(), "is the api");
        assert_eq!(v["nodes"][0]["id"], "api");
        assert_eq!(v["nodes"].as_array().unwrap().len(), 1);
        assert!(search(&graph(), "nothing here").get("nodes").unwrap().as_array().unwrap().is_empty());
    }

    #[test]
    fn the_overview_lists_every_node_on_one_line() {
        assert_eq!(overview(&graph())["nodes"], json!(["api: API ()", "db: DB ()"]));
    }

    #[test]
    fn a_node_comes_with_its_neighbours() {
        let v = node(&graph(), None, "db").unwrap();
        assert_eq!(v["linked_from"][0]["id"], "api");
        assert!(v["node"].get("pos").is_none());
        assert!(node(&graph(), None, "nope").is_err());
    }

    /// The offline half of the traversal evaluation (scripts/eval_traversal.py): for each
    /// question, keyword search over this repository's own graph must name a node owning one of
    /// the expected files in its top 3 for at least 6 of the 10 questions. Keyword search can't
/// match synonyms ("newer release" vs "Self-updater"); the traversal model reading node names
/// covers those, which the live script measures.
    #[test]
    fn search_finds_the_expected_files_for_the_evaluation_questions() {
        let questions: Vec<Value> = serde_json::from_str(include_str!("../scripts/traversal_questions.json")).unwrap();
        let g = graph::load(Path::new(env!("CARGO_MANIFEST_DIR")), GRAPH_FILE).unwrap().unwrap();
        let misses: Vec<&str> = questions
            .iter()
            .filter(|q| {
                let hits = search(&g, q["question"].as_str().unwrap());
                let top_files: Vec<&Value> = hits["nodes"].as_array().unwrap().iter().take(3).flat_map(|n| n["files"].as_array().unwrap()).collect();
                !q["expect"].as_array().unwrap().iter().any(|f| top_files.contains(&f))
            })
            .map(|q| q["question"].as_str().unwrap())
            .collect();
        println!("search missed {} of {}: {misses:#?}", misses.len(), questions.len());
        assert!(misses.len() * 10 <= questions.len() * 4, "missed more than 40%: {misses:#?}");
    }
}
