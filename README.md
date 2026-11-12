# Workbench

Workbench is a desktop app for shaping a codebase through an **architecture graph**, **project rules** and an **AI coding agent**. It is written in Rust with [Dioxus](https://dioxuslabs.com/) and runs as a native window on Linux, macOS and Windows.

You open a project folder, and Workbench shows how the code fits together as a graph of nodes (screens, services, stores, endpoints and so on). You talk to an agent in the chat. The agent edits the code and keeps the graph in step with it. You can also edit the graph yourself and ask the agent to make the code match.

## What it does

- **Architecture graph.** The graph is stored in `.workbench/graph.json` inside the opened project. Each node has a title, kind, description, files and links. Isolated features can be grouped into modules with their own network of nodes.
- **Changes against a baseline.** `.workbench/graph.baseline.json` holds the last graph that matched the code. The Changes pane lists what you changed since then, and you can send those changes to the agent to implement.
- **Flows.** You can draw a path through the nodes (for example "user logs in") and ask the agent to debug that flow.
- **Project rules.** Rules live in `.workbench/rules.json` and go into every agent session. An Audit action checks the code against all enabled rules.
- **Chat with an agent.** You choose the chat mode:
  - **Plan** only reads and proposes.
  - **Accept edits** may edit files.
  - **Auto** and **Full auto** let the agent run more steps without asking you.

  A chat session can hold more than one agent, and each agent has its own conversation. Sessions are saved, and they resume after a restart. You can close an agent from its chat tab.
- **Code view.** It has a file list, a CodeMirror editor with vim keys, git diffs against any base, a branch picker and GitHub pull requests through `gh`.
- **Projects.** A project groups the local clones of related repositories, for example a frontend and a backend. Each repository has its own graph, and the agent may read the other repositories for context.
- **Node history and ghost branches.** Node history shows who changed a node and when. Ghost branches show graph work on other branches.
- **Team coordination over git.** No server is needed. Everything comes from plain git branches.
  - **Claims.** Workbench works out a branch's claim from git. The claim is the nodes the branch changed in its graph, the files those nodes own, the code files the branch changed and the nodes that depend on them. If another branch's claim overlaps your changes, the Changes pane warns you before anything runs. It can also process only the changes that no other branch touches.
  - **Team view.** The **Team** button in the graph toolbar shows every branch with unmerged work and who committed to it. If another branch changes a node you are editing, or depends on it, the node gets a warning ring.
  - **Intent-conflict review.** While `graph.json` has a merge conflict, a review screen takes the place of the graph. Changes made on only one side are merged node by node. For each node that both sides changed in different ways, you see the two versions side by side and pick one. Saving stages the graph and suggests a commit message.
- **MCP servers.** Every provider can use MCP servers. You can add servers for all your projects or for one project (`.workbench/mcp.json`). Presets such as Linear show as switches in the MCP tab. If a remote server uses OAuth, you sign in from the MCP tab. The tokens are kept in your config folder and never in the project.
- **Self-updater (Linux and macOS).** The app checks GitHub for newer commits on `main` and can rebuild itself.

### LLM providers

Pick a provider and model in the header. The CLI agents can read and edit files. The API providers answer in the chat and can call tools on your MCP servers, but they do not edit files.

| Provider | What you need |
| --- | --- |
| Workbench (Claude Code) | The `claude` CLI installed and logged in |
| Codex (CLI) | The `codex` CLI on your `PATH` |
| Mistral Vibe (CLI) | The `vibe` CLI on your `PATH` |
| Anthropic API | `ANTHROPIC_API_KEY`, or a key added in the Providers tab |
| OpenAI API | `OPENAI_API_KEY`, or a key added in the Providers tab |
| Mistral API | `MISTRAL_API_KEY`, or a key added in the Providers tab |
| OpenRouter API | `OPENROUTER_API_KEY`, or a key added in the Providers tab |

The default provider is Claude Code. To install it, follow the [Claude Code docs](https://docs.claude.com/en/docs/claude-code), then run `claude` once in a terminal to log in.

## Using Workbench

1. Start the app with a project folder: `wbrs ~/code/my-app`. You can also start it without a folder and use the folder picker in the header.
2. If the project has no graph yet, click **Generate** in the graph toolbar. The agent reads the code and writes `.workbench/graph.json`.
3. Use the chat at the bottom of the window. Start in **Plan** mode, read the plan, then approve it in the mode you want the agent to use.
4. Edit nodes in the node inspector, add nodes with the toolbar, or describe new behaviour in the **Prompt** pane. Then open **Changes** and send the changes to the agent.
5. Add rules in the **Rules** view, for example "keep functions small". The agent follows the enabled rules in every session.
6. Commit `.workbench/` together with your code. This shares the graph, the baseline, the rules and the project MCP servers with your team.
7. When several people work on branches, click **Team** to see what each branch is changing. If merging a branch gives a conflict in `graph.json`, decide each conflicting node in the review screen.

You can also open a file directly: `wbrs <dir> <file>`.

Settings are stored in `~/.config/workbench/` (on Windows, `%APPDATA%\workbench\`). They include `providers.json`, `projects.json`, `chat_sessions.json` (saved chat sessions) and `mcp-oauth.json` (MCP sign-in tokens).

## Install and run

Each platform needs these tools:

- [Rust](https://rustup.rs/) (stable, with edition 2024 support)
- git
- On Linux and macOS, a C toolchain and the system webview libraries listed below

To use the default agent, also install the `claude` CLI (see [LLM providers](#llm-providers)). For the GitHub tab, install the [`gh` CLI](https://cli.github.com/).

### Linux

The install script finds what is missing and offers to install it with your package manager. It supports pacman, apt, dnf, zypper, apk and xbps.

```bash
git clone https://github.com/KarlOlofA/workbench.git
cd workbench
make install        # or: ./scripts/install.sh
```

The script does these things:

- It builds a release binary into `~/.local/lib/workbench/`.
- It adds a desktop entry (`~/.local/share/applications/se.codebrain.workbench.desktop`).
- It adds a `wbrs` launcher in `~/.local/bin`.

Make sure `~/.local/bin` is on your `PATH`, then run:

```bash
wbrs .              # open the current folder
```

If you install the libraries yourself, Workbench needs these: `webkit2gtk-4.1`, `gtk3`, `libsoup3`, `libxdo`, `pkg-config`, `make` and a C compiler. On Debian and Ubuntu, install them with:

```bash
sudo apt-get install build-essential pkg-config libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev libxdo-dev
```

WebKitGTK 4.1 needs Ubuntu 22.04 or later, or Debian 12 or later.

### macOS

```bash
xcode-select --install   # if the Command Line Tools are not installed yet
git clone https://github.com/KarlOlofA/workbench.git
cd workbench
make install
```

The script installs `~/Applications/Workbench.app` (ad-hoc signed) and a `wbrs` launcher in `~/.local/bin`. It also offers to install Rust with rustup if Rust is missing. Start the app from Launchpad or Spotlight, or from a terminal:

```bash
wbrs ~/code/my-app
```

macOS 11 or later is required.

### Windows

The install script and the self-updater do not run on Windows yet, so build the app with cargo.

1. Install [Rust](https://rustup.rs/) with the default MSVC toolchain. rustup offers to install the Visual Studio C++ Build Tools if they are missing.
2. Install [Git for Windows](https://git-scm.com/download/win).
3. Make sure the Microsoft Edge WebView2 Runtime is installed. Windows 10 and 11 normally include it. If not, download it from [Microsoft](https://developer.microsoft.com/microsoft-edge/webview2/).
4. Build and run the app in PowerShell:

```powershell
git clone https://github.com/KarlOlofA/workbench.git
cd workbench
cargo build --release
.\target\release\rust-ui-test.exe C:\path\to\your\project
```

You can copy `rust-ui-test.exe` to any folder and make a shortcut to it. Workbench finds CLIs installed by npm (`.cmd` shims) on your `PATH`. To update, run `git pull` and `cargo build --release` again.

### Updating and uninstalling (Linux and macOS)

```bash
make update      # rebuild and reinstall from the current checkout
make uninstall   # remove the app, the launcher and the desktop entry
```

When a newer build is on GitHub, the app shows an update button in the header. To turn the check off, set `WORKBENCH_NO_UPDATE_CHECK=1`.

## Development

```bash
make run      # cargo run -- .   (opens this repository in Workbench)
make test     # cargo test
make editor   # rebuild the CodeMirror bundle (needs Node.js and npm)
```

- `src/` contains the Rust app. `src/main.rs` is the entry point and `src/app.rs` is the root component.
- `editor-js/` is the source of the CodeMirror editor. It is bundled into `src/editor.bundle.js`. The bundle is committed, so you only need npm if you change the editor.
- `scripts/install.sh` is the Linux and macOS installer.
- `unsafe` Rust is not allowed. `#![forbid(unsafe_code)]` in `src/main.rs` enforces this. The project rules for this repository are in `.workbench/rules.json`.
- `.workbench/graph.json` is this repository's own architecture graph. Open the repository in Workbench to explore it.
