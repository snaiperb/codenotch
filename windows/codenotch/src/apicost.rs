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

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn sleep_interruptible(total_secs: u64) {
    for _ in 0..total_secs {
        if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
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
        ureq::get("https://management-api.x.ai/auth/management-keys/validation")
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
                let mut r = ureq::get("https://api.anthropic.com/v1/organizations/cost_report")
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
                let mut r = ureq::get("https://api.openai.com/v1/organization/costs")
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
    let req = ureq::post(&format!("https://management-api.x.ai/v1/billing/teams/{team}/usage"))
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
    Ok(parse_xai(&v))
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
            text: Some(fmt_usd(month)),
        },
        LimitWindow {
            id: "today".into(),
            label: "Today".into(),
            used: share(today_usd),
            resets_at: Some((today + 86_400) * 1000),
            count: None,
            derived: false,
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
        Ok(buckets) => snapshot_from(&buckets, budget, next, today),
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
        sleep_interruptible(POLL_SECS);
    });
}

/// Doctor line — runs before the app exists, so it reads the config file directly.
pub fn probe(vendor: Vendor) -> String {
    let (key, budget) = settings_from(&crate::config::load(), vendor);
    if key.is_empty() {
        return format!("{}: no admin key (Settings → API spend)", vendor.label());
    }
    let masked = format!("{}…{}", &key[..key.len().min(10)], &key[key.len().saturating_sub(4)..]);
    let now = now_ms() / 1000;
    let (start, next, today) = month_bounds(now);
    match fetch_month(vendor, &key, start, next) {
        Ok(b) => {
            let s = snapshot_from(&b, budget, next, today);
            format!(
                "{}: key {masked} OK, {} buckets, month {} today {} budget {}",
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
