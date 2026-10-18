//! Minimal iCalendar (RFC 5545) subset parser: VEVENT only, std only.
//!
//! Supported datetime forms for DTSTART/DTEND/DUE (DTEND preferred):
//! * `...Z` — UTC.
//! * `...±HHMM` / `...±HHMMSS` — fixed UTC offset.
//! * floating local time — interpreted as UTC (documented limitation).
//! * `TZID=` — accepted only for UTC aliases (`UTC`, `GMT`, `Etc/UTC`) or
//!   numeric-offset forms (`GMT+5`, `UTC-0500`); any other named zone fails
//!   closed with [`ParseError::UnsupportedTimezone`] because there is no
//!   tz database in this crate (honest gap, see API.md).
//!
//! DATE (all-day) values yield `all_day = true` at 00:00 UTC.
//!
//! VTODO / VJOURNAL components are skipped, never returned.

use std::fmt;

/// Epoch seconds for 2026-01-15T10:00:00Z, used across tests.
#[cfg(test)]
pub(crate) const T_JAN15_10Z: i64 = 1768471200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Malformed(String),
    UnsupportedTimezone(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(m) => write!(f, "malformed iCalendar data: {m}"),
            Self::UnsupportedTimezone(z) => {
                write!(
                    f,
                    "unsupported timezone '{z}': named zones need a tz database"
                )
            }
        }
    }
}

/// One parsed VEVENT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VEvent {
    pub uid: String,
    pub summary: String,
    pub description: String,
    pub start_utc: i64,
    pub end_utc: i64,
    pub all_day: bool,
}

/// Unfold RFC 5545 continuation lines (a CRLF followed by SP/HTAB).
pub(crate) fn unfold(input: &str) -> Result<Vec<String>, ParseError> {
    if input
        .bytes()
        .any(|b| b < 0x09 || (b > 0x0d && b < 0x20) || b == 0x7f)
    {
        return Err(ParseError::Malformed(
            "control characters are not valid iCalendar text".into(),
        ));
    }
    let mut lines: Vec<String> = Vec::new();
    // Normalise CRLF; a bare CR is rejected below.
    let norm = input.replace("\r\n", "\n");
    if norm.contains('\r') {
        return Err(ParseError::Malformed("bare CR in input".into()));
    }
    for raw in norm.split('\n') {
        if raw.is_empty() {
            continue;
        }
        if raw.starts_with(' ') || raw.starts_with('\t') {
            let tail = raw.trim_start_matches([' ', '\t']);
            match lines.last_mut() {
                Some(prev) => prev.push_str(tail),
                None => {
                    return Err(ParseError::Malformed(
                        "continuation line without a parent".into(),
                    ))
                }
            }
        } else {
            if raw.len() > 4096 {
                return Err(ParseError::Malformed("content line too long".into()));
            }
            lines.push(raw.to_string());
        }
    }
    Ok(lines)
}

#[derive(Debug, Clone)]
pub(crate) struct Prop {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

fn parse_line(line: &str) -> Result<Prop, ParseError> {
    let (left, value) = line.split_once(':').ok_or_else(|| {
        ParseError::Malformed(format!("content line without ':': '{}'", trunc(line, 48)))
    })?;
    let mut segs = left.split(';');
    let name = segs.next().unwrap_or("").trim().to_ascii_uppercase();
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(ParseError::Malformed(format!("bad property name '{name}'")));
    }
    let mut params = Vec::new();
    for seg in segs {
        if let Some((k, v)) = seg.split_once('=') {
            params.push((
                k.trim().to_ascii_uppercase(),
                v.trim().trim_matches('"').to_string(),
            ));
        } else {
            return Err(ParseError::Malformed(format!("bad parameter '{seg}'")));
        }
    }
    Ok(Prop {
        name,
        params,
        value: value.to_string(),
    })
}

