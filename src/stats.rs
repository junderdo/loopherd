//! `loopherd stats`: how each herd's work flowed, read from the ledger and the
//! mailboxes. Writes nothing and moves no cursor.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow, bail};
use serde::Serialize;

use crate::ledger::{AgentRecord, Ledger};
use crate::report::{self, Body, Entry};
use crate::usage::{self, Usage};

pub struct Query<'a> {
    pub herd: Option<&'a str>,
    /// Only members started at or after this.
    pub since: Option<SystemTime>,
    pub now: SystemTime,
    /// Where Claude keeps transcripts; `None` reads none.
    pub claude_dir: Option<&'a Path>,
}

#[derive(Debug, Serialize)]
pub struct Herd {
    pub herd: String,
    pub summary: Summary,
    /// The lead's tokens since `--since`, its subagents' included.
    pub lead_overhead: Option<Usage>,
    pub retired: Vec<Retired>,
    pub open: Vec<Open>,
}

#[derive(Debug, Serialize)]
pub struct Retired {
    pub name: String,
    pub template: Option<String>,
    pub started: String,
    pub first_done_secs: Option<u64>,
    /// `done` reports: one per review round; `None` when its mailbox is unreadable.
    pub rounds: Option<usize>,
    /// From the last `done` to retirement: waiting on merge.
    pub merge_wait_secs: Option<u64>,
    pub total_secs: Option<u64>,
    pub tokens: Option<Usage>,
}

