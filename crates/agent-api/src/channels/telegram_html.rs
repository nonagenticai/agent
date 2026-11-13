//! The agent's markdown as Telegram Bot API HTML (`parse_mode: "HTML"`), split into sendable parts.
//!
//! Agents answer in light markdown; Telegram shows it raw unless the text is sent with a parse mode. HTML is the
//! mode with the simplest escaping rules (`&`, `<`, `>`, and `"` inside an attribute), so every text is escaped
//! first and only the recognised markup becomes tags:
//!
//! - `**b**` / `__b__` → `<b>`, `*i*` / `_i_` → `<i>` (an underscore or asterisk inside a word stays literal, so
//!   `snake_case_name` is untouched), `` `code` `` → `<code>`, a ```` ``` ```` fence → `<pre>`.
//! - `[text](url)` → `<a href="url">text</a>` for an `http`/`https` URL only; any other link is its text alone.
//! - A `- ` / `* ` / `+ ` line is a list item (`• `), never italic; a `#` heading is a bold line.
//! - Anything else, unmatched delimiters included, is plain escaped text: text without markup reads exactly as sent.
//!
//! [`html_parts`] splits the rendered output, not the markdown: the document is a flat list of spans (text plus the
//! tags around it), a cut lands between or inside spans' text (a newline first, then whitespace, then a hard cut),
//! and a span cut in two is closed in one part and reopened in the next. Every part is well-formed HTML of at most
//! `max` characters, tags included, so it is under Telegram's 4096-character limit counted after entity parsing.
use std::collections::VecDeque;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tag {
    Bold,
    Italic,
    Code,
    Pre,
    Link(String),
}

impl Tag {
    fn open(&self, out: &mut String) {
        match self {
            Tag::Bold => out.push_str("<b>"),
            Tag::Italic => out.push_str("<i>"),
            Tag::Code => out.push_str("<code>"),
            Tag::Pre => out.push_str("<pre>"),
            Tag::Link(url) => {
                out.push_str("<a href=\"");
                escape_into(out, url, true);
                out.push_str("\">");
            }
        }
    }

    fn close(&self) -> &'static str {
        match self {
            Tag::Bold => "</b>",
            Tag::Italic => "</i>",
            Tag::Code => "</code>",
            Tag::Pre => "</pre>",
            Tag::Link(_) => "</a>",
        }
    }

    /// The characters this tag adds to a part: open + close.
    fn overhead(&self) -> usize {
        let mut open = String::new();
        self.open(&mut open);
        open.chars().count() + self.close().len()
    }
}

/// Unescaped text and the tags around it (outermost first).
#[derive(Clone, Debug)]
struct Span {
    tags: Vec<Tag>,
    text: String,
}

fn escaped_len(c: char, attr: bool) -> usize {
    match c {
        '&' => 5,
        '<' | '>' => 4,
        '"' if attr => 6,
        _ => 1,
    }
}

fn escape_into(out: &mut String, s: &str, attr: bool) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

fn render_span(s: &Span, out: &mut String) {
    if s.text.is_empty() {
        return;
    }
    for t in &s.tags {
        t.open(out);
    }
    escape_into(out, &s.text, false);
    for t in s.tags.iter().rev() {
        out.push_str(t.close());
    }
}

/// The whole markdown as Telegram HTML, unsplit.
pub fn to_html(markdown: &str) -> String {
    let mut out = String::new();
    for s in parse(markdown.trim()) {
        render_span(&s, &mut out);
    }
    out.trim_end().to_string()
}

