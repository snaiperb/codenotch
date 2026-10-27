//! API spend adapters — pay-as-you-go cost for the Anthropic and OpenAI platform accounts, as
//! opposed to the subscription limits the other cells show.
//!
//! Both vendors publish an organisation-level cost report behind an *admin* key, which is a
//! different credential from a normal API key and is entered by hand in Settings → API spend:
//!   - Anthropic: GET https://api.anthropic.com/v1/organizations/cost_report?starting_at=…&bucket_width=1d&limit=31
//!                headers x-api-key: <sk-ant-admin…>, anthropic-version: 2023-06-01
//!                reply: { data:[{starting_at, ending_at, results:[{amount:"<decimal string, cents>", currency, description, model?}]}], has_more, next_page }
//!   - OpenAI:    GET https://api.openai.com/v1/organization/costs?start_time=<unix>&bucket_width=1d&limit=31&group_by[]=line_item
//!                header Authorization: Bearer <sk-admin-…>
//!                reply: { data:[{start_time, end_time, results:[{amount:{value:<number, USD>, currency}, line_item}]}], has_more, next_page }
//!   - xAI:       POST https://management-api.x.ai/v1/billing/teams/{team_id}/usage  (management key, Bearer)
//!                body { analyticsRequest:{ timeRange:{startTime:"YYYY-MM-DD HH:MM:SS", endTime, timezone:"Etc/GMT"}, timeUnit:"TIME_UNIT_DAY",
//!                       values:[{name:"usd", aggregation:"AGGREGATION_SUM"}], groupBy:["description"], filters:[] } }
//!                reply: { timeSeries:[{group:["Chat grok-4"], dataPoints:[{timestamp:"…Z", values:[<USD>]}]}], limitReached }
//!                The team id comes from GET /auth/management-keys/validation (teamId / scopeId) when not entered by hand.
//!
//! The cell shows month-to-date spend. With a monthly budget set in Settings the ring fills as a
//! share of that budget; without one the ring stays empty and only the dollar figure is shown.
//! A missing key means absent (no cell). 401/403 means needsAuth (wrong or revoked key). Any
//! other failure keeps the last reading, marked stale, with the reason in the card — the same
//! discipline as the subscription cells: never invent a number.

use crate::usage::{LimitWindow, UsageSnapshot};
use crate::AppState;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

/// Cost reports lag real usage by minutes to hours on both vendors; polling faster buys nothing.
const POLL_SECS: u64 = 900;
/// How long a reading counts as current before the cell dims it.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// One flag per vendor, indexed by `Vendor as usize`: each poller consumes only its own, so one
/// Refresh (tray, a click on a ring, saving Settings → API spend) wakes all three. A single shared
/// flag was swapped back to false by whichever thread looked first, and the other two cells slept
/// on for up to a quarter-hour.
static REFRESH: [std::sync::atomic::AtomicBool; 3] = [
    std::sync::atomic::AtomicBool::new(false),
    std::sync::atomic::AtomicBool::new(false),
    std::sync::atomic::AtomicBool::new(false),
];

