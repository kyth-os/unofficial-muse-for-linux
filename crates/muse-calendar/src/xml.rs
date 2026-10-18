//! Minimal `multistatus` response scanner, std only.
//!
//! CalDAV discovery and REPORT replies are XML; this crate only needs three
//! facts out of them, so a purpose-built scanner replaces an XML library:
//! * the `href` inside `current-user-principal`,
//! * the `href` inside `calendar-home-set`,
//! * per-`response` entries: `href`, `displayname`, whether `resourcetype`
//!   contains a `calendar` collection, and embedded `calendar-data`.
//!
//! Anything structurally off fails closed with [`XmlError`].

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlError(pub String);

impl fmt::Display for XmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed CalDAV XML: {}", self.0)
    }
}

/// Strip any namespace prefix (`D:href` -> `href`).
fn local(tag: &str) -> &str {
    match tag.rsplit_once(':') {
        Some((_, l)) => l,
        None => tag,
    }
}

/// Decode the five predefined XML entities (+ numeric refs).
pub(crate) fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let semi = tail.find(';');
        match semi {
            Some(j) => {
                let ent = &tail[1..j];
                match ent {
                    "amp" => out.push('&'),
                    "lt" => out.push('<'),
                    "gt" => out.push('>'),
                    "quot" => out.push('"'),
                    "apos" => out.push('\''),
                    _ if ent.starts_with('#') => {
                        let n = if let Some(hex) =
                            ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X"))
                        {
                            u32::from_str_radix(hex, 16).ok()
                        } else {
                            ent[1..].parse::<u32>().ok()
                        };
                        match n.and_then(char::from_u32) {
                            Some(c) => out.push(c),
                            None => out.push_str(&tail[..=j]),
                        }
                    }
                    _ => out.push_str(&tail[..=j]),
                }
                rest = &tail[j + 1..];
            }
            None => {
                out.push_str(tail);
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Split raw XML into `(is_close, local_name, inner_text_between_tags)` tokens.
///
/// Attributes are ignored; comments/PIs are skipped. Text segments are
/// returned as `(false, "", text)` pseudo-tokens only when non-blank.
fn tokenize(xml: &str) -> Result<Vec<(bool, String, String)>, XmlError> {
    let bad = || XmlError("unparseable markup".into());
    let mut toks = Vec::new();
    let b = xml.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'<' {
            if xml[i..].starts_with("<!--") {
                let end = xml[i..].find("-->").ok_or_else(bad)? + i + 3;
                i = end;
                continue;
            }
            if xml[i..].starts_with("<?") {
                let end = xml[i..].find("?>").ok_or_else(bad)? + i + 2;
                i = end;
                continue;
            }
            let end = xml[i..].find('>').ok_or_else(bad)? + i;
            let mut tag = xml[i + 1..end].trim().to_string();
            let mut close = false;
            if let Some(stripped) = tag.strip_prefix('/') {
                close = true;
                tag = stripped.trim().to_string();
            }
            let self_close = tag.ends_with('/');
            if self_close {
                tag = tag[..tag.len() - 1].trim().to_string();
            }
            // Drop attributes: cut at first whitespace.
            let name = tag.split_whitespace().next().unwrap_or("").to_string();
            if name.is_empty() {
                return Err(bad());
            }
            let lname = local(&name).to_string();
            toks.push((close, lname.clone(), String::new()));
            if self_close {
                toks.push((true, lname, String::new()));
            }
            i = end + 1;
        } else {
            let end = xml[i..].find('<').map(|j| i + j).unwrap_or(b.len());
            let text = &xml[i..end];
            if !text.trim().is_empty() {
                toks.push((false, String::new(), text.to_string()));
            }
            i = end;
        }
    }
    Ok(toks)
}

/// Find `<outer><href>value</href></outer>` anywhere in the document.
pub(crate) fn inner_href(xml: &str, outer: &str) -> Result<String, XmlError> {
    let toks = tokenize(xml)?;
    let mut i = 0;
    while i < toks.len() {
        if !toks[i].0 && toks[i].1 == outer {
            // Scan forward for the first href before the matching close.
            let mut j = i + 1;
            let mut depth = 1;
            while j < toks.len() {
                let (close, name, _) = &toks[j];
                if !close && name == outer {
                    depth += 1;
                } else if *close && name == outer {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                } else if !close && name == "href" {
                    // Next token should be text then close.
                    if j + 1 < toks.len() && toks[j + 1].1.is_empty() {
                        return Ok(unescape(toks[j + 1].2.trim()));
                    }
                    return Err(XmlError(format!("<{outer}> holds an empty href")));
                }
                j += 1;
            }
            return Err(XmlError(format!("<{outer}> has no href child")));
        }
        i += 1;
    }
    Err(XmlError(format!("<{outer}> not found")))
}

/// One entry of a Depth:1 listing / REPORT reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Response {
    pub href: String,
    pub displayname: String,
    pub is_calendar: bool,
    pub calendar_data: Option<String>,
}

