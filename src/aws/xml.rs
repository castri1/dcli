//! Reading awsQuery XML responses (RDS, Redshift): elements read through their direct
//! children, and paginated listings.

use std::str::FromStr;

use super::{Aws, Result, Service};
use crate::util::{parse_rfc3339, xml_blocks, xml_decode};

/// Pages read at most by the list calls (100 items each).
const MAX_PAGES: usize = 20;

/// The direct children of an element's content, as (tag, raw content). Responses nest
/// elements of the same name (a cluster's `Status` and its security groups' `Status`), so
/// fields are read from an element's own children only.
pub fn children(xml: &str) -> Vec<(&str, &str)> {
    let bytes = xml.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(lt) = xml[i..].find('<').map(|p| p + i) {
        let Some(gt) = xml[lt..].find('>').map(|p| p + lt) else { break };
        let tag = &xml[lt + 1..gt];
        i = gt + 1;
        if tag.starts_with(['/', '?', '!']) {
            continue;
        }
        if let Some(empty) = tag.strip_suffix('/') {
            out.push((empty.split_whitespace().next().unwrap_or(""), ""));
            continue;
        }
        let name = tag.split_whitespace().next().unwrap_or("");
        let (open, close) = (format!("<{name}>"), format!("</{name}>"));
        let (mut depth, mut j) = (1usize, i);
        while j < bytes.len() {
            if bytes[j..].starts_with(close.as_bytes()) {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                j += close.len();
            } else if bytes[j..].starts_with(open.as_bytes()) {
                depth += 1;
                j += open.len();
            } else {
                j += 1;
            }
        }
        if j >= bytes.len() {
            break;
        }
        out.push((name, &xml[i..j]));
        i = j + close.len();
    }
    out
}

/// An element, read through its direct children.
pub struct El<'a> {
    kids: Vec<(&'a str, &'a str)>,
}

impl<'a> El<'a> {
    pub fn new(xml: &'a str) -> El<'a> {
        El { kids: children(xml) }
    }

    pub fn raw(&self, tag: &str) -> Option<&'a str> {
        self.kids.iter().find(|(t, _)| *t == tag).map(|(_, v)| *v)
    }

    pub fn text(&self, tag: &str) -> Option<String> {
        self.raw(tag).map(xml_decode).filter(|s| !s.is_empty())
    }

    pub fn string(&self, tag: &str) -> String {
        self.text(tag).unwrap_or_default()
    }

    pub fn flag(&self, tag: &str) -> bool {
        self.raw(tag).is_some_and(|v| v.trim() == "true")
    }

    pub fn num<T: FromStr>(&self, tag: &str) -> Option<T> {
        self.raw(tag)?.trim().parse().ok()
    }

    pub fn time(&self, tag: &str) -> Option<f64> {
        parse_rfc3339(self.raw(tag)?.trim())
    }

    pub fn child(&self, tag: &str) -> Option<El<'a>> {
        self.raw(tag).map(El::new)
    }

    /// The elements of the list `tag`, whatever their member name.
    pub fn list(&self, tag: &str) -> Vec<El<'a>> {
        self.raw(tag).map(|x| children(x).into_iter().map(|(_, v)| El::new(v)).collect()).unwrap_or_default()
    }

    /// The texts of the list `tag`.
    pub fn strings(&self, tag: &str) -> Vec<String> {
        self.raw(tag).map(|x| children(x).into_iter().map(|(_, v)| xml_decode(v)).collect()).unwrap_or_default()
    }

    /// Every direct child with text, as (tag, text).
    pub fn fields(&self) -> Vec<(&'a str, String)> {
        self.kids.iter().map(|(t, v)| (*t, xml_decode(v))).filter(|(_, v)| !v.is_empty()).collect()
    }
}

/// The `{action}Result` element of a response.
pub fn result_of<'a>(xml: &'a str, action: &str) -> El<'a> {
    El::new(xml_blocks(xml, &format!("{action}Result")).into_iter().next().unwrap_or(""))
}

/// Reads every page of `action`, each item of the list `list` through `parse`.
pub fn pages<T>(
    aws: &Aws,
    svc: &Service,
    version: &str,
    action: &str,
    params: &[(&str, &str)],
    list: &str,
    parse: impl Fn(&El) -> Option<T>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    let mut marker: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut page: Vec<(&str, &str)> = params.to_vec();
        if let Some(m) = &marker {
            page.push(("Marker", m));
        }
        let xml = aws.query(svc, action, version, &page)?;
        let result = result_of(&xml, action);
        out.extend(result.list(list).iter().filter_map(&parse));
        marker = result.text("Marker");
        if marker.is_none() {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_skip_nested_namesakes_and_empty_elements() {
        let kids = children("<A><A>in</A></A><B/><C>x &amp; y</C>");
        assert_eq!(kids, [("A", "<A>in</A>"), ("B", ""), ("C", "x &amp; y")]);
        assert_eq!(El::new("<C>a&#x1b;[31mb</C>").string("C"), "ab", "entities are decoded, then sanitized");
        assert!(children("<A>never closed").is_empty());
    }
}