/// The markdown as Telegram HTML parts of at most `max` characters each, every part well-formed on its own. An
/// empty or whitespace-only text has no parts.
pub fn html_parts(markdown: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut queue: VecDeque<Span> = parse(markdown.trim()).into();
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    let flush = |cur: &mut String, cur_len: &mut usize, parts: &mut Vec<String>| {
        let p = cur.trim_end();
        if !p.is_empty() {
            parts.push(p.to_string());
        }
        cur.clear();
        *cur_len = 0;
    };
    while let Some(mut sp) = queue.pop_front() {
        if cur_len == 0 && sp.tags.is_empty() {
            let t = sp.text.trim_start();
            if t.len() != sp.text.len() {
                sp.text = t.to_string();
            }
        }
        if sp.text.is_empty() {
            continue;
        }
        let mut over: usize = sp.tags.iter().map(Tag::overhead).sum();
        if cur_len == 0 && over >= max {
            // Only an absurd URL gets here: its text alone still goes out.
            sp.tags.clear();
            over = 0;
        }
        let need = over
            + sp.text
                .chars()
                .map(|c| escaped_len(c, false))
                .sum::<usize>();
        if cur_len + need <= max {
            render_span(&sp, &mut cur);
            cur_len += need;
            continue;
        }
        let budget = max.saturating_sub(cur_len + over);
        match cut(&sp.text, budget, cur_len == 0) {
            Some((head, tail)) => {
                let head = Span {
                    tags: sp.tags.clone(),
                    text: head,
                };
                render_span(&head, &mut cur);
                flush(&mut cur, &mut cur_len, &mut parts);
                queue.push_front(Span {
                    tags: sp.tags,
                    text: tail,
                });
            }
            None => {
                flush(&mut cur, &mut cur_len, &mut parts);
                queue.push_front(sp);
            }
        }
    }
    flush(&mut cur, &mut cur_len, &mut parts);
    parts
}

/// `text` cut so the head's escaped length fits `budget`: at the last newline that fits, else the last whitespace,
/// else (only when `must`, i.e. the part is empty) a hard cut. `None` means "start a new part first".
fn cut(text: &str, budget: usize, must: bool) -> Option<(String, String)> {
    let mut used = 0usize;
    let mut fit_end = 0usize;
    let mut last_nl = None;
    let mut last_ws = None;
    for (i, c) in text.char_indices() {
        used += escaped_len(c, false);
        if used > budget {
            break;
        }
        if c == '\n' {
            last_nl = Some(i);
        } else if c.is_whitespace() {
            last_ws = Some(i);
        }
        fit_end = i + c.len_utf8();
    }
    if let Some(i) = last_nl.or(last_ws) {
        let ws = text[i..].chars().next().map_or(1, char::len_utf8);
        return Some((text[..i].trim_end().to_string(), text[i + ws..].to_string()));
    }
    if !must {
        return None;
    }
    if fit_end == 0 {
        // A budget below one escaped character: take one anyway, or nothing ever goes out.
        fit_end = text.chars().next().map_or(0, char::len_utf8);
    }
    Some((text[..fit_end].to_string(), text[fit_end..].to_string()))
}

fn parse(md: &str) -> Vec<Span> {
    let mut out = Vec::new();
    let mut lines = md.lines();
    let mut first = true;
    while let Some(line) = lines.next() {
        if !first {
            push_plain(&mut out, &[], "\n");
        }
        first = false;
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("```") {
            let body = if let Some(inner) = rest.trim_end().strip_suffix("```") {
                // ```one line``` on a single line
                inner.to_string()
            } else {
                let mut body = Vec::new();
                for l in lines.by_ref() {
                    if l.trim_start().starts_with("```") {
                        break;
                    }
                    body.push(l);
                }
                body.join("\n")
            };
            if !body.trim().is_empty() {
                out.push(Span {
                    tags: vec![Tag::Pre],
                    text: body,
                });
            }
            continue;
        }
        if let Some(h) = heading(t) {
            let c: Vec<char> = h.chars().collect();
            inline(&c, &[Tag::Bold], false, &mut out);
            continue;
        }
        if let Some(item) = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .or_else(|| t.strip_prefix("+ "))
        {
            let indent = &line[..line.len() - t.len()];
            push_plain(&mut out, &[], &format!("{indent}• "));
            let c: Vec<char> = item.trim_start().chars().collect();
            inline(&c, &[], false, &mut out);
            continue;
        }
        let c: Vec<char> = line.chars().collect();
        inline(&c, &[], false, &mut out);
    }
    out
}

/// `# h` .. `###### h` → `h`.
fn heading(t: &str) -> Option<&str> {
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = t[hashes..].strip_prefix(' ')?.trim();
    (!rest.is_empty()).then_some(rest)
}

fn push_plain(out: &mut Vec<Span>, tags: &[Tag], text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.tags == tags
    {
        last.text.push_str(text);
        return;
    }
    out.push(Span {
        tags: tags.to_vec(),
        text: text.to_string(),
    });
}

fn with(tags: &[Tag], t: Tag) -> Vec<Tag> {
    let mut v = tags.to_vec();
    v.push(t);
    v
}

fn is_word(c: Option<&char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric())
}

