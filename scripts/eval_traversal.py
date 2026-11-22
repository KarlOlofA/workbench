#!/usr/bin/env python3
"""Measures whether finding files through the intent graph is cheaper than grepping (PRO-232).

Every question in scripts/traversal_questions.json is a small task: name the file and the
function that does something. Each way below runs it with `claude -p` and records cost, tokens,
turns and whether an expected file was named:
  grep         the current way: the execution model with Grep, Glob and Read
  selected     the execution model gets the selected node as the app sends it (description,
               files, links), checks it and steps out one linked node at a time if it is wrong;
               tools: the query CLI and Read
  names        the execution model reads node names (query overview/search); tools: the query CLI and Read
  haiku-first  the traversal model (haiku) finds the files from node names, then the execution
               model does the task starting from those files, with Grep, Glob and Read as fallback

Costs real money, so it only runs by hand:
  scripts/eval_traversal.py <path-to-workbench-binary> [execution model] [ways...]
Defaults: model sonnet, ways grep selected haiku-first.
"""

import json
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TRAVERSAL_MODEL = "haiku"
TASK = "Name the source file and the function that does this. Reply with only JSON: {\"files\": [repository-relative paths], \"functions\": [names]}."
FIND = "Reply with only a JSON array of the repository-relative source files that answer the question, nothing else."
GREP_TOOLS = ["Grep", "Glob", "Read"]


def graph_instructions(query: str) -> str:
    return (
        "Find code through the Workbench intent graph, in this order: "
        "1. If the user selected nodes, check first that one fits the request from its description and files. If it does, work there. "
        f"If not, step out one node at a time: open a single linked node with `{query} node <id> [--module <m>]`, check it, and only then try the next. "
        f"2. Otherwise read the node names with `{query} overview` or `{query} search <words>`, pick the nodes that look right and open them with `node`. "
        "3. Only if no node fits, search the files directly. "
        f"`{query} file <path>` lists the nodes that own a file. All print JSON."
    )


def selected_context(query: str, ids: list) -> str:
    """The selected nodes as the app puts them in the prompt: description, files and links."""
    out = "The user has selected these nodes in the Workbench architecture graph. Their message is probably about them. Before you act, check that a selected node fits the message from its description and files. If none fits, step out one linked node at a time and check each, before searching anywhere else.\n\n"
    for full in ids:
        module, _, node_id = full.rpartition("/")
        args = query.split() + ["node", node_id] + (["--module", module] if module else [])
        v = json.loads(subprocess.run(args, capture_output=True, text=True, check=True).stdout)
        n = v["node"]
        links = ", ".join(f"{l['id']} ({l['title']})" for l in v["links_to"] + v["linked_from"])
        out += f"Node `{n['id']}` ({n['title']}): {n.get('description', '')}\nFiles: {', '.join(n.get('files', []))}\nLinked nodes: {links}\n\n"
    return out


def claude(prompt: str, model: str, tools: list, denied: list, system: str = "") -> dict:
    cmd = ["claude", "-p", "--output-format", "json", "--model", model, "--strict-mcp-config", "--setting-sources", "", "--allowedTools", *tools]
    if denied:
        cmd += ["--disallowedTools", *denied]
    if system:
        cmd += ["--append-system-prompt", system]
    out = subprocess.run(cmd, input=prompt, capture_output=True, text=True, cwd=REPO, timeout=600)
    try:
        r = json.loads(out.stdout)
    except json.JSONDecodeError:
        return {"cost": 0.0, "tokens": 0, "turns": 0, "answer": "", "error": out.stderr.strip()[:200]}
    usage = r.get("usage", {})
    tokens = sum(usage.get(k, 0) for k in ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"])
    return {"cost": r.get("total_cost_usd", 0.0), "tokens": tokens, "turns": r.get("num_turns", 0), "answer": r.get("result", "").strip()}


def add(a: dict, b: dict) -> dict:
    return {"cost": a["cost"] + b["cost"], "tokens": a["tokens"] + b["tokens"], "turns": a["turns"] + b["turns"], "answer": b["answer"]}


def run(question: dict, way: str, query: str, model: str) -> dict:
    graph_tools = [f"Bash({query}:*)", "Read"]
    task = f"{question['question']}\n\n{TASK}"
    if way == "grep":
        return claude(task, model, GREP_TOOLS, ["Bash"])
    if way == "selected":
        return claude(selected_context(query, question["select"]) + task, model, graph_tools, ["Grep", "Glob"], graph_instructions(query))
    if way == "names":
        return claude(task, model, graph_tools, ["Grep", "Glob"], graph_instructions(query))
    if way == "haiku-first":
        found = claude(f"{question['question']}\n\n{FIND}", TRAVERSAL_MODEL, graph_tools, ["Grep", "Glob"], graph_instructions(query))
        hint = f"The likely files, found through the Workbench graph: {found['answer']}\nStart there; search elsewhere only if they don't hold the answer.\n\n"
        return add(found, claude(hint + task, model, GREP_TOOLS, ["Bash"]))
    sys.exit(f"unknown way `{way}`")


def main() -> None:
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    query = f"{Path(sys.argv[1]).resolve()} query {REPO}"
    model = sys.argv[2] if len(sys.argv) > 2 else "sonnet"
    ways = sys.argv[3:] or ["grep", "selected", "haiku-first"]
    questions = json.loads((REPO / "scripts" / "traversal_questions.json").read_text())
    totals = {w: {"cost": 0.0, "tokens": 0, "turns": 0, "hits": 0} for w in ways}
    for q in questions:
        print(f"\n{q['question']}" + (f"  [{q['note']}]" if "note" in q else ""), flush=True)
        for way in ways:
            r = run(q, way, query, model)
            hit = any(f in r["answer"] for f in q["expect"])
            t = totals[way]
            t["cost"] += r["cost"]
            t["tokens"] += r["tokens"]
            t["turns"] += r["turns"]
            t["hits"] += int(hit)
            shown = " ".join((r["answer"] or r.get("error", "")).split())[:110]
            print(f"  {way:12} ${r['cost']:.4f}  {r['tokens']:>7} tok  {r['turns']:>2} turns  {'hit ' if hit else 'MISS'}  {shown}", flush=True)
    print(f"\nexecution model: {model}, traversal model: {TRAVERSAL_MODEL}")
    print(f"{'way':12} {'cost':>9} {'tokens':>9} {'turns':>6} {'hits':>6}")
    for way, t in totals.items():
        print(f"{way:12} ${t['cost']:>8.4f} {t['tokens']:>9} {t['turns']:>6} {t['hits']:>3}/{len(questions)}")


if __name__ == "__main__":
    main()