pub fn request_refresh() {
    for flag in &REFRESH {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn take_refresh(vendor: Vendor) -> bool {
    REFRESH[vendor as usize].swap(false, std::sync::atomic::Ordering::Relaxed)
}

fn sleep_interruptible(vendor: Vendor, total_secs: u64) {
    for _ in 0..total_secs {
        if take_refresh(vendor) {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vendor {
    Anthropic,
    OpenAi,
    Xai,
}

impl Vendor {
    pub fn id(self) -> &'static str {
        match self {
            Vendor::Anthropic => "anthropic_api",
            Vendor::OpenAi => "openai_api",
            Vendor::Xai => "xai_api",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Vendor::Anthropic => "Anthropic API",
            Vendor::OpenAi => "OpenAI API",
            Vendor::Xai => "xAI API",
        }
    }
    fn store_name(self) -> &'static str {
        match self {
            Vendor::Anthropic => "anthropic_api.json",
            Vendor::OpenAi => "openai_api.json",
            Vendor::Xai => "xai_api.json",
        }
    }
}

fn store_path(v: Vendor) -> std::path::PathBuf {
    crate::config::config_path().with_file_name(v.store_name())
}

pub fn load_persisted(v: Vendor) -> UsageSnapshot {
    std::fs::read_to_string(store_path(v))
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        .map(|mut s| {
            if !s.windows.is_empty() {
                s.status = "stale".into();
            }
            s
        })
        .unwrap_or_default()
}

fn persist(v: Vendor, s: &UsageSnapshot) {
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(store_path(v), t);
    }
}

/// The key and budget for one vendor, read fresh from the config each poll so a change in
/// Settings takes effect on the next reading without a restart.
fn settings(app: &AppHandle, v: Vendor) -> (String, f64) {
    let st = app.state::<AppState>();
    let c = st.cfg.lock().unwrap();
    settings_from(&c, v)
}

fn settings_from(c: &crate::config::Config, v: Vendor) -> (String, f64) {
    match v {
        Vendor::Anthropic => (c.anthropic_admin_key.trim().to_string(), c.anthropic_budget_usd),
        Vendor::OpenAi => (c.openai_admin_key.trim().to_string(), c.openai_budget_usd),
        Vendor::Xai => (c.xai_management_key.trim().to_string(), c.xai_budget_usd),
    }
}

/// xAI needs a team id in the path. Hand-entered wins; otherwise the key describes itself.
fn xai_team_id(app: Option<&AppHandle>, key: &str) -> Result<String, FetchErr> {
    let typed = match app {
        Some(a) => {
            let st = a.state::<AppState>();
            let c = st.cfg.lock().unwrap();
            c.xai_team_id.trim().to_string()
        }
        None => crate::config::load().xai_team_id.trim().to_string(),
    };
    if !typed.is_empty() {
        return Ok(typed);
    }
    let v = get_json(
        agent().get("https://management-api.x.ai/auth/management-keys/validation")
            .set("authorization", &format!("Bearer {key}"))
            .set("user-agent", "codenotch-windows"),
    )?;
    ["scopeId", "teamId"]
        .iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(|x| x.to_string()))
        .ok_or_else(|| FetchErr::Other("management key validated but reports no team id — enter it in Settings → API spend".into()))
}

fn xai_local_ts(secs: u64) -> String {
    let (y, m, d) = ymd_utc(secs);
    let rem = secs % 86_400;
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// xAI's usage report is one time series per description, each dense over the range; fold them
/// into one bucket per day.
fn parse_xai(v: &serde_json::Value) -> Vec<Bucket> {
    let mut out: Vec<Bucket> = Vec::new();
    let Some(series) = v.get("timeSeries").and_then(|s| s.as_array()) else { return out };
    for ts in series {
        let name = ts
            .get("groupLabels")
            .or_else(|| ts.get("group"))
            .and_then(|g| g.as_array())
            .and_then(|g| g.first())
            .and_then(|g| g.as_str())
            .unwrap_or("other")
            .to_string();
        let Some(points) = ts.get("dataPoints").and_then(|p| p.as_array()) else { continue };
        for p in points {
            let start = p.get("timestamp").and_then(|t| t.as_str()).and_then(parse_rfc3339_secs).unwrap_or(0);
            let usd = p.get("values").and_then(|x| x.as_array()).and_then(|x| x.first()).and_then(|x| x.as_f64()).unwrap_or(0.0);
            if usd == 0.0 {
                continue;
            }
            let b = match out.iter_mut().find(|b| b.start_secs == start) {
                Some(b) => b,
                None => {
                    out.push(Bucket { start_secs: start, ..Default::default() });
                    out.last_mut().unwrap()
                }
            };
            b.usd += usd;
            match b.lines.iter_mut().find(|(n, _)| *n == name) {
                Some((_, acc)) => *acc += usd,
                None => b.lines.push((name.clone(), usd)),
            }
        }
    }
    out
}

/// Civil date from a unix timestamp (UTC). Enough calendar for "first of this month" and
/// "first of next month" without pulling in a date crate's timezone tables.
fn ymd_utc(secs: u64) -> (i64, u32, u32) {
    // Howard Hinnant's days-to-civil
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (start of this month, start of next month, start of today) — unix seconds, UTC.
/// Both vendors bucket their daily reports on UTC midnights, so the month is taken in UTC too;
/// the difference from the local calendar is at most a few hours at either end.
fn month_bounds(now_secs: u64) -> (u64, u64, u64) {
    let (y, m, _d) = ymd_utc(now_secs);
    let start = days_from_civil(y, m, 1) as u64 * 86_400;
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let next = days_from_civil(ny, nm, 1) as u64 * 86_400;
    let today = now_secs / 86_400 * 86_400;
    (start, next, today)
}

fn rfc3339_utc(secs: u64) -> String {
    let (y, m, d) = ymd_utc(secs);
    let rem = secs % 86_400;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

fn parse_rfc3339_secs(s: &str) -> Option<u64> {
    // "YYYY-MM-DDTHH:MM:SSZ" — the reports only ever use whole-day UTC buckets
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, l: usize| s.get(a..a + l)?.parse::<u64>().ok();
    let (y, m, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hh, mm, ss) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    Some(days_from_civil(y as i64, m as u32, d as u32) as u64 * 86_400 + hh * 3600 + mm * 60 + ss)
}

pub enum FetchErr {
    NeedsAuth(String),
    Other(String),
}

/// One day's spend plus its per-line breakdown.
#[derive(Default, Debug)]
pub struct Bucket {
    pub start_secs: u64,
    pub usd: f64,
    pub lines: Vec<(String, f64)>,
}

/// Which model a cost line belongs to, for the card's breakdown. Anthropic reports a `model`
/// field; OpenAI only a `line_item` such as "gpt-4o-mini, input" — the part before the comma.
fn line_name(vendor: Vendor, r: &serde_json::Value) -> String {
    match vendor {
        Vendor::Xai => "other".into(),
        Vendor::Anthropic => r
            .get("model")
            .and_then(|m| m.as_str())
            .filter(|m| !m.is_empty())
            .map(|m| m.to_string())
            .or_else(|| r.get("description").and_then(|d| d.as_str()).map(|d| d.to_string()))
            .unwrap_or_else(|| "other".into()),
        Vendor::OpenAi => r
            .get("line_item")
            .and_then(|m| m.as_str())
            .map(|m| m.split(',').next().unwrap_or(m).trim().to_string())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "other".into()),
    }
}

fn parse_buckets(vendor: Vendor, v: &serde_json::Value) -> Vec<Bucket> {
    let mut out = Vec::new();
    let Some(data) = v.get("data").and_then(|d| d.as_array()) else { return out };
    for b in data {
        let start_secs = match vendor {
            Vendor::Anthropic => b.get("starting_at").and_then(|s| s.as_str()).and_then(parse_rfc3339_secs),
            Vendor::OpenAi => b.get("start_time").and_then(|s| s.as_u64()),
            Vendor::Xai => None, // never reaches here: see parse_xai
        }
        .unwrap_or(0);
        let mut bucket = Bucket { start_secs, ..Default::default() };
        if let Some(results) = b.get("results").and_then(|r| r.as_array()) {
            for r in results {
                let usd = match vendor {
                    // decimal string, in cents
                    Vendor::Anthropic => r
                        .get("amount")
                        .and_then(|a| a.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| a.as_f64()))
                        .map(|c| c / 100.0)
                        .unwrap_or(0.0),
                    // {value, currency}, in dollars
                    Vendor::OpenAi => r
                        .get("amount")
                        .and_then(|a| a.get("value"))
                        .and_then(|x| x.as_f64())
                        .unwrap_or(0.0),
                    Vendor::Xai => 0.0,
                };
                bucket.usd += usd;
                let name = line_name(vendor, r);
                match bucket.lines.iter_mut().find(|(n, _)| *n == name) {
                    Some((_, acc)) => *acc += usd,
                    None => bucket.lines.push((name, usd)),
                }
            }
        }
        out.push(bucket);
    }
    out
}

/// One client for every vendor call: no redirects, so an Authorization header can never be
/// replayed to a host other than the one written in this file.
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().redirects(0).timeout(HTTP_TIMEOUT).build()
}

fn get_json(req: ureq::Request) -> Result<serde_json::Value, FetchErr> {
    match req.timeout(HTTP_TIMEOUT).call() {
        Ok(r) => r.into_json::<serde_json::Value>().map_err(|e| FetchErr::Other(format!("bad JSON: {e}"))),
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            Err(FetchErr::NeedsAuth("Admin key rejected (401/403) — check it in Settings → API spend".into()))
        }
        Err(ureq::Error::Status(429, r)) => {
            let ra = r.header("retry-after").unwrap_or("?").to_string();
            Err(FetchErr::Other(format!("HTTP 429, retry-after {ra}")))
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            let short: String = body.chars().take(160).collect();
            Err(FetchErr::Other(format!("HTTP {code} {short}")))
        }
        Err(e) => Err(FetchErr::Other(format!("network: {e}"))),
    }
}