fn inline(c: &[char], tags: &[Tag], in_link: bool, out: &mut Vec<Span>) {
    let mut plain = String::new();
    let mut i = 0;
    macro_rules! flush {
        () => {
            push_plain(out, tags, &plain);
            plain.clear();
        };
    }
    while i < c.len() {
        let ch = c[i];
        if ch == '`'
            && let Some(j) = (i + 1..c.len()).find(|&k| c[k] == '`')
            && j > i + 1
        {
            flush!();
            push_plain(
                out,
                &with(tags, Tag::Code),
                &c[i + 1..j].iter().collect::<String>(),
            );
            i = j + 1;
            continue;
        }
        if (ch == '*' || ch == '_') && c.get(i + 1) == Some(&ch) {
            if !is_word(i.checked_sub(1).and_then(|p| c.get(p)))
                && let Some(j) = close_double(c, i + 2, ch)
            {
                flush!();
                inline(&c[i + 2..j], &with(tags, Tag::Bold), in_link, out);
                i = j + 2;
                continue;
            }
            plain.push(ch);
            plain.push(ch);
            i += 2;
            continue;
        }
        if (ch == '*' || ch == '_')
            && !is_word(i.checked_sub(1).and_then(|p| c.get(p)))
            && c.get(i + 1).is_some_and(|n| !n.is_whitespace())
            && let Some(j) = close_single(c, i + 1, ch)
        {
            flush!();
            inline(&c[i + 1..j], &with(tags, Tag::Italic), in_link, out);
            i = j + 1;
            continue;
        }
        if ch == '['
            && !in_link
            && let Some((text_end, url_end)) = link_at(c, i)
        {
            flush!();
            let url: String = c[text_end + 2..url_end].iter().collect();
            let url = url.trim();
            let label = &c[i + 1..text_end];
            if safe_url(url) {
                inline(label, &with(tags, Tag::Link(url.to_string())), true, out);
            } else {
                inline(label, tags, true, out);
            }
            i = url_end + 1;
            continue;
        }
        plain.push(ch);
        i += 1;
    }
    flush!();
}

/// The closing `**`/`__` of a bold run opened before `from`: non-empty, no whitespace just inside either delimiter,
/// and (for `__`) not followed by a word character.
fn close_double(c: &[char], from: usize, ch: char) -> Option<usize> {
    if c.get(from).is_none_or(|n| n.is_whitespace()) {
        return None;
    }
    (from + 1..c.len().saturating_sub(1)).find(|&k| {
        c[k] == ch && c[k + 1] == ch && !c[k - 1].is_whitespace() && !is_word(c.get(k + 2))
    })
}

/// The closing `*`/`_` of an italic run whose content starts at `from`.
fn close_single(c: &[char], from: usize, ch: char) -> Option<usize> {
    (from + 1..c.len()).find(|&k| {
        c[k] == ch
            && !c[k - 1].is_whitespace()
            && c.get(k + 1) != Some(&ch)
            && c[k - 1] != ch
            && !is_word(c.get(k + 1))
    })
}

