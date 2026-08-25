use std::env;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use dotenv::dotenv;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

const API_BASE: &str = "https://api.hetzner.cloud/v1";

// ---------------------------------------------------------------------------
// API types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Zone {
    name: String,
}

#[derive(Deserialize)]
struct ZoneList {
    zones: Vec<Zone>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct RecordValue {
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
}

impl RecordValue {
    fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            comment: None,
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
struct RRSet {
    name: String,
    #[serde(rename = "type")]
    record_type: String,
    ttl: Option<u32>,
    records: Vec<RecordValue>,
}

#[derive(Deserialize)]
struct RRSetList {
    rrsets: Vec<RRSet>,
}

#[derive(Serialize)]
struct CreateRRSet<'a> {
    name: &'a str,
    #[serde(rename = "type")]
    record_type: &'a str,
    ttl: Option<u32>,
    records: Vec<RecordValue>,
}

#[derive(Serialize)]
struct RecordsBody {
    records: Vec<RecordValue>,
}

#[derive(Serialize)]
struct ChangeTtl {
    ttl: Option<u32>,
}

#[derive(Deserialize, Debug)]
struct Action {
    id: u64,
    status: String,
    #[serde(default)]
    error: Option<ActionError>,
}

#[derive(Deserialize, Debug)]
struct ActionError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct ActionResponse {
    action: Option<Action>,
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "hetzner_ddns",
    version,
    about = "Dynamic DNS updater and ACME DNS-01 helper for Hetzner DNS"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Update the AAAA (IPv6) record as well (only with the default `update` action)
    #[arg(long, global = true)]
    ipv6: bool,

    /// TTL to set on updated records (seconds, min 60)
    #[arg(long, global = true, default_value_t = 60)]
    ttl: u32,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Point the DNS_FQDN record at this host's current public IP (default)
    Update,

    /// Create an ACME DNS-01 challenge TXT record
    ///
    /// Called by the TrueNAS `shell` authenticator as:
    ///   <script> set <domain> <validation_name> <validation_content>
    Set {
        /// Certificate domain (unused; accepted for the TrueNAS calling convention)
        domain: String,
        /// FQDN of the TXT record, e.g. _acme-challenge.example.com
        validation_name: String,
        /// TXT record content supplied by the ACME server
        validation_content: String,
    },

    /// Remove an ACME DNS-01 challenge TXT record (idempotent)
    ///
    /// Called by the TrueNAS `shell` authenticator as:
    ///   <script> unset <domain> <validation_name> <validation_content>
    Unset {
        /// Certificate domain (unused; accepted for the TrueNAS calling convention)
        domain: String,
        /// FQDN of the TXT record, e.g. _acme-challenge.example.com
        validation_name: String,
        /// TXT record content supplied by the ACME server
        validation_content: String,
    },
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Send a request, returning an error for any non-2xx status.
///
/// Without this, a redirect to an HTML error page (as happened when Hetzner
/// retired the old DNS API) surfaces as an opaque JSON decode error.
fn send_ok(req: RequestBuilder) -> Result<Response, Box<dyn std::error::Error>> {
    let resp = req.send()?;
    let status = resp.status();
    if !status.is_success() {
        let url = resp.url().to_string();
        let body = resp.text().unwrap_or_default();
        let snippet: String = body.chars().take(300).collect();
        return Err(format!("❌ HTTP {} from {}\n{}", status, url, snippet).into());
    }
    Ok(resp)
}

/// Like `send_ok`, but treats the given status as a non-error and returns `None`.
fn send_allow(
    req: RequestBuilder,
    allow: StatusCode,
) -> Result<Option<Response>, Box<dyn std::error::Error>> {
    let resp = req.send()?;
    if resp.status() == allow {
        return Ok(None);
    }
    let status = resp.status();
    if !status.is_success() {
        let url = resp.url().to_string();
        let body = resp.text().unwrap_or_default();
        let snippet: String = body.chars().take(300).collect();
        return Err(format!("❌ HTTP {} from {}\n{}", status, url, snippet).into());
    }
    Ok(Some(resp))
}

/// Hetzner mutations return an async Action. Wait for it to reach a terminal state
/// so we do not report success before the change is actually applied.
fn await_action(
    client: &Client,
    token: &str,
    zone: &str,
    resp: Response,
) -> Result<(), Box<dyn std::error::Error>> {
    let parsed: ActionResponse = match resp.json() {
        Ok(p) => p,
        // Not every endpoint returns an action; nothing to wait for.
        Err(_) => return Ok(()),
    };
    let Some(action) = parsed.action else {
        return Ok(());
    };

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut action = action;
    loop {
        match action.status.as_str() {
            "success" => return Ok(()),
            "error" => {
                let e = action.error.unwrap_or(ActionError {
                    code: "unknown".into(),
                    message: "action failed".into(),
                });
                return Err(format!("❌ Hetzner action failed: {} — {}", e.code, e.message).into());
            }
            _ => {}
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "❌ Timed out waiting for Hetzner action {} (last status: {})",
                action.id, action.status
            )
            .into());
        }

