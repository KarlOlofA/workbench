use std::path::Path;

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

pub fn to_html(src: &str) -> String {
    render(src, None)
}

/// Like `to_html`, but file references such as `src/chat.rs` or `src/chat.rs:42:7` become links
/// (`a.file-ref`) that the chat opens in the editor. They are read relative to `cwd`; a path
/// written in backticks that isn't there is shown as a missing file. Plain words only become
/// links when the file exists, so ordinary text with slashes stays text.
pub fn to_html_with_files(src: &str, cwd: &Path) -> String {
    render(src, Some(cwd))
}

/// Plain text (a message the user typed), escaped, with references to existing files as links.
pub fn text_with_files(text: &str, cwd: &Path) -> String {
    text.split('\n').map(|line| linkify_words(line, cwd)).collect::<Vec<_>>().join("\n")
}

fn render(src: &str, cwd: Option<&Path>) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut in_link = false;
    let events = Parser::new_ext(src, opts).map(|ev| match ev {
        Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
        Event::Start(Tag::Link { .. }) => {
            in_link = true;
            ev
        }
        Event::End(TagEnd::Link) => {
            in_link = false;
            ev
        }
        Event::Code(code) if !in_link && cwd.is_some() => match file_ref(&code, cwd.unwrap_or(Path::new(".")), true) {
            Some(link) => Event::InlineHtml(CowStr::from(link.replace("{text}", &format!("<code>{}</code>", escape(&code))))),
            None => Event::Code(code),
        },
        Event::Text(text) if !in_link && cwd.is_some() => Event::InlineHtml(CowStr::from(linkify_words(&text, cwd.unwrap_or(Path::new("."))))),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

/// Escaped for HTML text and double-quoted attributes.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Plain text with every word that names an existing file turned into a link.
fn linkify_words(text: &str, cwd: &Path) -> String {
    let mut out = String::new();
    for (i, word) in text.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        // Punctuation around a path in a sentence isn't part of it: "see src/a.rs."
        let core = word.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | ';' | '"' | '\'' | '“' | '”')).trim_end_matches(['.', ':']);
        // Only words with a dot or a slash can name a file; checking the disk for the rest is waste.
        let maybe_file = core.contains(['.', '/']);
        match maybe_file.then(|| file_ref(core, cwd, false)).flatten() {
            Some(link) => {
                let start = word.find(core).unwrap_or(0);
                out.push_str(&escape(&word[..start]));
                out.push_str(&link.replace("{text}", &escape(core)));
                out.push_str(&escape(&word[start + core.len()..]));
            }
            None => out.push_str(&escape(word)),
        }
    }
    out
}

/// A link for `text` if it is a file reference, with `{text}` where the link text goes.
/// `strict` (backticks) also links paths that don't exist, shown as missing.
fn file_ref(text: &str, cwd: &Path, strict: bool) -> Option<String> {
    let (path, line, col) = split_position(text.trim());
    if path.is_empty() || path.contains(char::is_whitespace) || path.contains("://") {
        return None;
    }
    let rel = Path::new(path).strip_prefix(cwd).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string());
    match std::fs::metadata(cwd.join(&rel)) {
        Ok(meta) if meta.is_file() => {
            let size = human_size(meta.len());
            Some(format!(
                r#"<a class="file-ref" data-path="{}" data-line="{line}" data-col="{col}" title="{} · {size} · click to open">{{text}}</a>"#,
                escape(&rel),
                escape(&rel)
            ))
        }
        _ if strict && looks_like_path(path) => Some(format!(r#"<a class="file-ref missing" title="No such file: {}">{{text}}</a>"#, escape(&rel))),
        _ => None,
    }
}

/// `path:line:col`, `path:line` or `path`. A line range (`path:10-20`) goes to its first line.
fn split_position(text: &str) -> (&str, u32, u32) {
    let number = |s: &str| s.split('-').next().and_then(|n| n.parse::<u32>().ok());
    let mut parts = text.rsplitn(3, ':');
    let (last, mid, first) = (parts.next(), parts.next(), parts.next());
    match (first, mid.and_then(number), last.and_then(number)) {
        (Some(path), Some(line), Some(col)) => (path, line, col),
        _ => match (mid, last.and_then(number)) {
            (Some(_), Some(line)) => (&text[..text.rfind(':').unwrap_or(0)], line, 0),
            _ => (text, 0, 0),
        },
    }
}

/// A directory part and a file extension, like `src/chat.rs`.
fn looks_like_path(p: &str) -> bool {
    p.contains('/') && Path::new(p).extension().is_some()
}

fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{} KB", b / 1024),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_raw_html() {
        let out = super::to_html("hi <script>alert(1)</script>");
        assert!(!out.contains("<script>"));
    }

    #[test]
    fn positions_split_off_the_path() {
        assert_eq!(split_position("src/a.rs"), ("src/a.rs", 0, 0));
        assert_eq!(split_position("src/a.rs:42"), ("src/a.rs", 42, 0));
        assert_eq!(split_position("src/a.rs:42:7"), ("src/a.rs", 42, 7));
        assert_eq!(split_position("src/a.rs:10-20"), ("src/a.rs", 10, 0));
    }

    #[test]
    fn file_references_become_links() {
        let cwd = std::env::temp_dir().join(format!("wb-md-{}", std::process::id()));
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::write(cwd.join("src/a.rs"), "fn main() {}").unwrap();

        let out = to_html_with_files("See `src/a.rs:3`, then src/a.rs. Also `src/gone.rs` and a/b.", &cwd);
        assert!(out.contains(r#"data-path="src/a.rs" data-line="3""#), "{out}");
        assert_eq!(out.matches(r#"class="file-ref""#).count(), 2, "{out}");
        assert!(out.contains(r#"class="file-ref missing""#), "{out}");
        assert!(!out.contains("a/b</a>"), "{out}");
        // Text keeps its escaping.
        assert!(to_html_with_files("1 < 2 & “quoted” — ok", &cwd).contains("1 &lt; 2 &amp; “quoted” — ok"));
        std::fs::remove_dir_all(&cwd).unwrap();
    }
}
