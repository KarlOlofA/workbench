# Intent ledger spec (v1, frozen)

`.workbench/ledger.ndjson` is the source of truth for a project's architecture graph.
`.workbench/graph.json` is a materialized view of it. Implementation: `src/ledger.rs`.

## Files

| File | Role |
| --- | --- |
| `.workbench/ledger.ndjson` | Append-only log, one JSON entry per line. Never rewritten. |
| `.workbench/graph.json` | The ledger materialized, plus node positions. **Shape unchanged** (Graph/Node/Module/Flow in `src/graph.rs`), so external readers such as `workbench check` keep working. |
| `.workbench/.gitattributes` | Contains `ledger.ndjson merge=union`, so branches that both appended merge without conflicts. |

Optional fields added to Node later travel through `node.add` and `node.merge` like any other field: `export` (an exported surface, PRO-197) and `surface_hash` on each `reference.nodes` entry. Readers that don't know them ignore them.

`graph.baseline.json` and `node-history.jsonl` are unchanged in v1. Both could later be derived from the ledger.

## Entry

```json
{"id":"01JC9Z…","at":1763070000,"author":"Karl","source":"person","op":"node.merge","scope":null,"subject":"node","target_id":"db","patch":{"description":"…"}}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | ULID string | Unique. Lexicographic order is creation order. Entries are applied in `id` order. Duplicate ids (from a union merge) count once. |
| `at` | integer | Seconds since the Unix epoch. Informational: order comes from `id`. |
| `author` | string, optional | `git config user.name` of whoever ran Workbench. |
| `source` | `person` \| `agent` \| `outside` | A person edited the graph in Workbench, an agent edited it during a turn or job, or it changed on disk without Workbench (editor, git, another tool). |
| `proposed` | bool, default false | A suggestion. Left out of the graph until accepted. |
| `resolves` | ULID, optional | The proposal this entry accepts. |
| `op` | string | One of the ops below. Its fields sit at the top level of the entry. |

Node and flow ids inside ops are written as `target_id`, because `id` is always the entry's own ULID.

## Closed op set

Any other `op` value is a load error ("unknown variant"). Workbench then refuses to record and shows the error instead of silently dropping intent.

`scope` is the id of the module whose network the op applies to, or `null` for the top-level graph. Ops on a module itself use `"subject": "module"` and `scope: null`.

| Op | Fields | Effect |
| --- | --- | --- |
| `node.add` | `scope`, `subject`, `node` | Adds a node (`node` is a Graph Node with no `links` and no `pos`) or replaces the node with the same id. For modules only `id`, `title` and `description` are used. |
| `node.merge` | `scope`, `subject`, `target_id`, `patch` | Sets each field in `patch` on the node. A `null` value clears the field. Modules accept `title` and `description`. `patch` never contains `links` or `pos`. |
| `node.remove` | `scope`, `subject`, `target_id` | Removes the node (only the node: links to it are separate ops) or the module with its whole network. |
| `ref.add` | `scope`, `from`, `to` | Adds one link: `from` calls or depends on `to`. |
| `flow.put` | `scope`, `flow` | Adds a flow or replaces the flow with the same id. |
| `flow.remove` | `scope`, `target_id` | Removes a flow. |
| `tombstone` | `target` | Voids the entry `target`. Used for exactly two things: removing a link (target is its `ref.add`) and rejecting a proposal (target is the proposed entry). |

## Materialization

1. Collect every `tombstone` target.
2. Apply each entry in `id` order, skipping proposed and tombstoned entries.
3. Copy each node's `pos` from the current `graph.json`. Layout is not intent, so it is not in the ledger.

## Recording

Writers do not emit ops directly. Workbench, agents and git keep writing `graph.json` as before. Whenever Workbench loads the graph, sees it change on disk, or a person edits it, `ledger::record` diffs the materialized ledger against `graph.json` and appends the ops that explain the difference. The ops are attributed to `person`, `agent` (while a chat turn or job runs) or `outside`.

- **Bootstrap:** a project with a `graph.json` and no ledger gets genesis entries (`node.add`, `ref.add`, `flow.put`, all `outside`) the first time Workbench opens it.
- **Ledger without graph.json:** Workbench writes `graph.json` from the ledger.
- **Disagreement:** if `graph.json` on disk disagrees with the ledger, `graph.json` wins and the difference is recorded as `outside` ops.
- **External appends (PRO-246):** entries someone else appended since Workbench last read the ledger (a CI agent, a teammate's pull, another Workbench window) are adopted, never undone. Only what changed between the ledger Workbench last read and its graph is recorded, rebased onto the ledger on disk; `graph.json` is then rewritten from the result. When both sides changed the same field, the later entry wins as usual, and Workbench lists the conflict. The running app polls the ledger's size and modification time every 2 seconds (which also works on network mounts), waits while an agent turn or job runs, and shows a warning when the watcher stops checking. When a folder is first opened, `graph.json` still wins.

## Proposals (human-owned intent)

An agent may not change the intent (`description`) of an existing node or module. When it does, Workbench restores the description in `graph.json` and appends the agent's text as a `node.merge` with `"proposed": true`.

- **Accept:** a person-sourced `node.merge` with the same patch and `"resolves": <proposal id>`.
- **Reject:** a `tombstone` targeting the proposal. Rejected proposals stay in the log as accountability evidence.