/// Every daily bucket from the start of the month to now, following pagination.
pub fn fetch_month(vendor: Vendor, key: &str, month_start: u64, month_next: u64) -> Result<Vec<Bucket>, FetchErr> {
    if vendor == Vendor::Xai {
        return fetch_month_xai(None, key, month_start, month_next);
    }
    let mut all = Vec::new();
    let mut page: Option<String> = None;
    for _ in 0..4 {
        let req = match vendor {
            Vendor::Anthropic => {
                let mut r = agent().get("https://api.anthropic.com/v1/organizations/cost_report")
                    .query("starting_at", &rfc3339_utc(month_start))
                    .query("ending_at", &rfc3339_utc(month_next))
                    .query("bucket_width", "1d")
                    .query("limit", "31")
                    .query("group_by[]", "description")
                    .set("x-api-key", key)
                    .set("anthropic-version", "2023-06-01")
                    .set("user-agent", "codenotch-windows");
                if let Some(p) = &page {
                    r = r.query("page", p);
                }
                r
            }
            Vendor::OpenAi => {
                let mut r = agent().get("https://api.openai.com/v1/organization/costs")
                    .query("start_time", &month_start.to_string())
                    .query("end_time", &month_next.to_string())
                    .query("bucket_width", "1d")
                    .query("limit", "31")
                    .query("group_by[]", "line_item")
                    .set("authorization", &format!("Bearer {key}"))
                    .set("user-agent", "codenotch-windows");
                if let Some(p) = &page {
                    r = r.query("page", p);
                }
                r
            }
            Vendor::Xai => unreachable!("xAI is fetched by fetch_month_xai"),
        };
        let v = get_json(req)?;
        all.extend(parse_buckets(vendor, &v));
        let more = v.get("has_more").and_then(|m| m.as_bool()).unwrap_or(false);
        page = v.get("next_page").and_then(|p| p.as_str()).map(|p| p.to_string());
        if !more || page.is_none() {
            break;
        }
    }
    Ok(all)
}

