use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::graph::Graph;

pub const RULES_FILE: &str = ".workbench/rules.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct RulesFile {
    #[serde(default)]
    rules: Vec<Rule>,
}

pub fn load(cwd: &Path) -> Result<Vec<Rule>, String> {
    match std::fs::read_to_string(cwd.join(RULES_FILE)) {
        Ok(text) => serde_json::from_str::<RulesFile>(&text)
            .map(|f| f.rules)
            .map_err(|e| format!("{RULES_FILE}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{RULES_FILE}: {e}")),
    }
}

pub fn save(cwd: &Path, rules: &[Rule]) -> Result<(), String> {
    let path = cwd.join(RULES_FILE);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut text = serde_json::to_string_pretty(&RulesFile { rules: rules.to_vec() }).map_err(|e| e.to_string())?;
    text.push('\n');
    std::fs::write(path, text).map_err(|e| format!("{RULES_FILE}: {e}"))
}

pub fn new_id(rules: &[Rule]) -> String {
    (1..).map(|i| format!("r{i}")).find(|id| rules.iter().all(|r| &r.id != id)).unwrap()
}

pub fn relevant<'a>(rules: &'a [Rule], node_ids: Option<&[String]>) -> Vec<&'a Rule> {
    rules
        .iter()
        .filter(|r| r.enabled && !(r.title.trim().is_empty() && r.body.trim().is_empty()))
        .filter(|r| match (&r.scope, node_ids) {
            (None, _) => true,
            (Some(_), None) => true,
            (Some(s), Some(ids)) => ids.iter().any(|id| id == s),
        })
        .collect()
}

pub fn render(rules: &[&Rule], graph: &Graph) -> String {
    let mut out = String::new();
    for (i, r) in rules.iter().enumerate() {
        let scope = match &r.scope {
            Some(id) => format!(" (applies to node `{id}` — {})", graph.title_of(id)),
            None => String::new(),
        };
        let title = if r.title.trim().is_empty() { "Rule" } else { r.title.trim() };
        out.push_str(&format!("{}. **{title}**{scope}", i + 1));
        let body = r.body.trim();
        if !body.is_empty() {
            out.push('\n');
            for line in body.lines() {
                out.push_str("   ");
                out.push_str(line);
                out.push('\n');
            }
        } else {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, scope: Option<&str>, enabled: bool) -> Rule {
        Rule { id: id.into(), title: format!("Rule {id}"), body: "Do it".into(), enabled, scope: scope.map(String::from) }
    }

    #[test]
    fn relevance_filters_scope_and_enabled() {
        let rules = vec![rule("r1", None, true), rule("r2", Some("api"), true), rule("r3", Some("db"), true), rule("r4", None, false)];
        let ids = vec!["api".to_string()];
        let got: Vec<&str> = relevant(&rules, Some(&ids)).iter().map(|r| r.id.as_str()).collect();
        assert_eq!(got, ["r1", "r2"]);
        assert_eq!(relevant(&rules, None).len(), 3);
    }

    #[test]
    fn new_id_skips_taken() {
        assert_eq!(new_id(&[rule("r1", None, true), rule("r3", None, true)]), "r2");
    }

    #[test]
    fn enabled_defaults_true() {
        let f: RulesFile = serde_json::from_str(r#"{"rules":[{"id":"a","title":"T"}]}"#).unwrap();
        assert!(f.rules[0].enabled);
    }
}
