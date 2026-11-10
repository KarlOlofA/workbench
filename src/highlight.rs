use std::path::Path;
use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::html::{IncludeBackground, styled_line_to_highlighted_html};
use syntect::parsing::{SyntaxReference, SyntaxSet};

fn syntaxes() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static Theme {
    static T: OnceLock<Theme> = OnceLock::new();
    T.get_or_init(|| {
        let mut set = ThemeSet::load_defaults();
        set.themes.remove("base16-ocean.dark").expect("bundled theme")
    })
}

fn syntax_for(path: &str) -> &'static SyntaxReference {
    let ss = syntaxes();
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("");
    let ext = match ext {
        "ts" | "tsx" | "jsx" | "mjs" | "cjs" => "js",
        "toml" => "yaml",
        "zsh" | "fish" => "sh",
        other => other,
    };
    ss.find_syntax_by_extension(ext)
        .or_else(|| {
            Path::new(path)
                .file_name()
                .and_then(|f| f.to_str())
                .and_then(|f| ss.find_syntax_by_extension(f))
        })
        .unwrap_or_else(|| ss.find_syntax_plain_text())
}

pub fn highlight_line_html(path: &str, line: &str) -> String {
    let mut h = HighlightLines::new(syntax_for(path), theme());
    h.highlight_line(line, syntaxes())
        .ok()
        .and_then(|r| styled_line_to_highlighted_html(&r, IncludeBackground::No).ok())
        .unwrap_or_else(|| html_escape::encode_text(line).into_owned())
}
