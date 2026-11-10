use pulldown_cmark::{Event, Options, Parser, html};

pub fn to_html(src: &str) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(src, opts).map(|ev| match ev {
        Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn escapes_raw_html() {
        let out = super::to_html("hi <script>alert(1)</script>");
        assert!(!out.contains("<script>"));
    }
}