#[derive(Debug, Serialize)]
pub struct Open {
    pub name: String,
    pub template: Option<String>,
    pub started: String,
    pub so_far_secs: Option<u64>,
    pub tokens: Option<Usage>,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    /// Retired members; open ones aren't summarized.
    pub members: usize,
    pub first_done: Spread,
    pub rounds_median: Option<f64>,
    pub merge_wait: Spread,
    pub total: Spread,
    /// Of the members whose tokens are known.
    pub in_median: Option<u64>,
    pub out_median: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct Spread {
    pub median_secs: Option<u64>,
    pub max_secs: Option<u64>,
}

/// Herds by name, from members with a herd; leads are left out.
pub fn stats(
    ledger: &dyn Ledger,
    reports_dir: &Path,
    query: &Query,
) -> Result<(Vec<Herd>, Vec<String>)> {
    let mut by_herd: BTreeMap<String, Vec<AgentRecord>> = BTreeMap::new();
    let mut leads: BTreeMap<String, Vec<AgentRecord>> = BTreeMap::new();
    let runs = ledger.entries()?.into_values().flat_map(|mut record| {
        let earlier = std::mem::take(&mut record.earlier);
        earlier.into_iter().chain([record])
    });
    for record in runs {
        let Some(herd) = record.herd.clone() else {
            continue;
        };
        if query.herd.is_some_and(|h| h != herd) {
            continue;
        }
        if record.is_lead() {
            leads.entry(herd).or_default().push(record);
            continue;
        }
        if let Some(since) = query.since
            && parse_time(&record.created).is_none_or(|created| created < since)
        {
            continue;
        }
        by_herd.entry(herd).or_default().push(record);
    }
    // A herd with only its lead in the window is shown if the lead spent tokens.
    for herd in leads.keys() {
        by_herd.entry(herd.clone()).or_default();
    }
    let mut warnings = vec![];
    let mut herds = vec![];
    for (herd, mut records) in by_herd {
        let records_were_empty = records.is_empty();
        records.sort_by(|a, b| (&a.created, &a.name).cmp(&(&b.created, &b.name)));
        let mut retired = vec![];
        let mut open = vec![];
        for record in records {
            let started = parse_time(&record.created);
            let retired_at = record.retired_at.as_deref().and_then(parse_time);
            let entries = match report::history(reports_dir, &record.name) {
                Ok((entries, bad)) => {
                    warnings.extend(bad);
                    Some(entries)
                }
                Err(e) => {
                    warnings.push(format!("{e:#}"));
                    None
                }
            };
            // A reused name's mailbox still holds its earlier life's entries.
            let life: Option<Vec<(SystemTime, Entry)>> = entries.map(|entries| {
                entries
                    .into_iter()
                    .filter_map(|e| Some((parse_time(&e.at)?, e)))
                    .filter(|(at, _)| started.is_none_or(|s| *at >= s))
                    // retired_at has whole seconds; mailbox times have nanos.
                    .filter(|(at, _)| retired_at.is_none_or(|r| *at < r + Duration::from_secs(1)))
                    .collect()
            });
            let recorded = life
                .iter()
                .flatten()
                .rev()
                .find_map(|(_, e)| match &e.body {
                    Body::Usage { models } => Some(models.clone()),
                    _ => None,
                });
            let tokens = recorded.or_else(|| {
                let (life, dir) = life.as_ref().zip(query.claude_dir)?;
                let run =
                    usage::sessions(life.iter().map(|(_, e)| e), record.session_id.as_deref());
                usage::of_sessions(dir, &run, None)
            });
            if !record.retired {
                open.push(Open {
                    so_far_secs: started.map(|s| secs_between(s, query.now)),
                    tokens,
                    name: record.name,
                    template: record.template,
                    started: record.created,
                });
                continue;
            }
            let dones: Option<Vec<SystemTime>> = life.as_ref().map(|life| {
                life.iter()
                    .filter(|(_, e)| matches!(e.body, Body::Done { .. }))
                    .map(|(at, _)| *at)
                    .collect()
            });
            let dones = dones.as_deref();
            retired.push(Retired {
                first_done_secs: started
                    .zip(dones.and_then(<[_]>::first))
                    .map(|(s, d)| secs_between(s, *d)),
                rounds: dones.map(<[_]>::len),
                merge_wait_secs: dones
                    .and_then(<[_]>::last)
                    .zip(retired_at)
                    .map(|(d, r)| secs_between(*d, r)),
                total_secs: started.zip(retired_at).map(|(s, r)| secs_between(s, r)),
                tokens,
                name: record.name,
                template: record.template,
                started: record.created,
            });
        }
        let lead_overhead = query.claude_dir.and_then(|dir| {
            let runs = leads.get(&herd).map_or(&[][..], Vec::as_slice);
            let mut sessions: Vec<String> =
                runs.iter().filter_map(|r| r.session_id.clone()).collect();
            let names: BTreeSet<&str> = runs.iter().map(|r| r.name.as_str()).collect();
            for name in names {
                match report::history(reports_dir, name) {
                    Ok((entries, bad)) => {
                        warnings.extend(bad);
                        sessions.extend(usage::sessions(&entries, None));
                    }
                    Err(e) => warnings.push(format!("{e:#}")),
                }
            }
            sessions.sort_unstable();
            sessions.dedup();
            usage::of_sessions(dir, &sessions, query.since)
        });
        let lead_was_busy = lead_overhead.as_ref().is_some_and(|u| !u.is_empty());
        if records_were_empty && !lead_was_busy {
            continue;
        }
        herds.push(Herd {
            herd,
            summary: summarize(&retired),
            lead_overhead,
            retired,
            open,
        });
    }
    Ok((herds, warnings))
}

fn summarize(retired: &[Retired]) -> Summary {
    let spread = |column: fn(&Retired) -> Option<u64>| {
        let mut values: Vec<u64> = retired.iter().filter_map(column).collect();
        values.sort_unstable();
        Spread {
            median_secs: median(&values).map(|m| m.round() as u64),
            max_secs: values.last().copied(),
        }
    };
    let mut rounds: Vec<u64> = retired
        .iter()
        .filter_map(|r| r.rounds.filter(|&n| n > 0))
        .map(|n| n as u64)
        .collect();
    rounds.sort_unstable();
    let token_median = |side: fn(&usage::Tokens) -> u64| {
        let mut values: Vec<u64> = retired
            .iter()
            .filter_map(|r| r.tokens.as_ref())
            .map(|t| side(&usage::total(t)))
            .collect();
        values.sort_unstable();
        median(&values).map(|m| m.round() as u64)
    };
    Summary {
        in_median: token_median(usage::Tokens::read),
        out_median: token_median(|t| t.output),
        members: retired.len(),
        first_done: spread(|r| r.first_done_secs),
        rounds_median: median(&rounds),
        merge_wait: spread(|r| r.merge_wait_secs),
        total: spread(|r| r.total_secs),
    }
}

/// Of sorted `values`; the mean of the middle two for an even count.
fn median(values: &[u64]) -> Option<f64> {
    let mid = values.len() / 2;
    match values.len() {
        0 => None,
        n if n % 2 == 1 => Some(values[mid] as f64),
        _ => Some((values[mid - 1] + values[mid]) as f64 / 2.0),
    }
}

fn secs_between(from: SystemTime, to: SystemTime) -> u64 {
    to.duration_since(from).unwrap_or(Duration::ZERO).as_secs()
}

fn parse_time(at: &str) -> Option<SystemTime> {
    humantime::parse_rfc3339_weak(at).ok()
}

/// `--since`: a date, `2026-11-01`, meaning midnight UTC, or an RFC 3339 time
/// with `Z` or a `±HH:MM` offset.
pub fn parse_since(value: &str) -> Result<SystemTime> {
    parse_moment(
        value,
        "a date (2026-11-01) or RFC 3339 time (2026-11-01T12:00:00Z)",
    )
}

/// [`parse_since`], erring that `value` is not `accepted`.
pub fn parse_moment(value: &str, accepted: &str) -> Result<SystemTime> {
    let invalid = || anyhow!("{value:?} is not {accepted}");
    if value.len() == 10 {
        return humantime::parse_rfc3339(&format!("{value}T00:00:00Z")).map_err(|_| invalid());
    }
    if is_zoneless_time(value) {
        bail!("{value:?} has no zone; add Z or an offset such as +05:00");
    }
    let (local, offset) = split_offset(value).ok_or_else(invalid)?;
    // The weak parser would also take, and ignore, a zone left on `local`.
    if !is_zoneless_time(local) {
        return Err(invalid());
    }
    let at = humantime::parse_rfc3339_weak(local).map_err(|_| invalid())?;
    match offset < 0 {
        true => Ok(at + Duration::from_secs(offset.unsigned_abs())),
        false => Ok(at - Duration::from_secs(offset as u64)),
    }
}

/// `YYYY-MM-DD[T ]hh:mm:ss`, with an optional `.d+` fraction.
fn is_zoneless_time(value: &str) -> bool {
    let b = value.as_bytes();
    let digits = |from: usize, to: usize| b[from..to].iter().all(u8::is_ascii_digit);
    b.len() >= 19
        && digits(0, 4)
        && b[4] == b'-'
        && digits(5, 7)
        && b[7] == b'-'
        && digits(8, 10)
        && matches!(b[10], b'T' | b' ')
        && digits(11, 13)
        && b[13] == b':'
        && digits(14, 16)
        && b[16] == b':'
        && digits(17, 19)
        && match &b[19..] {
            [] => true,
            [b'.', fraction @ ..] => {
                !fraction.is_empty() && fraction.iter().all(u8::is_ascii_digit)
            }
            _ => false,
        }
}

/// The time without its `Z` or `±HH:MM`, and the offset east of UTC in seconds.
fn split_offset(value: &str) -> Option<(&str, i64)> {
    if let Some(local) = value.strip_suffix(['Z', 'z']) {
        return Some((local, 0));
    }
    let split = value.len().checked_sub(6)?;
    let (local, offset) = (value.get(..split)?, value.get(split..)?.as_bytes());
    let sign = match offset[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let two_digits = |at: usize| {
        let pair = &offset[at..at + 2];
        pair.iter()
            .all(u8::is_ascii_digit)
            .then(|| i64::from((pair[0] - b'0') * 10 + pair[1] - b'0'))
    };
    let (hours, minutes) = (two_digits(1)?, two_digits(4)?);
    if offset[3] != b':' || hours > 23 || minutes > 59 {
        return None;
    }
    Some((local, sign * (hours * 3600 + minutes * 60)))
}

pub fn render(herds: &[Herd]) -> String {
    let mut out = String::new();
    for (i, herd) in herds.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("herd {}\n", herd.herd));
        if !herd.retired.is_empty() {
            let mut rows = vec![cells(&[
                "member",
                "template",
                "started (UTC)",
                "first done",
                "rounds",
                "merge wait",
                "total",
                "in",
                "out",
            ])];
            rows.extend(herd.retired.iter().map(|r| {
                vec![
                    r.name.clone(),
                    template(&r.template),
                    minute(&r.started),
                    duration(r.first_done_secs),
                    r.rounds.map_or("-".to_string(), |n| n.to_string()),
                    duration(r.merge_wait_secs),
                    duration(r.total_secs),
                    count(r.tokens.as_ref().map(|t| usage::total(t).read())),
                    count(r.tokens.as_ref().map(|t| usage::total(t).output)),
                ]
            }));
            out.push_str(&table(&rows));
        }
        out.push_str(&format!("  summary: {}\n", summary_line(&herd.summary)));
        let lead = herd.lead_overhead.as_ref().map(usage::total);
        out.push_str(&format!(
            "  lead overhead: in {}, out {}\n",
            count(lead.map(|t| t.read())),
            count(lead.map(|t| t.output)),
        ));
        if !herd.open.is_empty() {
            let mut rows = vec![cells(&[
                "open",
                "template",
                "started (UTC)",
                "so far",
                "in",
                "out",
            ])];
            rows.extend(herd.open.iter().map(|o| {
                vec![
                    o.name.clone(),
                    template(&o.template),
                    minute(&o.started),
                    duration(o.so_far_secs),
                    count(o.tokens.as_ref().map(|t| usage::total(t).read())),
                    count(o.tokens.as_ref().map(|t| usage::total(t).output)),
                ]
            }));
            out.push_str(&table(&rows));
        }
    }
    out
}