/// Truncate to `n` characters. Byte slicing untrusted server text panics on
/// non-char boundaries, so error excerpts must use this instead.
fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn param<'a>(p: &'a Prop, key: &str) -> Option<&'a str> {
    p.params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Gregorian civil date -> days since 1970-01-01 (Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days-in-month check with leap years.
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if d > dim {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9).rem_euclid(12);
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// Civil date -> days since epoch, inverse (for UTC request formatting).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format epoch as CalDAV `time-range` UTC stamp `YYYYMMDDTHHMMSSZ`.
pub(crate) fn format_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86400);
    let secs = epoch.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn digits(s: &str, n: usize) -> Option<i64> {
    if s.len() != n || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Parse an iCalendar DATE-TIME / DATE value into epoch seconds.
///
/// Returns `(epoch, all_day)`.
pub(crate) fn parse_datetime(prop: &Prop) -> Result<(i64, bool), ParseError> {
    let v = prop.value.trim();
    // DATE value: YYYYMMDD (8 chars, all digits).
    if param(prop, "VALUE")
        .map(|s| s.eq_ignore_ascii_case("DATE"))
        .unwrap_or(false)
        || (v.len() == 8 && v.bytes().all(|b| b.is_ascii_digit()))
    {
        // Byte slices below require ASCII; multibyte input must fail closed.
        if v.len() != 8 || !v.is_ascii() {
            return Err(ParseError::Malformed(format!("bad DATE value '{v}'")));
        }
        let (y, m, d) = (
            digits(&v[0..4], 4),
            digits(&v[4..6], 2),
            digits(&v[6..8], 2),
        );
        match (y, m, d) {
            (Some(y), Some(m), Some(d)) => {
                let days = days_from_civil(y, m, d)
                    .ok_or_else(|| ParseError::Malformed(format!("bad DATE '{v}'")))?;
                return Ok((days * 86400, true));
            }
            _ => return Err(ParseError::Malformed(format!("bad DATE '{v}'"))),
        }
    }

    // DATE-TIME: YYYYMMDDTHHMMSS with optional Z / numeric offset suffix.
    let (date_part, time_part) = v
        .split_once('T')
        .ok_or_else(|| ParseError::Malformed(format!("bad DATE-TIME '{}'", trunc(v, 32))))?;
    if date_part.len() != 8 || !date_part.is_ascii() {
        return Err(ParseError::Malformed(format!("bad DATE-TIME date '{v}'")));
    }
    let (y, mo, d) = (
        digits(&date_part[0..4], 4),
        digits(&date_part[4..6], 2),
        digits(&date_part[6..8], 2),
    );
    let (y, mo, d) = match (y, mo, d) {
        (Some(y), Some(mo), Some(d)) => (y, mo, d),
        _ => return Err(ParseError::Malformed(format!("bad DATE-TIME date '{v}'"))),
    };
    let days = days_from_civil(y, mo, d)
        .ok_or_else(|| ParseError::Malformed(format!("bad date '{v}'")))?;

    // Split HHMMSS from trailing Z / offset.
    let (clock, suffix) = if let Some(stripped) = time_part
        .strip_suffix('Z')
        .or_else(|| time_part.strip_suffix('z'))
    {
        (stripped, "Z")
    } else if let Some(i) = time_part.find(['+', '-']) {
        time_part.split_at(i)
    } else {
        (time_part, "")
    };
    let _ = suffix;
    if clock.len() != 6 || !clock.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseError::Malformed(format!("bad DATE-TIME clock '{v}'")));
    }
    let (hh, mm, ss) = (
        digits(&clock[0..2], 2).unwrap(),
        digits(&clock[2..4], 2).unwrap(),
        digits(&clock[4..6], 2).unwrap(),
    );
    if hh > 23 || mm > 59 || ss > 60 {
        return Err(ParseError::Malformed(format!("bad DATE-TIME clock '{v}'")));
    }
    let naive = days * 86400 + hh * 3600 + mm * 60 + ss;

    // Determine the UTC offset to subtract.
    let offset: i64 = if time_part.ends_with('Z') || time_part.ends_with('z') {
        if param(prop, "TZID").is_some() {
            return Err(ParseError::Malformed(
                "DATE-TIME has both Z suffix and TZID".into(),
            ));
        }
        0
    } else if let Some(i) = time_part.find(['+', '-']) {
        parse_offset_suffix(&time_part[i..])?
    } else if let Some(tzid) = param(prop, "TZID") {
        tzid_to_offset(tzid)?
    } else {
        0 // floating: interpreted as UTC (documented)
    };
    Ok((naive - offset, false))
}

