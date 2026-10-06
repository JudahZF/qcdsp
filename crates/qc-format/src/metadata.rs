//! Byte-preserving edits of `ModelRepo.xml` and `moduleReport.xml`.
//!
//! The functions work on the decrypted XML text. They locate elements with a
//! small tokenizer that checks tag nesting, and they change only the bytes of
//! the element being edited, so every other byte of the file stays as it was.
//! Comments, CDATA, processing instructions and DOCTYPE are skipped.
//!
//! ModelRepo entries are `<Model blob=… id=… name=…>`; moduleReport entries
//! are `<Module blob_name=… id=… name=…>`. Some blob names appear in several
//! report entries, so match on `id` when editing.

use std::ops::Range;

use crate::{FormatError, Result};

/// An element's span in a document: start tag through end tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element<'a> {
    pub range: Range<usize>,
    pub text: &'a str,
}

/// Every element named `tag`, in document order. Fails on malformed nesting.
pub fn elements<'a>(doc: &'a str, tag: &str) -> Result<Vec<Element<'a>>> {
    if let Some((offset, _)) = doc
        .char_indices()
        .find(|(_, character)| !is_xml_char(*character))
    {
        return Err(xml_error(offset, "character is not allowed in XML 1.0"));
    }
    let mut found = Vec::new();
    let mut open: Vec<(&str, usize)> = Vec::new();
    let mut root_seen = false;
    let mut doctype_seen = false;
    for token in tokens(doc)? {
        if matches!(token, Token::Start { .. }) && open.is_empty() {
            if root_seen {
                return Err(FormatError::Xml(
                    "document has multiple root elements".into(),
                ));
            }
            root_seen = true;
        }
        match token {
            Token::Text { range } => {
                let text = &doc[range.clone()];
                if open.is_empty() {
                    let text = if range.start == 0 {
                        text.trim_start_matches('\u{feff}')
                    } else {
                        text
                    };
                    if !text
                        .chars()
                        .all(|character| matches!(character, ' ' | '\t' | '\r' | '\n'))
                    {
                        return Err(xml_error(range.start, "text outside the root element"));
                    }
                } else {
                    if text.contains("]]>") {
                        return Err(xml_error(range.start, "CDATA terminator in text"));
                    }
                    unescape(text)?;
                }
            }
            Token::Cdata { range } => {
                if open.is_empty() {
                    return Err(xml_error(range.start, "CDATA outside the root element"));
                }
            }
            Token::Doctype { range } => {
                if root_seen || doctype_seen {
                    return Err(xml_error(range.start, "misplaced or repeated DOCTYPE"));
                }
                doctype_seen = true;
            }
            Token::Start {
                name,
                range,
                self_closing: true,
            } => {
                if name == tag {
                    found.push(Element {
                        text: &doc[range.clone()],
                        range,
                    });
                }
            }
            Token::Start {
                name,
                range,
                self_closing: false,
            } => open.push((name, range.start)),
            Token::End { name, range } => {
                let (open_name, start) = open
                    .pop()
                    .ok_or_else(|| xml_error(range.start, "end tag without start tag"))?;
                if open_name != name {
                    return Err(xml_error(range.start, "end tag does not match start tag"));
                }
                if name == tag {
                    found.push(Element {
                        range: start..range.end,
                        text: &doc[start..range.end],
                    });
                }
            }
        }
    }
    if let Some((_, start)) = open.pop() {
        return Err(xml_error(start, "element is not closed"));
    }
    if !root_seen {
        return Err(FormatError::Xml("document has no root element".into()));
    }
    found.sort_by_key(|e| e.range.start);
    Ok(found)
}

/// The single `tag` element whose `attr` equals `value`. Fails when more
/// than one element matches, so an edit never hits the wrong entry.
pub fn find_element<'a>(
    doc: &'a str,
    tag: &str,
    attr_name: &str,
    value: &str,
) -> Result<Option<Element<'a>>> {
    let mut matches = Vec::new();
    for element in elements(doc, tag)? {
        if attr(element.text, attr_name)?.as_deref() == Some(value) {
            matches.push(element);
        }
    }
    match matches.len() {
        0 | 1 => Ok(matches.pop()),
        n => Err(FormatError::Xml(format!(
            "{n} <{tag}> elements have {attr_name}={value:?}"
        ))),
    }
}