/// Split a multistatus body into per-`response` entries.
pub(crate) fn responses(xml: &str) -> Result<Vec<Response>, XmlError> {
    let toks = tokenize(xml)?;
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if !toks[i].0 && toks[i].1 == "response" {
            // Collect tokens until matching close.
            let mut depth = 1;
            let mut j = i + 1;
            let mut slice: Vec<(bool, String, String)> = Vec::new();
            while j < toks.len() {
                let (close, name, _) = &toks[j];
                if !close && name == "response" {
                    depth += 1;
                } else if *close && name == "response" {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                slice.push(toks[j].clone());
                j += 1;
            }
            if depth != 0 {
                return Err(XmlError("unterminated response element".into()));
            }
            out.push(build_response(&slice)?);
            i = j + 1;
        } else {
            i += 1;
        }
    }
    Ok(out)
}

/// Text of the first `<name>text</name>` in a token slice.
fn first_text(slice: &[(bool, String, String)], name: &str) -> Option<String> {
    let mut k = 0;
    while k < slice.len() {
        if !slice[k].0 && slice[k].1 == name {
            if k + 1 < slice.len() && slice[k + 1].1.is_empty() {
                return Some(unescape(slice[k + 1].2.trim()));
            }
            return Some(String::new());
        }
        k += 1;
    }
    None
}

fn build_response(slice: &[(bool, String, String)]) -> Result<Response, XmlError> {
    let href = first_text(slice, "href").ok_or_else(|| XmlError("response without href".into()))?;
    if href.is_empty() {
        return Err(XmlError("response with empty href".into()));
    }
    let displayname = first_text(slice, "displayname").unwrap_or_default();
    // resourcetype spans from <resourcetype> to </resourcetype>; a calendar
    // collection contains a <...:calendar/> child. Schedule inboxes/outboxes
    // also carry calendar-ish markers on some servers, so exclude those.
    let mut is_calendar = false;
    let mut k = 0;
    while k < slice.len() {
        if !slice[k].0 && slice[k].1 == "resourcetype" {
            let mut m = k + 1;
            let mut depth = 1;
            while m < slice.len() {
                let (close, name, _) = &slice[m];
                if !close && name == "resourcetype" {
                    depth += 1;
                } else if *close && name == "resourcetype" {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                } else if !close && name == "calendar" {
                    is_calendar = true;
                } else if !close && (name == "schedule-inbox" || name == "schedule-outbox") {
                    is_calendar = false;
                    break;
                }
                m += 1;
            }
            break;
        }
        k += 1;
    }
    let calendar_data = first_text(slice, "calendar-data");
    Ok(Response {
        href,
        displayname,
        is_calendar,
        calendar_data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_principal_and_homeset() {
        let xml = r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/</D:href>
            <D:propstat><D:prop><D:current-user-principal><D:href>/p/1/</D:href>
            </D:current-user-principal></D:prop></D:propstat></D:response></D:multistatus>"#;
        assert_eq!(inner_href(xml, "current-user-principal").unwrap(), "/p/1/");
        assert!(inner_href(xml, "calendar-home-set").is_err());
    }

    #[test]
    fn splits_responses_and_flags_calendars() {
        let xml = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
            <D:response><D:href>/h/</D:href><D:propstat><D:prop>
            <D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>
            <D:response><D:href>/h/work/</D:href><D:propstat><D:prop>
            <D:displayname>W &amp; Co</D:displayname>
            <D:resourcetype><D:collection/><C:calendar/></D:resourcetype></D:prop>
            </D:propstat></D:response></D:multistatus>"#;
        let r = responses(xml).unwrap();
        assert_eq!(r.len(), 2);
        assert!(!r[0].is_calendar);
        assert!(r[1].is_calendar);
        assert_eq!(r[1].displayname, "W & Co");
    }

    #[test]
    fn rejects_truncated_xml() {
        assert!(responses("<D:multistatus><D:response>").is_err());
    }
}