fn fetch_month_xai(app: Option<&AppHandle>, key: &str, month_start: u64, month_next: u64) -> Result<Vec<Bucket>, FetchErr> {
    let team = xai_team_id(app, key)?;
    let body = serde_json::json!({
        "analyticsRequest": {
            "timeRange": { "startTime": xai_local_ts(month_start), "endTime": xai_local_ts(month_next), "timezone": "Etc/GMT" },
            "timeUnit": "TIME_UNIT_DAY",
            "values": [ { "name": "usd", "aggregation": "AGGREGATION_SUM" } ],
            "groupBy": [ "description" ],
            "filters": []
        }
    });
    let req = agent().post(&format!("https://management-api.x.ai/v1/billing/teams/{team}/usage"))
        .set("authorization", &format!("Bearer {key}"))
        .set("content-type", "application/json")
        .set("user-agent", "codenotch-windows")
        .timeout(HTTP_TIMEOUT);
    let v = match req.send_string(&body.to_string()) {
        Ok(r) => r.into_json::<serde_json::Value>().map_err(|e| FetchErr::Other(format!("bad JSON: {e}")))?,
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            return Err(FetchErr::NeedsAuth("Management key rejected (401/403) — check it in Settings → API spend".into()))
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            let short: String = body.chars().take(160).collect();
            return Err(FetchErr::Other(format!("HTTP {code} {short}")));
        }
        Err(e) => return Err(FetchErr::Other(format!("network: {e}"))),
    };
    let buckets = parse_xai(&v);
    log_on_change(&XAI_USAGE_TRACE, xai_usage_trace(&v, &buckets));
    Ok(buckets)
}

/// xAI prepaid credit left on the team, in USD. The ledger's `total` is in cents with purchases
/// negative and spend positive, so the balance is its negation.
fn xai_balance(app: Option<&AppHandle>, key: &str) -> Result<f64, FetchErr> {
    let team = xai_team_id(app, key)?;
    let v = get_json(
        agent().get(&format!("https://management-api.x.ai/v1/billing/teams/{team}/prepaid/balance"))
            .set("authorization", &format!("Bearer {key}"))
            .set("user-agent", "codenotch-windows"),
    )?;
    log_on_change(&XAI_BALANCE_TRACE, xai_balance_trace(&v, month_bounds(now_ms() / 1000).0));
    let cents = v
        .get("total")
        .and_then(|t| t.get("val"))
        .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| x.as_f64()))
        .ok_or_else(|| FetchErr::Other("balance reply without total.val".into()))?;
    Ok(-cents / 100.0)
}

static XAI_USAGE_TRACE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static XAI_BALANCE_TRACE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// The xAI traces go to run.log when they change, not on every quarter-hour poll.
fn log_on_change(last: &std::sync::Mutex<String>, line: String) {
    let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
    if *last != line {
        crate::applog(&line);
        *last = line;
    }
}