/// The unescaped value of attribute `name` on the element's start tag.
pub fn attr(element: &str, name: &str) -> Result<Option<String>> {
    let tag = start_tag(element)?;
    tag.attrs
        .iter()
        .find(|a| a.name == name)
        .map(|a| unescape(&element[a.value.clone()]))
        .transpose()
}

/// Sets attributes on the element's start tag. Existing attributes keep
/// their position and quote style; new ones are appended. Values are escaped.
pub fn set_attrs(element: &str, edits: &[(&str, &str)]) -> Result<String> {
    let tag = start_tag(element)?;
    let mut replaced: Vec<(Range<usize>, String)> = Vec::new();
    let mut appended = String::new();
    for &(name, value) in edits {
        if !is_name(name) {
            return Err(FormatError::Xml(format!("invalid attribute name {name:?}")));
        }
        let escaped = escape(value)?;
        match tag.attrs.iter().find(|a| a.name == name) {
            Some(a) => replaced.push((a.value.clone(), escaped)),
            None => appended.push_str(&format!(" {name}=\"{escaped}\"")),
        }
    }
    replaced.push((tag.attrs_end..tag.attrs_end, appended));
    replaced.sort_by_key(|(range, _)| range.start);

    let mut out = String::with_capacity(element.len() + 64);
    let mut at = 0;
    for (range, text) in replaced {
        if range.start < at {
            return Err(FormatError::Xml(format!(
                "attribute edited twice in {:?}",
                &element[..tag.end]
            )));
        }
        out.push_str(&element[at..range.start]);
        out.push_str(&text);
        at = range.end;
    }
    out.push_str(&element[at..]);
    Ok(out)
}

/// Copies the `tag` element matching `(attr, value)`, applies `edits`, and
/// inserts the copy right after the source with the same leading whitespace.
/// Fails if, after the edit, any attribute in `unique` is missing from the
/// copy or equals that attribute on an existing `tag` element.
pub fn clone_element(
    doc: &str,
    tag: &str,
    matching: (&str, &str),
    edits: &[(&str, &str)],
    unique: &[&str],
) -> Result<String> {
    let source = required(doc, tag, matching)?;
    let copy = set_attrs(source.text, edits)?;
    let existing = elements(doc, tag)?;
    for &name in unique {
        let value = attr(&copy, name)?
            .ok_or_else(|| FormatError::Xml(format!("clone has no {name} attribute")))?;
        for element in &existing {
            if attr(element.text, name)?.as_deref() == Some(value.as_str()) {
                return Err(FormatError::Xml(format!(
                    "a <{tag}> with {name}={value:?} already exists"
                )));
            }
        }
    }
    let indent = leading_whitespace(doc, source.range.start);
    let mut out = String::with_capacity(doc.len() + indent.len() + copy.len());
    out.push_str(&doc[..source.range.end]);
    out.push_str(indent);
    out.push_str(&copy);
    out.push_str(&doc[source.range.end..]);
    Ok(out)
}

/// Replaces the `tag` element matching `(attr, value)` with `new_element`,
/// which must be one well-formed `tag` element.
pub fn replace_element(
    doc: &str,
    tag: &str,
    matching: (&str, &str),
    new_element: &str,
) -> Result<String> {
    let parsed = elements(new_element, tag)?;
    if parsed.first().map(|e| e.range.clone()) != Some(0..new_element.len()) {
        return Err(FormatError::Xml(format!(
            "replacement is not a single <{tag}> element"
        )));
    }
    let source = required(doc, tag, matching)?;
    Ok([
        &doc[..source.range.start],
        new_element,
        &doc[source.range.end..],
    ]
    .concat())
}

/// Removes the `tag` element matching `(attr, value)` and the whitespace before it.
pub fn remove_element(doc: &str, tag: &str, matching: (&str, &str)) -> Result<String> {
    let source = required(doc, tag, matching)?;
    let start = source.range.start - leading_whitespace(doc, source.range.start).len();
    Ok([&doc[..start], &doc[source.range.end..]].concat())
}

