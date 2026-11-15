//! Verification before absorbing. An agent's change to a node only moves into the baseline
//! (counts as done) when the node's description passes the lint and a model confirms its code
//! does what the description says. Otherwise the change stays pending with the reason.
//!
//! The checks themselves live in `workbench-check` (workbench_auditer repo), the library
//! behind the `workbench check` CI gate, so the app and CI judge the same way.

use std::path::Path;

use workbench_check::config::Config;
use workbench_check::lint;
use workbench_check::satisfaction::{self, Verdict};
use workbench_llm::{Anthropic, Model};

use crate::graph::{Change, ChangeKind, Graph, Subject};

/// Whether absorbing `c` needs verification: a node that was added or changed and owns
/// files here. Removals, modules, references and module attachments have no code of their
/// own to verify.
pub fn needs_check(c: &Change) -> bool {
    let Some(n) = c.after.as_ref() else { return false };
    c.subject == Subject::Node && c.kind != ChangeKind::Removed && !n.is_reference() && n.module.is_none() && !n.files.is_empty()
}

/// The model the app verifies with: Anthropic, as configured in `.workbench/check.toml`.
pub fn anthropic(config: &Config) -> Result<Box<dyn Model>, String> {
    Anthropic::from_env(&config.model, &config.effort).map(|m| Box::new(m) as Box<dyn Model>)
}