/// One run.log line per xAI usage reply — enough to tell "xAI has no newer charges yet" from
/// "we dropped them". Nothing secret: counts, one raw timestamp, and the last days with spend.
fn xai_usage_trace(v: &serde_json::Value, buckets: &[Bucket]) -> String {
    let series = v.get("timeSeries").and_then(|s| s.as_array());
    let (mut points, mut unparsed, mut sample) = (0usize, 0usize, String::new());
    for ts in series.into_iter().flatten() {
        for p in ts.get("dataPoints").and_then(|d| d.as_array()).into_iter().flatten() {
            points += 1;
            let t = p.get("timestamp");
            if sample.is_empty() {
                sample = t.map(|t| t.to_string()).unwrap_or_else(|| "none".into()).chars().take(40).collect();
            }
            if t.and_then(|t| t.as_str()).and_then(parse_rfc3339_secs).is_none() {
                unparsed += 1;
            }
        }
    }
    let mut days: Vec<&Bucket> = buckets.iter().filter(|b| b.usd != 0.0).collect();
    days.sort_by_key(|b| b.start_secs);
    let recent = days
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(|b| {
            let day: String = rfc3339_utc(b.start_secs).chars().take(10).collect();
            format!("{day} ${:.4}", b.usd)
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "xai_api: usage — {} series, {points} points, limitReached={}, ts e.g. {sample}, unparsed {unparsed}; last days with spend: {}",
        series.map(|s| s.len()).unwrap_or(0),
        v.get("limitReached").map(|x| x.to_string()).unwrap_or_else(|| "?".into()),
        if recent.is_empty() { "none".to_string() } else { recent }
    )
}

/// One run.log line per balance read: how many ledger entries, what this month's SPEND entries
/// add up to, and the newest three — so a balance that "never moves" shows whether xAI posted
/// anything since.
fn xai_balance_trace(v: &serde_json::Value, month_start: u64) -> String {
    let changes: Vec<&serde_json::Value> = v.get("changes").and_then(|c| c.as_array()).map(|c| c.iter().collect()).unwrap_or_default();
    let ts_of = |c: &serde_json::Value| -> String {
        c.get("createTs")
            .or_else(|| c.get("createTime"))
            .map(|t| t.as_str().map(|s| s.to_string()).unwrap_or_else(|| t.to_string()))
            .unwrap_or_default()
    };
    let usd_of = |c: &serde_json::Value| -> f64 {
        c.get("amount")
            .and_then(|a| a.get("val"))
            .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| x.as_f64()))
            .unwrap_or(0.0)
            / 100.0
    };
    let origin_of = |c: &serde_json::Value| -> String {
        c.get("changeOrigin").and_then(|o| o.as_str()).unwrap_or("?").to_string()
    };
    let month: String = rfc3339_utc(month_start).chars().take(7).collect();
    let spend_month: Vec<&&serde_json::Value> =
        changes.iter().filter(|c| origin_of(c) == "SPEND" && ts_of(c).starts_with(&month)).collect();
    let mut sorted = changes.clone();
    sorted.sort_by_key(|c| ts_of(c));
    let newest = sorted
        .iter()
        .rev()
        .take(3)
        .map(|c| format!("{} {:+.2} @ {}", origin_of(c), usd_of(c), ts_of(c)))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "xai_api: balance — {} ledger entries, SPEND in {month}: {} entries ${:.2}; newest: {}",
        changes.len(),
        spend_month.len(),
        spend_month.iter().map(|c| usd_of(c)).sum::<f64>(),
        if newest.is_empty() { "none".to_string() } else { newest }
    )
}

fn balance_window(usd: f64) -> LimitWindow {
    LimitWindow {
        id: "balance".into(),
        label: "Prepaid balance".into(),
        used: 0.0,
        resets_at: None,
        count: None,
        derived: false,
        group: None,
        text: Some(fmt_usd(usd)),
    }
}

fn fmt_usd(x: f64) -> String {
    let x = x + 0.0; // an empty f64 sum is -0.0, which would print as "$-0.00"
    if x >= 100.0 {
        format!("${x:.0}")
    } else if x >= 10.0 {
        format!("${x:.1}")
    } else {
        format!("${x:.2}")
    }
}

