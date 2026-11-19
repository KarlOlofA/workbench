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
    let usage = "usage: query <dir> (node <id> | file <path> | history <id>) [--module <m>] | query <dir> modules";
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
    fn a_node_comes_with_its_neighbours() {
        let v = node(&graph(), None, "db").unwrap();
        assert_eq!(v["linked_from"][0]["id"], "api");
        assert!(v["node"].get("pos").is_none());
        assert!(node(&graph(), None, "nope").is_err());
    }
}