fn parse_offset_suffix(s: &str) -> Result<i64, ParseError> {
    // Forms: ±HHMM or ±HHMMSS.
    if !s.is_ascii() || (s.len() != 5 && s.len() != 7) {
        return Err(ParseError::Malformed(format!("bad UTC offset '{s}'")));
    }
    let sign = match s.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return Err(ParseError::Malformed(format!("bad UTC offset '{s}'"))),
    };
    let hh = digits(&s[1..3], 2)
        .ok_or_else(|| ParseError::Malformed(format!("bad UTC offset '{s}'")))?;
    let mm = digits(&s[3..5], 2)
        .ok_or_else(|| ParseError::Malformed(format!("bad UTC offset '{s}'")))?;
    let ss = if s.len() == 7 {
        digits(&s[5..7], 2).ok_or_else(|| ParseError::Malformed(format!("bad UTC offset '{s}'")))?
    } else {
        0
    };
    if hh > 23 || mm > 59 || ss > 60 {
        return Err(ParseError::Malformed(format!("bad UTC offset '{s}'")));
    }
    Ok(sign * (hh * 3600 + mm * 60 + ss))
}

/// Resolve TZID without a tz database: UTC aliases and numeric offsets only.
fn tzid_to_offset(tzid: &str) -> Result<i64, ParseError> {
    let t = tzid.trim();
    if t.eq_ignore_ascii_case("UTC")
        || t.eq_ignore_ascii_case("GMT")
        || t.eq_ignore_ascii_case("Etc/UTC")
        || t.eq_ignore_ascii_case("Etc/GMT")
    {
        return Ok(0);
    }
    // GMT+5, UTC-0500, GMT+0530 …
    let up = t.to_ascii_uppercase();
    let rest = up
        .strip_prefix("GMT")
        .or_else(|| up.strip_prefix("UTC"))
        .ok_or_else(|| ParseError::UnsupportedTimezone(tzid.to_string()))?;
    if rest.is_empty() {
        return Ok(0);
    }
    // Accept H, HH, HMM, HHMM with sign.
    let (sign, dig) = match rest.as_bytes()[0] {
        b'+' => (1i64, &rest[1..]),
        b'-' => (-1i64, &rest[1..]),
        _ => return Err(ParseError::UnsupportedTimezone(tzid.to_string())),
    };
    if !dig.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseError::UnsupportedTimezone(tzid.to_string()));
    }
    let (hh, mm) = match dig.len() {
        1 | 2 => (dig.parse::<i64>().unwrap_or(99), 0),
        3 => (
            dig[..1].parse::<i64>().unwrap_or(99),
            dig[1..].parse::<i64>().unwrap_or(99),
        ),
        4 => (
            dig[..2].parse::<i64>().unwrap_or(99),
            dig[2..].parse::<i64>().unwrap_or(99),
        ),
        _ => return Err(ParseError::UnsupportedTimezone(tzid.to_string())),
    };
    if hh > 23 || mm > 59 {
        return Err(ParseError::UnsupportedTimezone(tzid.to_string()));
    }
    Ok(sign * (hh * 3600 + mm * 60))
}

