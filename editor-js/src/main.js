import { EditorState, Compartment } from "@codemirror/state";
import {
  EditorView,
  keymap,
  lineNumbers,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  drawSelection,
  rectangularSelection,
  crosshairCursor,
  ViewPlugin,
} from "@codemirror/view";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import {
  indentOnInput,
  bracketMatching,
  foldGutter,
  foldKeymap,
  syntaxHighlighting,
  defaultHighlightStyle,
  StreamLanguage,
} from "@codemirror/language";
import { searchKeymap, highlightSelectionMatches } from "@codemirror/search";
import { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
import { oneDark } from "@codemirror/theme-one-dark";
import { vim, Vim } from "@replit/codemirror-vim";

import { rust } from "@codemirror/lang-rust";
import { javascript } from "@codemirror/lang-javascript";
import { python } from "@codemirror/lang-python";
import { json } from "@codemirror/lang-json";
import { markdown } from "@codemirror/lang-markdown";
import { html } from "@codemirror/lang-html";
import { css } from "@codemirror/lang-css";
import { yaml } from "@codemirror/lang-yaml";
import { go } from "@codemirror/lang-go";
import { cpp } from "@codemirror/lang-cpp";
import { java } from "@codemirror/lang-java";
import { sql } from "@codemirror/lang-sql";
import { xml } from "@codemirror/lang-xml";
import { shell } from "@codemirror/legacy-modes/mode/shell";
import { toml } from "@codemirror/legacy-modes/mode/toml";
import { lua } from "@codemirror/legacy-modes/mode/lua";
import { ruby } from "@codemirror/legacy-modes/mode/ruby";
import { swift } from "@codemirror/legacy-modes/mode/swift";
import { dockerFile } from "@codemirror/legacy-modes/mode/dockerfile";

const byExt = {
  rs: () => rust(),
  js: () => javascript(),
  mjs: () => javascript(),
  cjs: () => javascript(),
  jsx: () => javascript({ jsx: true }),
  ts: () => javascript({ typescript: true }),
  tsx: () => javascript({ typescript: true, jsx: true }),
  py: () => python(),
  json: () => json(),
  md: () => markdown(),
  markdown: () => markdown(),
  html: () => html(),
  htm: () => html(),
  css: () => css(),
  scss: () => css(),
  yaml: () => yaml(),
  yml: () => yaml(),
  go: () => go(),
  c: () => cpp(),
  h: () => cpp(),
  cc: () => cpp(),
  cpp: () => cpp(),
  hpp: () => cpp(),
  java: () => java(),
  kt: () => java(),
  sql: () => sql(),
  xml: () => xml(),
  svg: () => xml(),
  plist: () => xml(),
  sh: () => StreamLanguage.define(shell),
  bash: () => StreamLanguage.define(shell),
  zsh: () => StreamLanguage.define(shell),
  toml: () => StreamLanguage.define(toml),
  lua: () => StreamLanguage.define(lua),
  rb: () => StreamLanguage.define(ruby),
  swift: () => StreamLanguage.define(swift),
};

function languageFor(path) {
  const name = path.split("/").pop() || "";
  if (name === "Dockerfile") return StreamLanguage.define(dockerFile);
  if (name === "Makefile") return StreamLanguage.define(shell);
  const ext = name.includes(".") ? name.split(".").pop().toLowerCase() : "";
  const make = byExt[ext];
  return make ? make() : [];
}

function relativeNumbers(state) {
  const current = state.doc.lineAt(state.selection.main.head).number;
  return lineNumbers({
    formatNumber: (n) => (n === current ? String(n) : String(Math.abs(n - current))),
  });
}

const theme = EditorView.theme(
  {
    "&": { height: "100%", fontSize: "12.5px", backgroundColor: "#1b1d23" },
    ".cm-scroller": { fontFamily: '"SF Mono", Menlo, Monaco, monospace', lineHeight: "1.5" },
    ".cm-gutters": { backgroundColor: "#1b1d23", borderRight: "1px solid #353a45" },
    ".cm-activeLineGutter": { color: "#e5c07b", backgroundColor: "transparent" },
    ".cm-lineNumbers .cm-gutterElement": { minWidth: "3.5ch" },
    ".cm-panels-bottom": { borderTop: "1px solid #353a45", backgroundColor: "#22252c" },
    ".cm-vim-panel": { padding: "2px 8px", fontFamily: '"SF Mono", Menlo, monospace', fontSize: "12px" },
    ".cm-vim-panel input": { color: "#d8dbe2", fontFamily: "inherit" },
    ".cm-fat-cursor": { background: "#d97757 !important", color: "#1b1d23 !important" },
    "&:not(.cm-focused) .cm-fat-cursor": { outline: "1px solid #d97757", background: "transparent !important" },
  },
  { dark: true },
);

let exDefined = false;
let current = null;

function defineEx() {
  if (exDefined) return;
  exDefined = true;
  const editor = () => current;
  Vim.defineEx("write", "w", () => editor()?.save());
  Vim.defineEx("update", "up", () => editor()?.save());
  Vim.defineEx("quit", "q", () => editor()?.close(false));
  Vim.defineEx("wq", "wq", () => {
    editor()?.save();
    editor()?.close(true);
  });
  Vim.defineEx("xit", "x", () => {
    editor()?.save();
    editor()?.close(true);
  });
  Vim.defineEx("edit", "e", () => editor()?.send({ type: "reload" }));
  Vim.map("<C-s>", ":w<CR>", "normal");
  Vim.map("Y", "y$", "normal");
}

function mount(host, send) {
  defineEx();

  const language = new Compartment();
  const numbers = new Compartment();
  let path = "";
  let changeTimer = null;
  let suppress = false;

  const flush = () => {
    clearTimeout(changeTimer);
    changeTimer = null;
    send({ type: "change", text: view.state.doc.toString() });
  };

  const api = {
    send,
    save() {
      clearTimeout(changeTimer);
      changeTimer = null;
      send({ type: "save", text: view.state.doc.toString() });
    },
    close(saved) {
      send({ type: "close", saved });
    },
  };

  const relativeTracker = ViewPlugin.fromClass(
    class {
      constructor(v) {
        this.line = v.state.doc.lineAt(v.state.selection.main.head).number;
      }
      update(u) {
        const line = u.state.doc.lineAt(u.state.selection.main.head).number;
        if (line !== this.line) {
          this.line = line;
          queueMicrotask(() => u.view.dispatch({ effects: numbers.reconfigure(relativeNumbers(u.view.state)) }));
        }
      }
    },
  );

  const extensions = (doc, p) => [
    vim({ status: true }),
    numbers.of(relativeNumbers(EditorState.create({ doc }))),
    relativeTracker,
    highlightActiveLineGutter(),
    highlightSpecialChars(),
    history(),
    foldGutter(),
    drawSelection(),
    EditorState.allowMultipleSelections.of(true),
    indentOnInput(),
    syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
    bracketMatching(),
    closeBrackets(),
    rectangularSelection(),
    crosshairCursor(),
    highlightActiveLine(),
    highlightSelectionMatches(),
    keymap.of([
      { key: "Mod-s", run: () => (api.save(), true), preventDefault: true },
      ...closeBracketsKeymap,
      ...defaultKeymap,
      ...searchKeymap,
      ...historyKeymap,
      ...foldKeymap,
      indentWithTab,
    ]),
    language.of(languageFor(p)),
    oneDark,
    theme,
    EditorView.updateListener.of((u) => {
      if (!u.docChanged || suppress) return;
      clearTimeout(changeTimer);
      changeTimer = setTimeout(flush, 150);
    }),
  ];

  const view = new EditorView({
    parent: host,
    state: EditorState.create({ doc: "", extensions: extensions("", "") }),
  });

  current = api;

  window.addEventListener("focus", () => {
    const active = document.activeElement;
    const typingElsewhere = active && active !== document.body && !host.contains(active);
    if (host.offsetParent !== null && !typingElsewhere) view.focus();
  });

  return {
    load(newPath, text, keepCursor) {
      clearTimeout(changeTimer);
      changeTimer = null;
      const samePath = newPath === path;
      const head = view.state.selection.main.head;
      const scrollTop = view.scrollDOM.scrollTop;
      path = newPath;
      suppress = true;
      if (samePath && keepCursor) {
        view.dispatch({
          changes: { from: 0, to: view.state.doc.length, insert: text },
          selection: { anchor: Math.min(head, text.length) },
        });
        view.scrollDOM.scrollTop = scrollTop;
      } else {
        view.setState(EditorState.create({ doc: text, extensions: extensions(text, newPath) }));
      }
      suppress = false;
      current = api;
    },
    focus() {
      view.focus();
    },
  };
}

window.WB = { mount };