        std::thread::sleep(Duration::from_secs(1));

        let refreshed: ActionResponse = send_ok(
            client
                .get(format!("{}/zones/{}/actions/{}", API_BASE, zone, action.id))
                .bearer_auth(token),
        )?
        .json()?;

        match refreshed.action {
            Some(a) => action = a,
            None => return Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Zone helpers
// ---------------------------------------------------------------------------

fn normalize_fqdn(s: &str) -> String {
    s.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn is_in_zone(fqdn: &str, zone: &str) -> bool {
    fqdn == zone || fqdn.ends_with(&format!(".{}", zone))
}

/// Resolve which of the account's zones hosts `fqdn`.
///
/// `DNS_ZONE` short-circuits the lookup. Otherwise the longest matching zone
/// wins, so `a.b.example.com` resolves correctly whether the zone is
/// `example.com` or `b.example.com`.
fn resolve_zone(
    client: &Client,
    token: &str,
    fqdn: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Ok(z) = env::var("DNS_ZONE") {
        let z = normalize_fqdn(&z);
        if !z.is_empty() {
            if !is_in_zone(fqdn, &z) {
                return Err(format!("❌ {} is not inside DNS_ZONE {}", fqdn, z).into());
            }
            return Ok(z);
        }
    }

    let zones: ZoneList = send_ok(
        client
            .get(format!("{}/zones", API_BASE))
            .query(&[("per_page", "100")])
            .bearer_auth(token),
    )?
    .json()?;

    zones
        .zones
        .into_iter()
        .filter(|z| is_in_zone(fqdn, &normalize_fqdn(&z.name)))
        .max_by_key(|z| z.name.len())
        .map(|z| normalize_fqdn(&z.name))
        .ok_or_else(|| format!("❌ No zone in your account matches {}", fqdn).into())
}

/// Record name relative to its zone; `@` for the apex.
fn record_name_in_zone(fqdn: &str, zone: &str) -> String {
    if fqdn == zone {
        "@".to_string()
    } else {
        fqdn[..fqdn.len() - zone.len() - 1].to_string()
    }
}

fn fetch_rrsets(
    client: &Client,
    token: &str,
    zone: &str,
    name: &str,
) -> Result<Vec<RRSet>, Box<dyn std::error::Error>> {
    let list: RRSetList = send_ok(
        client
            .get(format!("{}/zones/{}/rrsets", API_BASE, zone))
            .query(&[("name", name), ("per_page", "100")])
            .bearer_auth(token),
    )?
    .json()?;
    Ok(list.rrsets)
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    if let Err(e) = run() {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // A missing .env is fine as long as the variables come from the real
    // environment. This matters for the TrueNAS shell authenticator, which runs
    // the script as `nobody` from an unspecified working directory.
    let dotenv_err = dotenv().err();

    let api_token = env::var("HETZNER_API_TOKEN").map_err(|_| match &dotenv_err {
        Some(e) => format!("❌ Missing HETZNER_API_TOKEN and could not load .env: {}", e),
        None => "❌ Missing HETZNER_API_TOKEN in environment (check .env file)".to_string(),
    })?;

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    match cli.command.unwrap_or(Command::Update) {
        Command::Update => cmd_update(&client, &api_token, cli.ipv6, cli.ttl),
        Command::Set {
            validation_name,
            validation_content,
            ..
        } => cmd_challenge(
            &client,
            &api_token,
            &validation_name,
            &validation_content,
            cli.ttl,
            true,
        ),
        Command::Unset {
            validation_name,
            validation_content,
            ..
        } => cmd_challenge(
            &client,
            &api_token,
            &validation_name,
            &validation_content,
            cli.ttl,
            false,
        ),
    }
}

// ---------------------------------------------------------------------------
// `update` — dynamic DNS
// ---------------------------------------------------------------------------

fn cmd_update(
    client: &Client,
    token: &str,
    want_ipv6: bool,
    ttl: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let dns_fqdn = env::var("DNS_FQDN")
        .map_err(|_| "❌ Missing DNS_FQDN in environment (check .env file)")?;
    let dns_fqdn = normalize_fqdn(&dns_fqdn);

    if !dns_fqdn.contains('.') {
        return Err("DNS_FQDN must be a valid FQDN (e.g. dyndns.example.com)".into());
    }

    let zone = resolve_zone(client, token, &dns_fqdn)?;
    let record_name = record_name_in_zone(&dns_fqdn, &zone);
    println!("🌍 Zone: {}, record: {}", zone, record_name);

    let ip4 = send_ok(client.get("https://ipv4.icanhazip.com"))?
        .text()?
        .trim()
        .to_string();

    let ip6 = if want_ipv6 {
        client
            .get("https://ipv6.icanhazip.com")
            .send()
            .ok()
            .filter(|r| r.status().is_success())
            .and_then(|r| r.text().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    let rrsets = fetch_rrsets(client, token, &zone, &record_name)?;

    update_address_record(client, token, &zone, &record_name, "A", &ip4, ttl, &rrsets)?;

    if want_ipv6 {
        match ip6 {
            Some(ip6) => update_address_record(
                client,
                token,
                &zone,
                &record_name,
                "AAAA",
                &ip6,
                ttl,
                &rrsets,
            )?,
            None => println!("ℹ️ No public IPv6 address found. Skipping AAAA update."),
        }
    } else {
        println!("ℹ️ Skipping AAAA update (use --ipv6 to enable).");
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn update_address_record(
    client: &Client,
    token: &str,
    zone: &str,
    record_name: &str,
    record_type: &str,
    ip: &str,
    ttl: u32,
    rrsets: &[RRSet],
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(existing) = rrsets
        .iter()
        .find(|r| r.name == record_name && r.record_type == record_type)
    else {
        println!("⚠️ {} record not found in zone.", record_type);
        return Ok(());
    };

    let current: Vec<&str> = existing.records.iter().map(|r| r.value.as_str()).collect();

    if current == [ip] {
        println!("✅ {} record already up to date: {}", record_type, ip);
    } else {
        println!(
            "🔄 Updating {} record from {} to {}",
            record_type,
            current.join(", "),
            ip
        );
        let resp = send_ok(
            client
                .post(format!(
                    "{}/zones/{}/rrsets/{}/{}/actions/set_records",
                    API_BASE, zone, record_name, record_type
                ))
                .bearer_auth(token)
                .json(&RecordsBody {
                    records: vec![RecordValue::new(ip)],
                }),
        )?;
        await_action(client, token, zone, resp)?;
        println!("✅ {} record updated.", record_type);
    }

    if existing.ttl != Some(ttl) {
        println!("🔄 Setting {} record TTL to {}s", record_type, ttl);
        let resp = send_ok(
            client
                .post(format!(
                    "{}/zones/{}/rrsets/{}/{}/actions/change_ttl",
                    API_BASE, zone, record_name, record_type
                ))
                .bearer_auth(token)
                .json(&ChangeTtl { ttl: Some(ttl) }),
        )?;
        await_action(client, token, zone, resp)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// `set` / `unset` — ACME DNS-01
// ---------------------------------------------------------------------------

/// Hetzner rejects unquoted TXT values with `422 invalid_input`.
fn quote_txt(value: &str) -> String {
    let v = value.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v.to_string()
    } else {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn cmd_challenge(
    client: &Client,
    token: &str,
    validation_name: &str,
    validation_content: &str,
    ttl: u32,
    create: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let fqdn = normalize_fqdn(validation_name);
    if fqdn.is_empty() {
        return Err("❌ validation_name must not be empty".into());
    }

    let zone = resolve_zone(client, token, &fqdn)?;
    let name = record_name_in_zone(&fqdn, &zone);
    let value = quote_txt(validation_content);

    // ACME requires a TTL floor of 60s here; anything longer just slows renewal.
    let ttl = ttl.max(60);

    if create {
        acme_set(client, token, &zone, &name, &value, ttl)
    } else {
        acme_unset(client, token, &zone, &name, &value)
    }
}

fn acme_set(
    client: &Client,
    token: &str,
    zone: &str,
    name: &str,
    value: &str,
    ttl: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let existing = fetch_rrsets(client, token, zone, name)?
        .into_iter()
        .find(|r| r.name == name && r.record_type == "TXT");

    match existing {
        // A TXT RRSet already exists. Add to it rather than replacing: a
        // wildcard plus base-domain cert produces two challenges on the same
        // name, and clobbering one fails the other.
        Some(rrset) => {
            if rrset.records.iter().any(|r| r.value == value) {
                println!("✅ TXT {}.{} already present", name, zone);
                return Ok(());
            }
            let resp = send_ok(
                client
                    .post(format!(
                        "{}/zones/{}/rrsets/{}/TXT/actions/add_records",
                        API_BASE, zone, name
                    ))
                    .bearer_auth(token)
                    .json(&RecordsBody {
                        records: vec![RecordValue::new(value)],
                    }),
            )?;
            await_action(client, token, zone, resp)?;
            println!("✅ Added TXT {}.{}", name, zone);
        }
        None => {
            let resp = send_ok(
                client
                    .post(format!("{}/zones/{}/rrsets", API_BASE, zone))
                    .bearer_auth(token)
                    .json(&CreateRRSet {
                        name,
                        record_type: "TXT",
                        ttl: Some(ttl),
                        records: vec![RecordValue::new(value)],
                    }),
            )?;
            await_action(client, token, zone, resp)?;
            println!("✅ Created TXT {}.{}", name, zone);
        }
    }

    Ok(())
}

fn acme_unset(
    client: &Client,
    token: &str,
    zone: &str,
    name: &str,
    value: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // 404 covers both "no such RRSet" and "RRSet has no record with that value".
    // Cleanup must stay idempotent, so neither is an error.
    let resp = send_allow(
        client
            .post(format!(
                "{}/zones/{}/rrsets/{}/TXT/actions/remove_records",
                API_BASE, zone, name
            ))
            .bearer_auth(token)
            .json(&RecordsBody {
                records: vec![RecordValue::new(value)],
            }),
        StatusCode::NOT_FOUND,
    )?;

    match resp {
        Some(resp) => {
            await_action(client, token, zone, resp)?;
            println!("✅ Removed TXT {}.{}", name, zone);
        }
        None => println!("ℹ️ TXT {}.{} already absent", name, zone),
    }

    // Removing the final record deletes the RRSet, so no extra cleanup is needed.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_bare_txt_values() {
        assert_eq!(quote_txt("abc123"), "\"abc123\"");
    }

    #[test]
    fn leaves_already_quoted_values_alone() {
        assert_eq!(quote_txt("\"abc123\""), "\"abc123\"");
    }

    #[test]
    fn escapes_embedded_quotes() {
        assert_eq!(quote_txt("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn splits_subdomain_from_zone() {
        assert_eq!(record_name_in_zone("home.zlor.fi", "zlor.fi"), "home");
        assert_eq!(
            record_name_in_zone("_acme-challenge.home.zlor.fi", "zlor.fi"),
            "_acme-challenge.home"
        );
    }

    #[test]
    fn uses_at_for_zone_apex() {
        assert_eq!(record_name_in_zone("zlor.fi", "zlor.fi"), "@");
    }

    #[test]
    fn zone_membership_is_label_aware() {
        assert!(is_in_zone("home.zlor.fi", "zlor.fi"));
        assert!(is_in_zone("zlor.fi", "zlor.fi"));
        // Must not match a zone that is merely a string suffix.
        assert!(!is_in_zone("notzlor.fi", "zlor.fi"));
    }

    #[test]
    fn normalizes_trailing_dot_and_case() {
        assert_eq!(normalize_fqdn("Home.Zlor.Fi."), "home.zlor.fi");
    }
}