/// Turns a month of buckets into the snapshot the notch draws.
pub fn snapshot_from(buckets: &[Bucket], budget: f64, month_next: u64, today: u64) -> UsageSnapshot {
    let month: f64 = buckets.iter().map(|b| b.usd).sum();
    let today_usd: f64 = buckets.iter().filter(|b| b.start_secs == today).map(|b| b.usd).sum();
    let share = |usd: f64| if budget > 0.0 { (usd / budget).max(0.0) } else { 0.0 };
    let mut windows = vec![
        LimitWindow {
            id: "month".into(),
            label: if budget > 0.0 { format!("This month · budget {}", fmt_usd(budget)) } else { "This month".into() },
            used: share(month),
            resets_at: Some(month_next * 1000),
            count: None,
            derived: false,
            group: None,
            text: Some(fmt_usd(month)),
        },
        LimitWindow {
            id: "today".into(),
            label: "Today".into(),
            used: share(today_usd),
            resets_at: Some((today + 86_400) * 1000),
            count: None,
            derived: false,
            group: None,
            text: Some(fmt_usd(today_usd)),
        },
    ];
    // Without a budget the ring has nothing to fill: keep the windows (the card still lists the
    // figures) but make sure nothing draws a percentage out of them.
    if budget <= 0.0 {
        for w in &mut windows {
            w.used = 0.0;
        }
    }
    // The newest day with any spend, as its own row: a month figure that has stopped moving then
    // says why at a glance (nothing charged since that day) instead of passing for a stuck reading.
    // Sent as the bare UTC day, YYYY-MM-DD — the card writes it out in its own language.
    if let Some(last) = buckets.iter().filter(|b| b.usd > 0.0).map(|b| b.start_secs).max() {
        windows.push(LimitWindow {
            id: "last_charge".into(),
            label: "Last charge".into(),
            used: 0.0,
            resets_at: None,
            count: None,
            derived: false,
            group: None,
            text: Some(rfc3339_utc(last).chars().take(10).collect()),
        });
    }
    // Per-model breakdown for the card, largest first
    let mut lines: Vec<(String, f64)> = Vec::new();
    for b in buckets {
        for (n, usd) in &b.lines {
            match lines.iter_mut().find(|(m, _)| m == n) {
                Some((_, acc)) => *acc += usd,
                None => lines.push((n.clone(), *usd)),
            }
        }
    }
    lines.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let note = lines
        .iter()
        .filter(|(_, usd)| *usd >= 0.005)
        .take(4)
        .map(|(n, usd)| format!("{n} {}", fmt_usd(*usd)))
        .collect::<Vec<_>>()
        .join(" · ");
    UsageSnapshot { status: "ok".into(), windows, fetched_at: now_ms(), note, backoff_until: 0 }
}

fn read_once(app: &AppHandle, vendor: Vendor, prev: &UsageSnapshot) -> UsageSnapshot {
    let (key, budget) = settings(app, vendor);
    if key.is_empty() {
        return UsageSnapshot { status: "absent".into(), ..Default::default() };
    }
    let now = now_ms() / 1000;
    let (start, next, today) = month_bounds(now);
    let fetched = if vendor == Vendor::Xai { fetch_month_xai(Some(app), &key, start, next) } else { fetch_month(vendor, &key, start, next) };
    match fetched {
        Ok(buckets) => {
            let mut snap = snapshot_from(&buckets, budget, next, today);
            if vendor == Vendor::Xai {
                match xai_balance(Some(app), &key) {
                    Ok(usd) => snap.windows.push(balance_window(usd)),
                    Err(FetchErr::NeedsAuth(m)) | Err(FetchErr::Other(m)) => crate::applog(&format!("xai_api: balance — {m}")),
                }
            }
            snap
        }
        Err(FetchErr::NeedsAuth(msg)) => UsageSnapshot { status: "needsAuth".into(), note: msg, fetched_at: now_ms(), ..Default::default() },
        Err(FetchErr::Other(msg)) => {
            let mut s = prev.clone();
            s.status = if s.windows.is_empty() { "error".into() } else { "stale".into() };
            s.note = msg;
            s
        }
    }
}

fn broadcast(app: &AppHandle, vendor: Vendor, snap: UsageSnapshot) {
    {
        let st = app.state::<AppState>();
        let slot = match vendor {
            Vendor::Anthropic => &st.anthropic_api,
            Vendor::OpenAi => &st.openai_api,
            Vendor::Xai => &st.xai_api,
        };
        *slot.lock().unwrap() = snap.clone();
    }
    if snap.status != "absent" {
        persist(vendor, &snap);
    }
    let _ = app.emit(vendor.id(), &snap);
}

pub fn start(app: AppHandle, vendor: Vendor) {
    std::thread::spawn(move || loop {
        let prev = {
            let st = app.state::<AppState>();
            match vendor {
                Vendor::Anthropic => st.anthropic_api.lock().unwrap().clone(),
                Vendor::OpenAi => st.openai_api.lock().unwrap().clone(),
                Vendor::Xai => st.xai_api.lock().unwrap().clone(),
            }
        };
        let snap = read_once(&app, vendor, &prev);
        if snap.status == "error" || snap.status == "stale" || snap.status == "needsAuth" {
            crate::applog(&format!("{}: {} — {}", vendor.id(), snap.status, snap.note));
        }
        broadcast(&app, vendor, snap);
        sleep_interruptible(vendor, POLL_SECS);
    });
}