fn summary_line(s: &Summary) -> String {
    let spread = |label: &str, spread: &Spread| {
        format!(
            "{label} median {}, max {}",
            duration(spread.median_secs),
            duration(spread.max_secs)
        )
    };
    let rounds = s.rounds_median.map_or("-".to_string(), |m| format!("{m}"));
    format!(
        "{} members; {}; rounds median {rounds}; {}; {}; in median {}; out median {}",
        s.members,
        spread("first done", &s.first_done),
        spread("merge wait", &s.merge_wait),
        spread("total", &s.total),
        count(s.in_median),
        count(s.out_median),
    )
}

fn cells(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|t| t.to_string()).collect()
}

/// Two-space indented, columns padded to their widest cell.
fn table(rows: &[Vec<String>]) -> String {
    let columns = rows.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..columns)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:width$}"))
            .collect();
        out.push_str(&format!("  {}\n", line.join("  ").trim_end()));
    }
    out
}

fn template(template: &Option<String>) -> String {
    template.clone().unwrap_or_else(|| "-".into())
}

/// `2026-11-01 00:00` from an RFC 3339 time.
fn minute(at: &str) -> String {
    match (at.get(..10), at.get(11..16)) {
        (Some(day), Some(minute)) => format!("{day} {minute}"),
        _ => at.to_string(),
    }
}

/// `1d03h`, `2h05m` or `7m`; `-` when unknown.
fn duration(secs: Option<u64>) -> String {
    let Some(secs) = secs else {
        return "-".into();
    };
    let (days, hours, minutes) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h{minutes:02}m"),
        _ => format!("{days}d{hours:02}h"),
    }
}

/// `950`, `12.3k` or `4.5M` tokens; `-` when unknown.
fn count(tokens: Option<u64>) -> String {
    match tokens {
        None => "-".into(),
        Some(n) if n < 1_000 => n.to_string(),
        Some(n) if n < 999_950 => format!("{:.1}k", n as f64 / 1e3),
        Some(n) => format!("{:.1}M", n as f64 / 1e6),
    }
}