/// Checks the node of each change in `cur`. `Ok` means satisfied; `Err` says why the change
/// stays pending. `connect` is only called when a node gets as far as the model.
/// Blocking: reads the node's files and calls the model.
///
/// The same graph, code and rules always give the same results: the request is built only
/// from them, in a fixed order, and each answer is cached by the request's hash in
/// `.workbench/.check-cache`, so a request is never sent to the model twice.
pub fn run(root: &Path, cur: &Graph, changes: &[Change], connect: impl FnOnce(&Config) -> Result<Box<dyn Model>, String>) -> Vec<Result<(), String>> {
    let config = match Config::load(root) {
        Ok(c) => c,
        Err(e) => return changes.iter().map(|_| Err(e.clone())).collect(),
    };
    let rules = workbench_graph::load_rules(root).unwrap_or_default();
    let mut connect = Some(connect);
    let mut model: Option<Result<Box<dyn Model>, String>> = None;
    let mut out = Vec::new();
    for c in changes {
        let result = (|| {
            let after = c.after.as_ref().ok_or("the node was removed")?;
            let node: workbench_graph::Node = serde_json::to_value(after).and_then(serde_json::from_value).map_err(|e| e.to_string())?;
            if let Some(problem) = lint::problem(&node, config.min_description_words) {
                return Err(format!("description lint: {problem}"));
            }
            let source = satisfaction::gather_source(root, &node, config.max_node_bytes)?;
            let net = cur.net(c.scope.as_deref()).ok_or("its module is gone")?;
            // Sorted: link order means nothing (the graph diff ignores it), so it mustn't change the request.
            let mut neighbours: Vec<String> = node.links.iter().filter_map(|l| net.node(l)).map(|n| n.title.clone()).collect();
            neighbours.sort();
            let request = satisfaction::request(&node, &workbench_graph::rules_for(&rules, &node.id), &neighbours, &source);
            let model = model
                .get_or_insert_with(|| connect.take().map_or_else(|| Err("no model".to_string()), |c| c(&config)))
                .as_ref()
                .map_err(|e| format!("satisfaction check: {e}"))?;
            let outcome = satisfaction::check(root, model.as_ref(), &config.effort, &request, &source).map_err(|e| format!("satisfaction check: {e}"))?;
            match outcome.verdict {
                Verdict::Satisfied => Ok(()),
                Verdict::Drifted(_) => Err(format!("the code doesn't do what the description says: {}", outcome.summary)),
                Verdict::Uncertain(why) => Err(format!("the model couldn't confirm the description: {why}")),
            }
        })();
        out.push(result);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{self, Node};

    #[test]
    fn vague_descriptions_stay_pending_without_calling_a_model() {
        let dir = std::env::temp_dir().join(format!("wb-verify-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/api.rs"), "fn main() {}\n").unwrap();
        let base = Graph::default();
        let api = Node { id: "api".into(), title: "API".into(), description: "Handles stuff.".into(), files: vec!["src/api.rs".into()], ..Default::default() };
        let cur = Graph { nodes: vec![api], ..Default::default() };
        let changes = graph::diff(&base, &cur);
        assert!(needs_check(&changes[0]));
        let results = run(&dir, &cur, &changes, |_| Err("the lint must stop this node before any model is needed".into()));
        assert!(results[0].as_ref().unwrap_err().starts_with("description lint"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    use std::cell::Cell;
    use std::rc::Rc;
    use workbench_llm::{JsonRequest, LlmError};

    /// A model that always gives the same answer and counts how often it is asked.
    struct Fake {
        calls: Rc<Cell<usize>>,
        answer: serde_json::Value,
    }

    impl Model for Fake {
        fn id(&self) -> &str {
            "fake"
        }
        fn complete_json(&self, _request: &JsonRequest) -> Result<serde_json::Value, LlmError> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.answer.clone())
        }
    }

    const RETRY: &str = "Sends one event to the collector with up to three attempts, waiting one second between attempts, and returns the last error.";

    fn project(name: &str) -> std::path::PathBuf {
        let dir = temp(name);
        std::fs::write(dir.join("src/send.rs"), "pub fn send(e: &str) -> Result<(), String> {\n    post(e)\n}\n").unwrap();
        std::fs::write(dir.join("src/wire.rs"), "pub fn post(e: &str) -> Result<(), String> {\n    Ok(())\n}\n").unwrap();
        dir
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wb-verify-{name}-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        dir
    }

    fn graph_with(send: Node) -> Graph {
        let other = |id: &str| Node { id: id.into(), title: id.to_uppercase(), ..Default::default() };
        Graph { nodes: vec![send, other("store"), other("auth")], ..Default::default() }
    }

    fn send_node(description: &str, files: &[&str], links: &[&str]) -> Node {
        Node {
            id: "send".into(),
            title: "Send".into(),
            description: description.into(),
            files: files.iter().map(|f| f.to_string()).collect(),
            links: links.iter().map(|l| l.to_string()).collect(),
            ..Default::default()
        }
    }

    /// Runs the check on the `send` node with a fake model.
    fn check(dir: &Path, send: Node, calls: &Rc<Cell<usize>>, answer: serde_json::Value) -> Result<(), String> {
        let cur = graph_with(send);
        let changes = graph::diff(&Graph::default(), &cur);
        let send_change: Vec<Change> = changes.into_iter().filter(|c| c.id == "send").collect();
        let calls = calls.clone();
        run(dir, &cur, &send_change, move |_| Ok(Box::new(Fake { calls, answer }) as Box<dyn Model>)).remove(0)
    }

    fn satisfied() -> serde_json::Value {
        serde_json::json!({ "verdict": "satisfied", "summary": "does what it says", "claims": [] })
    }

    #[test]
    fn the_same_inputs_give_the_same_verdict_without_asking_twice() {
        let dir = project("same");
        let calls = Rc::new(Cell::new(0));
        let node = send_node(RETRY, &["src/send.rs", "src/wire.rs"], &["store", "auth"]);
        let first = check(&dir, node.clone(), &calls, satisfied());
        let second = check(&dir, node.clone(), &calls, satisfied());
        assert_eq!(first, Ok(()));
        assert_eq!(first, second);
        assert_eq!(calls.get(), 1, "the second run must come from the cache");

        // Reordering files or links is the same intent, so the same request.
        let reordered = send_node(RETRY, &["src/wire.rs", "src/send.rs"], &["auth", "store"]);
        assert_eq!(check(&dir, reordered, &calls, satisfied()), first);
        assert_eq!(calls.get(), 1);

        // A different description, or different code, is a new question.
        check(&dir, send_node(&format!("{RETRY} It logs every failure."), &["src/send.rs", "src/wire.rs"], &["store", "auth"]), &calls, satisfied()).unwrap();
        assert_eq!(calls.get(), 2);
        std::fs::write(dir.join("src/wire.rs"), "pub fn post(e: &str) -> Result<(), String> {\n    Err(e.into())\n}\n").unwrap();
        check(&dir, node, &calls, satisfied()).unwrap();
        assert_eq!(calls.get(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejections_are_identical_across_runs() {
        let dir = project("reject");
        let calls = Rc::new(Cell::new(0));
        // Drift whose quote is not in the file: the evidence fails verification, so "uncertain".
        let answer = serde_json::json!({
            "verdict": "drifted",
            "summary": "no retries",
            "claims": [{ "claim": "three attempts", "status": "contradicted", "path": "src/send.rs", "line": 2, "quote": "for _ in 0..3", "explanation": "single call" }]
        });
        let node = send_node(RETRY, &["src/send.rs"], &[]);
        let first = check(&dir, node.clone(), &calls, answer.clone());
        let second = check(&dir, node, &calls, answer);
        assert!(first.as_ref().unwrap_err().starts_with("the model couldn't confirm"));
        assert_eq!(first, second);
        assert_eq!(calls.get(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The verdict without the model's wording, which may vary between samples.
    fn class(r: &Result<(), String>) -> &'static str {
        match r {
            Ok(()) => "satisfied",
            Err(e) if e.starts_with("description lint") => "lint",
            Err(e) if e.starts_with("the code doesn't") => "drifted",
            Err(e) if e.starts_with("the model couldn't") => "uncertain",
            Err(_) => "error",
        }
    }

    fn cache_files(dir: &Path) -> usize {
        let answers = std::fs::read_dir(dir.join(satisfaction::CACHE_DIR)).into_iter().flatten().flatten();
        answers.filter(|e| e.path().extension().is_some_and(|x| x == "json")).count()
    }

    /// Live, needs ANTHROPIC_API_KEY: `cargo test live_ -- --ignored --nocapture`.
    /// Clear-cut nodes get the right verdict on every fresh sample, and a warm cache gives
    /// byte-identical results without asking the model again.
    #[test]
    #[ignore]
    fn live_verdicts_are_correct_and_stable() {
        let files = [
            ("src/total.rs", "/// Sum of prices in cents.\npub fn total(prices: &[u64]) -> u64 {\n    prices.iter().sum()\n}\n"),
            (
                "src/pay.rs",
                "pub fn charge(client: &Client, amount: u64) -> Result<Receipt, Error> {\n    // One attempt; errors go straight back to the caller.\n    client.post(\"/charge\", amount)\n}\n",
            ),
        ];
        let node = |id: &str, description: &str, file: &str| Node {
            id: id.into(),
            title: id.into(),
            kind: "service".into(),
            description: description.into(),
            files: vec![file.into()],
            ..Default::default()
        };
        let g = Graph {
            nodes: vec![
                node("total", "Adds up a list of prices given in cents and returns the total in cents. An empty list gives a total of zero.", "src/total.rs"),
                node(
                    "charge",
                    "Charges the customer through the payment server. A failed request is retried up to three times, waiting one second between attempts, before the last error is returned.",
                    "src/pay.rs",
                ),
                node("vague", "Handles stuff.", "src/total.rs"),
            ],
            ..Default::default()
        };
        let changes = graph::diff(&Graph::default(), &g);
        let fresh = || {
            let dir = temp("live");
            for (path, text) in files {
                std::fs::write(dir.join(path), text).unwrap();
            }
            dir
        };

        let mut rounds = Vec::new();
        let mut first_dir = None;
        for round in 0..3 {
            let dir = fresh();
            let results = run(&dir, &g, &changes, anthropic);
            println!("round {round}:");
            for (c, r) in changes.iter().zip(&results) {
                println!("  {:<7} {:<10} {}", c.id, class(r), r.as_ref().err().map(String::as_str).unwrap_or(""));
            }
            rounds.push(results);
            first_dir.get_or_insert(dir);
        }
        let classes: Vec<Vec<&str>> = rounds.iter().map(|r| r.iter().map(class).collect()).collect();
        let agree = classes.iter().filter(|c| **c == classes[0]).count();
        println!("verdict agreement across fresh samples: {agree}/{}", classes.len());
        for c in &classes {
            assert_eq!(c[0], "satisfied", "the correct node must pass every time");
            assert!(matches!(c[1], "drifted" | "uncertain"), "the drifted node must never be absorbed");
            assert_eq!(c[2], "lint", "the vague node must stop at the lint");
        }

        let dir = first_dir.unwrap();
        let cached = cache_files(&dir);
        let again = run(&dir, &g, &changes, anthropic);
        assert_eq!(again, rounds[0], "a warm cache must give byte-identical results");
        assert_eq!(cache_files(&dir), cached, "a warm cache must not ask the model again");
        println!("warm cache: identical results, {cached} cached answers, no new requests");
    }

    /// Live, needs ANTHROPIC_API_KEY: verdicts for a few real nodes of this repo, checked on a
    /// copy so the cache isn't written into the project. Printed, not asserted.
    #[test]
    #[ignore]
    fn live_native_nodes() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let g = graph::load(repo, graph::GRAPH_FILE).unwrap().unwrap();
        let ids = ["graph-model", "rules-model", "claims"];
        let dir = temp("native");
        std::fs::create_dir_all(dir.join(".workbench")).unwrap();
        let _ = std::fs::copy(repo.join(".workbench/rules.json"), dir.join(".workbench/rules.json"));
        let changes: Vec<Change> = graph::diff(&Graph::default(), &g).into_iter().filter(|c| ids.contains(&c.id.as_str())).collect();
        for c in &changes {
            for f in &c.after.as_ref().unwrap().files {
                std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
                std::fs::copy(repo.join(f), dir.join(f)).unwrap();
            }
        }
        for (c, r) in changes.iter().zip(run(&dir, &g, &changes, anthropic)) {
            println!("{:<12} {:<10} {}", c.id, class(&r), r.as_ref().err().map(String::as_str).unwrap_or(""));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removals_and_nodes_without_files_need_no_check() {
        let api = Node { id: "api".into(), title: "API".into(), ..Default::default() };
        let g = Graph { nodes: vec![api], ..Default::default() };
        assert!(!needs_check(&graph::diff(&Graph::default(), &g)[0]));
        assert!(!needs_check(&graph::diff(&g, &Graph::default())[0]));
    }
}