/// Escapes an attribute value, rejecting characters forbidden by XML 1.0.
pub fn escape(value: &str) -> Result<String> {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            c if !is_xml_char(c) => {
                return Err(FormatError::Xml(format!(
                    "character {c:?} is not allowed in XML 1.0"
                )));
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

fn unescape(raw: &str) -> Result<String> {
    if raw.chars().any(|character| !is_xml_char(character)) {
        return Err(FormatError::Xml(
            "character is not allowed in XML 1.0".into(),
        ));
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let semi = rest[amp..]
            .find(';')
            .ok_or_else(|| FormatError::Xml(format!("bad entity in {raw:?}")))?;
        let entity = &rest[amp + 1..amp + semi];
        let c = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => entity
                .strip_prefix("#x")
                .map(|hex| u32::from_str_radix(hex, 16))
                .or_else(|| entity.strip_prefix('#').map(str::parse))
                .and_then(|code| code.ok())
                .and_then(char::from_u32)
                .ok_or_else(|| FormatError::Xml(format!("unknown entity &{entity};")))?,
        };
        if !is_xml_char(c) {
            return Err(FormatError::Xml(format!(
                "entity &{entity}; is not allowed in XML 1.0"
            )));
        }
        out.push(c);
        rest = &rest[amp + semi + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn required<'a>(doc: &'a str, tag: &str, (name, value): (&str, &str)) -> Result<Element<'a>> {
    find_element(doc, tag, name, value)?
        .ok_or_else(|| FormatError::Xml(format!("no <{tag}> with {name}={value:?}")))
}

fn leading_whitespace(doc: &str, end: usize) -> &str {
    let before = &doc[..end];
    &before[before.trim_end_matches([' ', '\t', '\r', '\n']).len()..]
}

fn xml_error(offset: usize, reason: &str) -> FormatError {
    FormatError::Xml(format!("{reason} at byte {offset}"))
}

fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))
        && !name.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '.')
}

fn is_xml_char(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

enum Token<'a> {
    Text {
        range: Range<usize>,
    },
    Cdata {
        range: Range<usize>,
    },
    Doctype {
        range: Range<usize>,
    },
    Start {
        name: &'a str,
        range: Range<usize>,
        self_closing: bool,
    },
    End {
        name: &'a str,
        range: Range<usize>,
    },
}

struct Attr<'a> {
    name: &'a str,
    /// Value bytes between the quotes.
    value: Range<usize>,
}

struct StartTag<'a> {
    name: &'a str,
    attrs: Vec<Attr<'a>>,
    /// Where new attributes go: after the last attribute, before `/>` or `>`.
    attrs_end: usize,
    /// One past the closing `>`.
    end: usize,
}

fn tokens(doc: &str) -> Result<Vec<Token<'_>>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < doc.len() {
        let lt = doc[at..].find('<').map_or(doc.len(), |offset| at + offset);
        if lt > at {
            out.push(Token::Text { range: at..lt });
        }
        if lt == doc.len() {
            break;
        }
        let rest = &doc[lt..];
        let skip_to = |close: &str| {
            rest.find(close)
                .map(|i| lt + i + close.len())
                .ok_or_else(|| xml_error(lt, "unterminated markup"))
        };
        at = if rest.starts_with("<!--") {
            let end = skip_to("-->")?;
            if doc[lt + 4..end - 3].contains("--") {
                return Err(xml_error(lt, "double hyphen in comment"));
            }
            end
        } else if rest.starts_with("<![CDATA[") {
            let end = skip_to("]]>")?;
            out.push(Token::Cdata { range: lt..end });
            end
        } else if rest.starts_with("<?") {
            skip_to("?>")?
        } else if rest.starts_with("<!DOCTYPE") {
            let end = skip_to(">")?;
            out.push(Token::Doctype { range: lt..end });
            end
        } else if rest.starts_with("<!") {
            return Err(xml_error(lt, "unsupported declaration"));
        } else if let Some(body) = rest.strip_prefix("</") {
            let close = body
                .find('>')
                .ok_or_else(|| xml_error(lt, "unterminated end tag"))?;
            let name = body[..close].trim_end();
            if !is_name(name) {
                return Err(xml_error(lt, "invalid end tag"));
            }
            let end = lt + 2 + close + 1;
            out.push(Token::End {
                name,
                range: lt..end,
            });
            end
        } else {
            let tag = parse_start_tag(doc, lt)?;
            let self_closing = doc[..tag.end].ends_with("/>");
            out.push(Token::Start {
                name: tag.name,
                range: lt..tag.end,
                self_closing,
            });
            tag.end
        };
    }
    Ok(out)
}