/// `[text](url)` at `i`: the index of `]` and of the `)` closing the URL (parentheses inside it balanced).
fn link_at(c: &[char], i: usize) -> Option<(usize, usize)> {
    let j = (i + 1..c.len()).find(|&k| c[k] == ']' || c[k] == '[')?;
    if c[j] != ']' || j == i + 1 || c.get(j + 1) != Some(&'(') {
        return None;
    }
    let mut depth = 0usize;
    for (k, &ch) in c.iter().enumerate().skip(j + 2) {
        match ch {
            '(' => depth += 1,
            ')' if depth == 0 => return (k > j + 2).then_some((j, k)),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

fn safe_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty()) && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tag opened in `html` is closed in order, and no tag or entity is cut.
    fn well_formed(html: &str) -> bool {
        let mut stack: Vec<String> = Vec::new();
        let mut rest = html;
        while let Some(i) = rest.find(['<', '&']) {
            let tail = &rest[i..];
            if tail.starts_with('&') {
                let Some(e) = tail.find(';') else {
                    return false;
                };
                if !["&amp;", "&lt;", "&gt;", "&quot;"].contains(&&tail[..=e]) {
                    return false;
                }
                rest = &tail[e + 1..];
                continue;
            }
            let Some(e) = tail.find('>') else {
                return false;
            };
            let tag = &tail[1..e];
            if let Some(name) = tag.strip_prefix('/') {
                if stack.pop().as_deref() != Some(name) {
                    return false;
                }
            } else {
                let name = tag.split(' ').next().unwrap_or("");
                if !["b", "i", "code", "pre", "a"].contains(&name) {
                    return false;
                }
                stack.push(name.to_string());
            }
            rest = &tail[e + 1..];
        }
        stack.is_empty() && !rest.contains('>')
    }

    #[test]
    fn plain_text_is_only_escaped() {
        assert_eq!(to_html("a < b & c > d"), "a &lt; b &amp; c &gt; d");
        assert_eq!(to_html("Hi there."), "Hi there.");
        assert_eq!(
            to_html("I've passed this conversation to a colleague, who will reply here."),
            "I've passed this conversation to a colleague, who will reply here."
        );
        assert_eq!(
            to_html("snake_case_name and 2*3*4"),
            "snake_case_name and 2*3*4"
        );
        assert_eq!(to_html("a ** b and _ c"), "a ** b and _ c");
        assert_eq!(
            to_html("<script>&amp;</script>"),
            "&lt;script&gt;&amp;amp;&lt;/script&gt;"
        );
    }

    #[test]
    fn bold_italic_code_and_links_are_tags() {
        assert_eq!(
            to_html("**Note:** read __this__"),
            "<b>Note:</b> read <b>this</b>"
        );
        assert_eq!(
            to_html("an *important* _point_"),
            "an <i>important</i> <i>point</i>"
        );
        assert_eq!(to_html("run `a<b`"), "run <code>a&lt;b</code>");
        assert_eq!(
            to_html("see [the docs](https://ex.com/a?x=1&y=\"2\")"),
            "see <a href=\"https://ex.com/a?x=1&amp;y=&quot;2&quot;\">the docs</a>"
        );
        assert_eq!(
            to_html("[Wiki](http://w.org/A_(b))."),
            "<a href=\"http://w.org/A_(b)\">Wiki</a>."
        );
        assert_eq!(
            to_html("**[x](https://a.b)**"),
            "<b><a href=\"https://a.b\">x</a></b>"
        );
        assert_eq!(
            to_html("```\nlet a = b < c;\n```"),
            "<pre>let a = b &lt; c;</pre>"
        );
        assert_eq!(to_html("## Your order"), "<b>Your order</b>");
    }

    #[test]
    fn a_non_http_link_is_its_text() {
        assert_eq!(to_html("[click](javascript:alert(1))"), "click");
        assert_eq!(to_html("[mail](mailto:a@b.c) or [x](ftp://h)"), "mail or x");
        assert_eq!(to_html("[sp](https://a b)"), "sp");
        assert_eq!(to_html("[not a link] (x)"), "[not a link] (x)");
    }

    #[test]
    fn bullets_are_list_items_not_italic() {
        assert_eq!(
            to_html("Options:\n* first *one*\n- second\n  + nested **b**"),
            "Options:\n• first <i>one</i>\n• second\n  • nested <b>b</b>"
        );
    }

    #[test]
    fn a_long_message_splits_into_well_formed_parts() {
        let para = "Some **bold words** and *italic* plus `code` & a [link](https://ex.com/p). ";
        let md = format!(
            "{}\n\n**{}**\n\n```\n{}```",
            para.repeat(80),
            "long bold ".repeat(300),
            "line < of code\n".repeat(200)
        );
        let parts = html_parts(&md, 4000);
        assert!(parts.len() >= 3, "{}", parts.len());
        for p in &parts {
            assert!(p.chars().count() <= 4000, "{}", p.chars().count());
            assert!(well_formed(p), "{p}");
        }
        // Nothing is lost: the visible text survives the cuts (whitespace at a cut aside).
        let visible = |s: &str| {
            let mut out = String::new();
            let mut in_tag = false;
            for c in s.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    c if !in_tag && !c.is_whitespace() => out.push(c),
                    _ => {}
                }
            }
            out
        };
        assert_eq!(visible(&parts.concat()), visible(&to_html(&md)));
        // A tight limit still yields well-formed parts.
        for p in html_parts(&md, 50) {
            assert!(p.chars().count() <= 50 && well_formed(&p), "{p}");
        }
        assert_eq!(html_parts("  \n ", 4000), Vec::<String>::new());
        assert_eq!(html_parts("a < b", 4000), vec!["a &lt; b"]);
    }
}