/// Unescape TEXT values (\\, \;, \,, \n/\N).
pub(crate) fn unescape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(x) => out.push(x),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse every VEVENT in a VCALENDAR string. VTODO/VJOURNAL are skipped.
pub(crate) fn parse_vevents(input: &str) -> Result<Vec<VEvent>, ParseError> {
    let lines = unfold(input)?;
    let mut events = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let prop = parse_line(&lines[i])?;
        if prop.name == "BEGIN" && prop.value.eq_ignore_ascii_case("VEVENT") {
            events.push(parse_one_event(&lines, &mut i)?);
        } else if prop.name == "BEGIN"
            && (prop.value.eq_ignore_ascii_case("VTODO")
                || prop.value.eq_ignore_ascii_case("VJOURNAL"))
        {
            skip_component(&lines, &mut i)?;
        } else {
            i += 1;
        }
    }
    Ok(events)
}

fn skip_component(lines: &[String], i: &mut usize) -> Result<(), ParseError> {
    // lines[*i] is BEGIN:X; advance past matching END:X with nesting depth.
    let start = parse_line(&lines[*i])?;
    let want = start.value.to_ascii_uppercase();
    let mut depth = 1usize;
    *i += 1;
    while *i < lines.len() {
        let p = parse_line(&lines[*i])?;
        if p.name == "BEGIN" && p.value.eq_ignore_ascii_case(&want) {
            depth += 1;
        } else if p.name == "END" && p.value.eq_ignore_ascii_case(&want) {
            depth -= 1;
            if depth == 0 {
                *i += 1;
                return Ok(());
            }
        }
        *i += 1;
    }
    Err(ParseError::Malformed("unterminated component".into()))
}

fn parse_one_event(lines: &[String], i: &mut usize) -> Result<VEvent, ParseError> {
    // lines[*i] == BEGIN:VEVENT
    *i += 1;
    let mut uid: Option<String> = None;
    let mut summary = String::new();
    let mut description = String::new();
    let mut start: Option<(i64, bool)> = None;
    let mut end: Option<(i64, bool)> = None;
    let mut due: Option<(i64, bool)> = None;
    let mut duration_secs: Option<i64> = None;
    let mut closed = false;
    while *i < lines.len() {
        let p = parse_line(&lines[*i])?;
        match p.name.as_str() {
            "END" if p.value.eq_ignore_ascii_case("VEVENT") => {
                closed = true;
                *i += 1;
                break;
            }
            "BEGIN" => {
                // Nested component (e.g. VALARM): skip it.
                skip_component(lines, i)?;
                continue;
            }
            "UID" => uid = Some(p.value.trim().to_string()),
            "SUMMARY" => summary = unescape_text(&p.value),
            "DESCRIPTION" => description = unescape_text(&p.value),
            "DTSTART" => start = Some(parse_datetime(&p)?),
            "DTEND" => end = Some(parse_datetime(&p)?),
            "DUE" => due = Some(parse_datetime(&p)?),
            "DURATION" => duration_secs = Some(parse_duration(&p.value)?),
            _ => {}
        }
        *i += 1;
    }
    if !closed {
        return Err(ParseError::Malformed("unterminated VEVENT".into()));
    }
    let uid = uid
        .filter(|u| !u.is_empty())
        .ok_or_else(|| ParseError::Malformed("VEVENT without UID".into()))?;
    let (start_utc, start_allday) =
        start.ok_or_else(|| ParseError::Malformed("VEVENT without DTSTART".into()))?;
    let (end_utc, all_day) = match (end, due, duration_secs) {
        (Some((e, aday)), _, _) => (e, aday || start_allday),
        (None, Some((d, aday)), _) => (d, aday || start_allday),
        (None, None, Some(dur)) => (start_utc.saturating_add(dur), start_allday),
        (None, None, None) => {
            if start_allday {
                (start_utc.saturating_add(86400), true)
            } else {
                (start_utc, false)
            }
        }
    };
    if end_utc < start_utc {
        return Err(ParseError::Malformed("VEVENT ends before it starts".into()));
    }
    Ok(VEvent {
        uid,
        summary,
        description,
        start_utc,
        end_utc,
        all_day,
    })
}