fn start_tag(element: &str) -> Result<StartTag<'_>> {
    if !element.starts_with('<')
        || element.starts_with("</")
        || element.starts_with("<!")
        || element.starts_with("<?")
    {
        return Err(FormatError::Xml(
            "text does not begin with a start tag".into(),
        ));
    }
    parse_start_tag(element, 0)
}

/// Parses the start tag at `lt` (which points at `<`).
fn parse_start_tag(doc: &str, lt: usize) -> Result<StartTag<'_>> {
    let bytes = doc.as_bytes();
    let name_end = doc[lt + 1..]
        .find(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>')
        .map(|i| lt + 1 + i)
        .ok_or_else(|| xml_error(lt, "unterminated start tag"))?;
    let name = &doc[lt + 1..name_end];
    if !is_name(name) {
        return Err(xml_error(lt, "invalid tag name"));
    }
    let mut attrs = Vec::new();
    let mut at = name_end;
    let mut attrs_end = name_end;
    loop {
        let before_whitespace = at;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        match bytes.get(at) {
            None => return Err(xml_error(lt, "unterminated start tag")),
            Some(b'>') => {
                return Ok(StartTag {
                    name,
                    attrs,
                    attrs_end,
                    end: at + 1,
                });
            }
            Some(b'/') if bytes.get(at + 1) == Some(&b'>') => {
                return Ok(StartTag {
                    name,
                    attrs,
                    attrs_end,
                    end: at + 2,
                });
            }
            Some(_) => {}
        }
        if !attrs.is_empty() && at == before_whitespace {
            return Err(xml_error(at, "attributes must be separated by whitespace"));
        }
        let eq = doc[at..]
            .find('=')
            .map(|i| at + i)
            .ok_or_else(|| xml_error(at, "attribute without value"))?;
        let attr_name = doc[at..eq].trim_end();
        if !is_name(attr_name) || attrs.iter().any(|a: &Attr<'_>| a.name == attr_name) {
            return Err(xml_error(at, "invalid or duplicate attribute name"));
        }
        let mut q = eq + 1;
        while q < bytes.len() && bytes[q].is_ascii_whitespace() {
            q += 1;
        }
        let quote = match bytes.get(q) {
            Some(&c @ (b'"' | b'\'')) => c as char,
            _ => return Err(xml_error(q, "attribute value is not quoted")),
        };
        let close = doc[q + 1..]
            .find(quote)
            .map(|i| q + 1 + i)
            .ok_or_else(|| xml_error(q, "unterminated attribute value"))?;
        if doc[q + 1..close].contains('<') {
            return Err(xml_error(q, "'<' in attribute value"));
        }
        unescape(&doc[q + 1..close])?;
        attrs.push(Attr {
            name: attr_name,
            value: q + 1..close,
        });
        at = close + 1;
        attrs_end = at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "<?xml version=\"1.0\" ?><Models><!-- <Model blob=\"ghost\"/> -->\n  <Category id=\"16\" name=\"Utility\">\n    <Model blob=\"volume\" id=\"16006\" name=\"Volume\">\n      <Padding cpu=\"0.4\" dm_hp=\"64\" sw=\"256\"/>\n      <Parameter name=\"LEVEL\" type=\"float\" units=\"%\"/>\n    </Model>\n    <Model blob='gainblock' id=\"16005\" name=\"Gain &amp; Trim\"/>\n  </Category>\n</Models>\n";

    #[test]
    fn finds_elements_and_reads_attributes() {
        let models = elements(REPO, "Model").unwrap();
        assert_eq!(models.len(), 2, "the commented-out model is skipped");
        assert!(models[0].text.ends_with("</Model>"));
        let gain = find_element(REPO, "Model", "id", "16005").unwrap().unwrap();
        assert_eq!(
            attr(gain.text, "name").unwrap().as_deref(),
            Some("Gain & Trim")
        );
        assert_eq!(
            attr(gain.text, "blob").unwrap().as_deref(),
            Some("gainblock")
        );
        assert_eq!(attr(gain.text, "missing").unwrap(), None);
        assert!(find_element(REPO, "Model", "id", "1").unwrap().is_none());
    }

    #[test]
    fn clone_inserts_edited_copy_and_preserves_other_bytes() {
        let edits = [
            ("blob", "volume_qcdsp"),
            ("id", "16900"),
            ("name", "Volume <qcdsp>"),
            ("tm", "new"),
        ];
        let out = clone_element(REPO, "Model", ("id", "16006"), &edits, &["blob", "id"]).unwrap();
        let source = find_element(REPO, "Model", "id", "16006").unwrap().unwrap();
        let copy = find_element(&out, "Model", "id", "16900").unwrap().unwrap();
        let body = |text: &str| text[text.find("<Padding").unwrap()..].to_string();
        assert_eq!(body(copy.text), body(source.text));
        assert!(copy.text.starts_with(
            "<Model blob=\"volume_qcdsp\" id=\"16900\" name=\"Volume &lt;qcdsp&gt;\" tm=\"new\">"
        ));
        assert_eq!(
            attr(copy.text, "name").unwrap().as_deref(),
            Some("Volume <qcdsp>")
        );
        // Removing the clone restores the original byte for byte.
        assert_eq!(
            remove_element(&out, "Model", ("id", "16900")).unwrap(),
            REPO
        );
    }

    #[test]
    fn clone_refuses_collisions() {
        let err = clone_element(
            REPO,
            "Model",
            ("id", "16006"),
            &[("id", "16005"), ("blob", "x")],
            &["id"],
        );
        assert!(err.is_err());
        let err = clone_element(REPO, "Model", ("id", "16006"), &[("id", "1")], &["blob"]);
        assert!(
            err.is_err(),
            "blob not changed, so it collides with the source"
        );
    }

    #[test]
    fn replace_and_set_attrs() {
        let gain = find_element(REPO, "Model", "id", "16005").unwrap().unwrap();
        let renamed = set_attrs(gain.text, &[("name", "Gain")]).unwrap();
        assert_eq!(
            renamed,
            "<Model blob='gainblock' id=\"16005\" name=\"Gain\"/>"
        );
        let out = replace_element(REPO, "Model", ("id", "16005"), &renamed).unwrap();
        assert_eq!(
            out.len(),
            REPO.len() - "Gain &amp; Trim".len() + "Gain".len()
        );
        assert!(replace_element(REPO, "Model", ("id", "16005"), "<Model/><Model/>").is_err());
        assert!(replace_element(REPO, "Model", ("id", "16005"), "<Module/>").is_err());
        assert!(set_attrs(gain.text, &[("bad name", "x")]).is_err());
        assert!(set_attrs(gain.text, &[("name", "a\u{1}")]).is_err());
        for invalid in ['\u{fffe}', '\u{ffff}'] {
            assert!(escape(&invalid.to_string()).is_err());
        }
        assert!(escape("音 🎛\t\n\r").is_ok());
        assert!(set_attrs(gain.text, &[("name", "a"), ("name", "b")]).is_err());
    }

    #[test]
    fn rejects_malformed_documents_and_ambiguous_matches() {
        for bad in [
            "<a><b></a>",
            "<a>",
            "</a>",
            "<a x=1/>",
            "<a x=\"1\" x=\"2\"/>",
            "<a x=\"1",
            "<!-- open",
            "<a/><extra/>",
            "text<a/>",
            "<a/>text",
            "<![CDATA[text]]><a/>",
            "<a/><![CDATA[text]]>",
            "<!--nothing-->",
            "<a x='1'y='2'/>",
            "<a x='&#0;'/>",
            "<a x='&#xFFFF;'/>",
            "<a>\u{ffff}</a>",
        ] {
            assert!(elements(bad, "a").is_err(), "{bad}");
        }
        let doc = "<r><M id=\"1\" b=\"x\"/><M id=\"2\" b=\"x\"/></r>";
        assert!(find_element(doc, "M", "b", "x").is_err());
        assert!(find_element(doc, "M", "id", "2").unwrap().is_some());
        assert!(attr("<a x=\"&bogus;\"/>", "x").is_err());
        assert_eq!(
            attr("<a x=\"&#65;&#x42;\"/>", "x").unwrap().as_deref(),
            Some("AB")
        );
    }
}