/// Doctor line — runs before the app exists, so it reads the config file directly.
pub fn probe(vendor: Vendor) -> String {
    let (key, budget) = settings_from(&crate::config::load(), vendor);
    if key.is_empty() {
        return format!("{}: no admin key (Settings → API spend)", vendor.label());
    }
    // Enough to tell keys apart in doctor output, never enough to reuse
    let masked = format!("{}…{}", &key[..key.len().min(7)], &key[key.len().saturating_sub(3)..]);
    let now = now_ms() / 1000;
    let (start, next, today) = month_bounds(now);
    match fetch_month(vendor, &key, start, next) {
        Ok(b) => {
            let s = snapshot_from(&b, budget, next, today);
            let bal = if vendor == Vendor::Xai {
                match xai_balance(None, &key) {
                    Ok(usd) => format!(", balance {}", fmt_usd(usd)),
                    Err(FetchErr::NeedsAuth(m)) | Err(FetchErr::Other(m)) => format!(", balance unavailable ({m})"),
                }
            } else {
                String::new()
            };
            format!(
                "{}: key {masked} OK, {} buckets, month {} today {} budget {}{bal}",
                vendor.label(),
                b.len(),
                s.windows[0].text.clone().unwrap_or_default(),
                s.windows[1].text.clone().unwrap_or_default(),
                if budget > 0.0 { fmt_usd(budget) } else { "none".into() }
            )
        }
        Err(FetchErr::NeedsAuth(m)) | Err(FetchErr::Other(m)) => format!("{}: key {masked} — {m}", vendor.label()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_refresh_wakes_every_vendor() {
        request_refresh();
        // each poller takes its own flag; none of them steals another's
        assert!(take_refresh(Vendor::Xai));
        assert!(take_refresh(Vendor::Anthropic));
        assert!(take_refresh(Vendor::OpenAi));
        assert!(!take_refresh(Vendor::Xai));
    }

    #[test]
    fn xai_traces_summarise_the_reply() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"timeSeries":[{"group":["API grok-4.7"],"dataPoints":[{"timestamp":"2026-10-25T00:00:00Z","values":[1.5]},{"timestamp":"2026-10-26T00:00:00Z","values":[0]}]}],"limitReached":false}"#,
        )
        .unwrap();
        let t = xai_usage_trace(&v, &parse_xai(&v));
        assert!(t.contains("1 series, 2 points, limitReached=false"), "{t}");
        assert!(t.contains("unparsed 0; last days with spend: 2026-10-25 $1.5000"), "{t}");
        let b: serde_json::Value = serde_json::from_str(
            r#"{"changes":[{"changeOrigin":"PURCHASE","amount":{"val":"-5000"},"createTs":"2026-10-03T10:00:00Z"},
                           {"changeOrigin":"SPEND","amount":{"val":"420"},"createTs":"2026-10-26T01:00:00Z"}],"total":{"val":"-4580"}}"#,
        )
        .unwrap();
        let t = xai_balance_trace(&b, parse_rfc3339_secs("2026-10-01T00:00:00Z").unwrap());
        assert!(t.contains("2 ledger entries, SPEND in 2026-10: 1 entries $4.20"), "{t}");
        assert!(t.contains("newest: SPEND +4.20 @ 2026-10-26T01:00:00Z; PURCHASE -50.00 @ 2026-10-03T10:00:00Z"), "{t}");
    }

    #[test]
    fn the_last_day_with_spend_gets_its_own_row() {
        let day = |s: &str| parse_rfc3339_secs(s).unwrap();
        let b = vec![
            Bucket { start_secs: day("2026-10-12T00:00:00Z"), usd: 8.09, lines: vec![] },
            Bucket { start_secs: day("2026-10-14T00:00:00Z"), usd: 0.91, lines: vec![] },
            Bucket { start_secs: day("2026-10-27T00:00:00Z"), usd: 0.0, lines: vec![] },
        ];
        let s = snapshot_from(&b, 20.0, 0, day("2026-10-27T00:00:00Z"));
        assert_eq!(s.windows[0].id, "month", "the month stays first: ring and headline read windows[0]");
        let last = s.windows.iter().find(|w| w.id == "last_charge").expect("a last-charge row");
        assert_eq!(last.text.as_deref(), Some("2026-10-14"));
        assert_eq!((last.used, last.resets_at), (0.0, None), "a plain figure: no bar, no reset");
        assert!(snapshot_from(&[], 20.0, 0, 0).windows.iter().all(|w| w.id != "last_charge"), "no spend, no row");
    }

    #[test]
    fn xai_series_fold_into_daily_buckets() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"timeSeries":[
              {"group":["Chat grok-4"],"groupLabels":["Chat grok-4"],"dataPoints":[{"timestamp":"2026-09-01T00:00:00Z","values":[0.75]},{"timestamp":"2026-09-02T00:00:00Z","values":[0]}]},
              {"group":["Image grok-imagine"],"groupLabels":["Image grok-imagine"],"dataPoints":[{"timestamp":"2026-09-01T00:00:00Z","values":[0.25]},{"timestamp":"2026-09-02T00:00:00Z","values":[1.5]}]}
            ],"limitReached":false}"#,
        )
        .unwrap();
        let b = parse_xai(&v);
        assert_eq!(b.len(), 2);
        assert!((b[0].usd - 1.0).abs() < 1e-9);
        assert!((b[1].usd - 1.5).abs() < 1e-9);
        let s = snapshot_from(&b, 10.0, 0, b[1].start_secs);
        assert_eq!(s.windows[0].text.as_deref(), Some("$2.50"));
        assert_eq!(s.windows[1].text.as_deref(), Some("$1.50"));
        assert!(s.note.starts_with("Image grok-imagine $1.75"));
        assert_eq!(xai_local_ts(b[0].start_secs), "2026-09-01 00:00:00");
    }

    #[test]
    fn civil_roundtrip() {
        // 2026-09-12T15:00:00Z
        let t = 1_789_225_200u64;
        assert_eq!(ymd_utc(t), (2026, 9, 12));
        let (start, next, today) = month_bounds(t);
        assert_eq!(rfc3339_utc(start), "2026-09-01T00:00:00Z");
        assert_eq!(rfc3339_utc(next), "2026-10-01T00:00:00Z");
        assert_eq!(rfc3339_utc(today), "2026-09-12T00:00:00Z");
        assert_eq!(parse_rfc3339_secs("2026-09-01T00:00:00Z"), Some(start));
        // December rolls the year
        let (_, next, _) = month_bounds(parse_rfc3339_secs("2026-12-20T10:00:00Z").unwrap());
        assert_eq!(rfc3339_utc(next), "2027-01-01T00:00:00Z");
    }

    #[test]
    fn anthropic_amounts_are_cents() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"data":[{"starting_at":"2026-09-12T00:00:00Z","ending_at":"2026-09-13T00:00:00Z","results":[
              {"amount":"123.5","currency":"USD","description":"Claude Opus 5 Usage - Input Tokens","model":"claude-opus-5"},
              {"amount":"76.5","currency":"USD","description":"Claude Opus 5 Usage - Output Tokens","model":"claude-opus-5"}]}],"has_more":false}"#,
        )
        .unwrap();
        let b = parse_buckets(Vendor::Anthropic, &v);
        assert_eq!(b.len(), 1);
        assert!((b[0].usd - 2.0).abs() < 1e-9);
        assert_eq!(b[0].lines.len(), 1);
        let s = snapshot_from(&b, 50.0, 0, b[0].start_secs);
        assert_eq!(s.windows[0].text.as_deref(), Some("$2.00"));
        assert!((s.windows[0].used - 0.04).abs() < 1e-9);
        assert_eq!(s.windows[1].text.as_deref(), Some("$2.00"));
        assert!(s.note.starts_with("claude-opus-5 $2.00"));
    }

    #[test]
    fn openai_amounts_are_dollars_and_no_budget_means_empty_ring() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"object":"page","data":[{"object":"bucket","start_time":1789171200,"end_time":1789257600,"results":[
              {"object":"organization.costs.result","amount":{"value":0.75,"currency":"usd"},"line_item":"gpt-4o-mini, input"},
              {"object":"organization.costs.result","amount":{"value":0.25,"currency":"usd"},"line_item":"gpt-4o-mini, output"}]}],"has_more":false,"next_page":null}"#,
        )
        .unwrap();
        let b = parse_buckets(Vendor::OpenAi, &v);
        assert!((b[0].usd - 1.0).abs() < 1e-9);
        assert_eq!(b[0].lines[0].0, "gpt-4o-mini");
        let s = snapshot_from(&b, 0.0, 0, 0);
        assert_eq!(s.windows[0].used, 0.0);
        assert_eq!(s.windows[0].text.as_deref(), Some("$1.00"));
        assert_eq!(s.windows[1].text.as_deref(), Some("$0.00"));
    }
}