fn parse_duration(s: &str) -> Result<i64, ParseError> {
    // PnDTnHnMnS subset (weeks/days/hours/minutes/seconds). Negative
    // durations are rejected: callers treat end<start as malformed anyway,
    // and silently flipping the sign would invert the server's meaning.
    let s = s.trim();
    if s.starts_with('-') {
        return Err(ParseError::Malformed(format!("negative DURATION '{s}'")));
    }
    let s = s
        .strip_prefix('P')
        .or_else(|| s.strip_prefix('p'))
        .ok_or_else(|| ParseError::Malformed(format!("bad DURATION '{s}'")))?;
    if s.is_empty() {
        return Err(ParseError::Malformed("empty DURATION".into()));
    }
    let (date_p, time_p) = match s.split_once(['T', 't']) {
        Some((d, t)) => (d, t),
        None => (s, ""),
    };
    let mut total: i64 = 0;
    // checked_* below: server-supplied components like P1000000000000000W
    // must yield Malformed, never wrap (release) or panic (debug).
    let mut bump = |n: i64, unit: i64| -> Result<(), ParseError> {
        let bad = || ParseError::Malformed(format!("bad DURATION '{s}'"));
        let add = n.checked_mul(unit).ok_or_else(bad)?;
        total = total.checked_add(add).ok_or_else(bad)?;
        Ok(())
    };
    let mut num = String::new();
    for c in date_p.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: i64 = num
                .parse()
                .map_err(|_| ParseError::Malformed(format!("bad DURATION '{s}'")))?;
            num.clear();
            match c {
                'W' | 'w' => bump(n, 7 * 86400)?,
                'D' | 'd' => bump(n, 86400)?,
                _ => return Err(ParseError::Malformed(format!("bad DURATION '{s}'"))),
            }
        }
    }
    for c in time_p.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: i64 = num
                .parse()
                .map_err(|_| ParseError::Malformed(format!("bad DURATION '{s}'")))?;
            num.clear();
            match c {
                'H' | 'h' => bump(n, 3600)?,
                'M' | 'm' => bump(n, 60)?,
                'S' | 's' => bump(n, 1)?,
                _ => return Err(ParseError::Malformed(format!("bad DURATION '{s}'"))),
            }
        }
    }
    if !num.is_empty() {
        return Err(ParseError::Malformed(format!("bad DURATION '{s}'")));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_offset_and_floating_to_epoch() {
        // 2026-01-15T10:00:00Z
        let p = Prop {
            name: "DTSTART".into(),
            params: vec![],
            value: "20260115T100000Z".into(),
        };
        assert_eq!(parse_datetime(&p).unwrap(), (T_JAN15_10Z, false));

        // Same instant as +00 offset arithmetic: 05:00-0500 == 10:00Z.
        let p = Prop {
            name: "DTSTART".into(),
            params: vec![],
            value: "20260115T050000-0500".into(),
        };
        assert_eq!(parse_datetime(&p).unwrap(), (T_JAN15_10Z, false));

        // Floating time is read as UTC.
        let p = Prop {
            name: "DTSTART".into(),
            params: vec![],
            value: "20260115T100000".into(),
        };
        assert_eq!(parse_datetime(&p).unwrap(), (T_JAN15_10Z, false));

        // TZID UTC alias works.
        let p = Prop {
            name: "DTSTART".into(),
            params: vec![("TZID".into(), "UTC".into())],
            value: "20260115T100000".into(),
        };
        assert_eq!(parse_datetime(&p).unwrap(), (T_JAN15_10Z, false));

        // Named zone without a tz database fails closed.
        let p = Prop {
            name: "DTSTART".into(),
            params: vec![("TZID".into(), "America/New_York".into())],
            value: "20260115T100000".into(),
        };
        assert!(matches!(
            parse_datetime(&p).unwrap_err(),
            ParseError::UnsupportedTimezone(_)
        ));

        // Impossible dates fail.
        let p = Prop {
            name: "DTSTART".into(),
            params: vec![],
            value: "20260230T100000Z".into(),
        };
        assert!(matches!(
            parse_datetime(&p).unwrap_err(),
            ParseError::Malformed(_)
        ));
    }

    #[test]
    fn all_day_date_values() {
        let p = Prop {
            name: "DTSTART".into(),
            params: vec![("VALUE".into(), "DATE".into())],
            value: "20260115".into(),
        };
        let (epoch, all_day) = parse_datetime(&p).unwrap();
        assert!(all_day);
        assert_eq!(epoch, T_JAN15_10Z - 10 * 3600);
    }

    #[test]
    fn skips_todo_and_journal() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
            BEGIN:VEVENT\r\nUID:e1\r\nSUMMARY:A\r\nDTSTART:20260115T100000Z\r\nDTEND:20260115T110000Z\r\nEND:VEVENT\r\n\
            BEGIN:VTODO\r\nUID:t1\r\nSUMMARY:T\r\nEND:VTODO\r\n\
            BEGIN:VJOURNAL\r\nUID:j1\r\nSUMMARY:J\r\nEND:VJOURNAL\r\nEND:VCALENDAR\r\n";
        let ev = parse_vevents(ics).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].uid, "e1");
    }

    #[test]
    fn rejects_garbage_and_missing_uid() {
        assert!(matches!(
            parse_vevents("THIS IS NOT ICAL\r\n").unwrap_err(),
            ParseError::Malformed(_)
        ));
        assert!(matches!(
            parse_vevents("BEGIN:VEVENT\r\nSUMMARY:x\r\nEND:VEVENT\r\n").unwrap_err(),
            ParseError::Malformed(_)
        ));
        assert!(matches!(
            parse_vevents("BEGIN:VEVENT\r\nUID:x\r\n").unwrap_err(),
            ParseError::Malformed(_)
        ));
        assert!(matches!(
            parse_vevents("no-colon-here\r\n").unwrap_err(),
            ParseError::Malformed(_)
        ));
    }

    #[test]
    fn utc_roundtrip_format() {
        assert_eq!(format_utc(T_JAN15_10Z), "20260115T100000Z");
    }

    #[test]
    fn hostile_byte_slicing_inputs_fail_closed_without_panicking() {
        // Regression: error excerpts and fixed-width field slices used byte
        // indexing on untrusted server text, panicking on non-char
        // boundaries. Each case below panics on the old code (byte offsets
        // hand-checked to split a multibyte char) and must now return
        // Malformed. Never panic.
        assert!(matches!(
            parse_vevents("no-colon-here-Xµµµµµµµµµµµµµµµµµµµµµµµµµµµµµµµµµµµµµµµµµ\r\n")
                .unwrap_err(),
            ParseError::Malformed(_)
        ));
        for value in [
            "20ࠀ011T100000Z",      // 3-byte char split inside the date part
            "20260115T100000+aé0", // 2-byte char split inside the UTC offset
        ] {
            let p = Prop {
                name: "DTSTART".into(),
                params: vec![],
                value: value.into(),
            };
            assert!(
                matches!(parse_datetime(&p).unwrap_err(), ParseError::Malformed(_)),
                "value {value:?} must fail closed"
            );
        }
        let p = Prop {
            name: "DTSTART".into(),
            params: vec![("VALUE".into(), "DATE".into())],
            value: "202µ615".into(),
        };
        assert!(matches!(
            parse_datetime(&p).unwrap_err(),
            ParseError::Malformed(_)
        ));
    }

    #[test]
    fn duration_overflow_and_sign_fail_closed() {
        // 1e15 weeks overflows i64 multiplication: Malformed, not wrap/panic.
        assert!(matches!(
            parse_duration("P1000000000000000W").unwrap_err(),
            ParseError::Malformed(_)
        ));
        // A leading '-' used to be silently dropped, inverting the meaning.
        assert!(matches!(
            parse_duration("-P1D").unwrap_err(),
            ParseError::Malformed(_)
        ));
        // Sane values still parse.
        assert_eq!(parse_duration("P1W").unwrap(), 7 * 86400);
        assert_eq!(parse_duration("P1DT2H30M").unwrap(), 86400 + 9000);
    }
}
