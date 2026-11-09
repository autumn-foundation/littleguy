//! A small HTML to text pass for web pages.
//!
//! It is not a full parser. It drops scripts, styles and comments, turns
//! block tags into line breaks, decodes the common entities, and folds
//! white space and empty lines. That is enough for a model to read a page.

/// Tags whose content is not text for a reader.
const SKIPPED: [&str; 6] = ["script", "style", "noscript", "template", "svg", "iframe"];

/// Tags that start a new line.
const BLOCKS: [&str; 28] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "dd",
    "div",
    "dl",
    "dt",
    "figcaption",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "title",
];

/// Tags that also start a new line: tables and lists.
const ROWS: [&str; 5] = ["table", "tr", "ul", "td", "th"];

/// The readable text of `html`.
#[must_use]
pub fn to_text(html: &str) -> String {
    let mut raw = String::with_capacity(html.len() / 2);
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        raw.push_str(&decode(&rest[..open]));
        rest = &rest[open..];
        if let Some(body) = rest.strip_prefix("<!--") {
            rest = body.find("-->").map_or("", |end| &body[end + 3..]);
            continue;
        }
        let Some(close) = rest.find('>') else {
            // A lone `<` is text.
            raw.push_str(&decode(rest));
            rest = "";
            break;
        };
        let tag = &rest[1..close];
        rest = &rest[close + 1..];
        let name = tag_name(tag);
        let closing = tag.starts_with('/');
        if !closing && SKIPPED.contains(&name.as_str()) && !tag.ends_with('/') {
            rest = skip_to_end(rest, &name);
        } else if BLOCKS.contains(&name.as_str()) || ROWS.contains(&name.as_str()) {
            raw.push('\n');
        }
        // An inline tag adds nothing, so it never splits a word.
    }
    raw.push_str(&decode(rest));
    fold(&raw)
}

/// The lower-case name of a tag body such as `/div class="x"`.
fn tag_name(tag: &str) -> String {
    tag.trim_start_matches('/')
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The text after the closing tag of `name`, or nothing when it is missing.
fn skip_to_end<'a>(rest: &'a str, name: &str) -> &'a str {
    let lower = rest.to_ascii_lowercase();
    let needle = format!("</{name}");
    lower.find(&needle).map_or("", |start| {
        let after = &rest[start..];
        after.find('>').map_or("", |end| &after[end + 1..])
    })
}

/// Folds each line to single spaces, and drops the empty lines.
fn fold(raw: &str) -> String {
    raw.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decodes the named entities that pages use most, and numeric ones.
#[must_use]
pub fn decode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let entity = rest
            .find(';')
            .filter(|end| *end <= 10)
            .and_then(|end| entity(&rest[1..end]).map(|c| (c, end)));
        if let Some((c, end)) = entity {
            out.push(c);
            rest = &rest[end + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// The character of the entity `name`, without its `&` and `;`.
fn entity(name: &str) -> Option<char> {
    let numeric = name.strip_prefix('#').and_then(|number| {
        number
            .strip_prefix(['x', 'X'])
            .map_or_else(
                || number.parse().ok(),
                |hex| u32::from_str_radix(hex, 16).ok(),
            )
            .and_then(char::from_u32)
    });
    numeric.or(match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "hellip" => Some('…'),
        "rsquo" => Some('’'),
        "lsquo" => Some('‘'),
        "rdquo" => Some('”'),
        "ldquo" => Some('“'),
        "copy" => Some('©'),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn a_page_becomes_lines_of_text() {
        let page = "<html><head><title>Tea &amp; cake</title>\
            <style>body { color: red }</style>\
            <script>alert('<p>no</p>')</script></head>\
            <body><h1>Menu</h1><p>Green <b>tea</b>,\n   black tea.</p>\
            <!-- a <p>hidden</p> note --><ul><li>one</li><li>two&nbsp;&#x1F375;</li></ul>\
            <SCRIPT type=x>bad()</SCRIPT>end</body></html>";
        assert_eq!(
            to_text(page),
            "Tea & cake\nMenu\nGreen tea,\nblack tea.\none\ntwo 🍵\nend"
        );
    }

    #[test]
    fn broken_markup_keeps_its_text() {
        assert_eq!(to_text("a < b and c"), "a < b and c");
        assert_eq!(to_text("x<script>never closed"), "x");
        assert_eq!(to_text("x<!-- never closed"), "x");
        assert_eq!(to_text("<br/>a<br />b"), "a\nb");
    }

    #[test]
    fn entities_decode_and_unknown_ones_stay() {
        assert_eq!(
            decode("&lt;a&gt; &quot;b&quot; &#39;c&#39;"),
            "<a> \"b\" 'c'"
        );
        assert_eq!(
            decode("&bogus; & &#xZZ; &#99999999;"),
            "&bogus; & &#xZZ; &#99999999;"
        );
        assert_eq!(decode("R&amp;D &mdash; ok&hellip;"), "R&D — ok…");
    }

    proptest! {
        #[test]
        fn any_input_converts_without_a_panic(page in ".{0,200}") {
            let _ = to_text(&page);
        }

        #[test]
        fn plain_text_survives(words in proptest::collection::vec("[a-z]{1,8}", 1..10)) {
            let text = words.join(" ");
            prop_assert_eq!(to_text(&text), text);
        }
    }
}
