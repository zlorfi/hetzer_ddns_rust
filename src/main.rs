use std::env;
use reqwest::blocking::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use dotenv::dotenv;
use clap::Parser;
use dotenv::Error as DotenvError;

const API_BASE: &str = "https://api.hetzner.cloud/v1";

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
struct SetRecords {
    records: Vec<RecordValue>,
}

#[derive(Serialize)]
struct ChangeTtl {
    ttl: Option<u32>,
}

#[derive(Parser, Debug)]
#[command(name = "hetzner-ddns", version, about = "Dynamic DNS updater for Hetzner")]
struct Cli {
    /// Update the AAAA (IPv6) record as well
    #[arg(long)]
    ipv6: bool,

    /// TTL to set on updated records (seconds, min 60)
    #[arg(long, default_value_t = 60)]
    ttl: u32,
}

/// Send a request and fail loudly on non-2xx instead of trying to parse the body as JSON.
fn send_ok(req: RequestBuilder) -> Result<reqwest::blocking::Response, Box<dyn std::error::Error>> {
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Cli::parse();

    match dotenv() {
        Ok(_) => {}
        Err(DotenvError::Io(ref e)) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("❌ Error: .env file not found. Please create one with DNS_FQDN=...");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("❌ Error loading .env file: {}", e);
            std::process::exit(1);
        }
    }

    let api_token = env::var("HETZNER_API_TOKEN")
        .map_err(|_| "❌ Missing HETZNER_API_TOKEN in environment (check .env file)")?;
    let dns_fqdn = env::var("DNS_FQDN")
        .map_err(|_| "❌ Missing DNS_FQDN in environment (check .env file)")?;
    let dns_fqdn = dns_fqdn.trim().trim_end_matches('.').to_string();

    if !dns_fqdn.contains('.') {
        return Err("DNS_FQDN must be a valid FQDN (e.g. dyndns.example.com)".into());
    }

    let client = Client::new();

    // --- Resolve zone ---
    // DNS_ZONE (optional) skips the zone listing call. Otherwise pick the
    // longest zone name in the account that is a suffix of the FQDN.
    let zone_name = match env::var("DNS_ZONE") {
        Ok(z) => {
            let z = z.trim().trim_end_matches('.').to_string();
            if dns_fqdn != z && !dns_fqdn.ends_with(&format!(".{}", z)) {
                return Err(format!("❌ DNS_FQDN {} is not inside DNS_ZONE {}", dns_fqdn, z).into());
            }
            z
        }
        Err(_) => {
            let zones: ZoneList = send_ok(
                client
                    .get(format!("{}/zones", API_BASE))
                    .query(&[("per_page", "100")])
                    .bearer_auth(&api_token),
            )?
            .json()?;

            zones
                .zones
                .into_iter()
                .filter(|z| dns_fqdn == z.name || dns_fqdn.ends_with(&format!(".{}", z.name)))
                .max_by_key(|z| z.name.len())
                .ok_or_else(|| format!("❌ No zone in your account matches {}", dns_fqdn))?
                .name
        }
    };

    // Record name relative to the zone ("@" for the apex)
    let record_name = if dns_fqdn == zone_name {
        "@".to_string()
    } else {
        dns_fqdn[..dns_fqdn.len() - zone_name.len() - 1].to_string()
    };

    println!("🌍 Zone: {}, record: {}", zone_name, record_name);

    // --- Fetch public IPs ---
    let ip4 = send_ok(client.get("https://ipv4.icanhazip.com"))?
        .text()?
        .trim()
        .to_string();
    let ip6 = if args.ipv6 {
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

    // --- Fetch RRSets for this record name ---
    let rrsets: RRSetList = send_ok(
        client
            .get(format!("{}/zones/{}/rrsets", API_BASE, zone_name))
            .query(&[("name", record_name.as_str()), ("per_page", "100")])
            .bearer_auth(&api_token),
    )?
    .json()?;

    update_rrset(&client, &api_token, &zone_name, &record_name, "A", &ip4, args.ttl, &rrsets)?;

    if args.ipv6 {
        match ip6 {
            Some(ip6) => update_rrset(
                &client, &api_token, &zone_name, &record_name, "AAAA", &ip6, args.ttl, &rrsets,
            )?,
            None => println!("ℹ️ No public IPv6 address found. Skipping AAAA update."),
        }
    } else {
        println!("ℹ️ Skipping AAAA update (use --ipv6 to enable).");
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn update_rrset(
    client: &Client,
    api_token: &str,
    zone_name: &str,
    record_name: &str,
    record_type: &str,
    ip: &str,
    ttl: u32,
    rrsets: &RRSetList,
) -> Result<(), Box<dyn std::error::Error>> {
    let existing = rrsets
        .rrsets
        .iter()
        .find(|r| r.name == record_name && r.record_type == record_type);

    let Some(existing) = existing else {
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
        send_ok(
            client
                .post(format!(
                    "{}/zones/{}/rrsets/{}/{}/actions/set_records",
                    API_BASE, zone_name, record_name, record_type
                ))
                .bearer_auth(api_token)
                .json(&SetRecords {
                    records: vec![RecordValue {
                        value: ip.to_string(),
                        comment: None,
                    }],
                }),
        )?;
        println!("✅ {} record updated.", record_type);
    }

    if existing.ttl != Some(ttl) {
        println!("🔄 Setting {} record TTL to {}s", record_type, ttl);
        send_ok(
            client
                .post(format!(
                    "{}/zones/{}/rrsets/{}/{}/actions/change_ttl",
                    API_BASE, zone_name, record_name, record_type
                ))
                .bearer_auth(api_token)
                .json(&ChangeTtl { ttl: Some(ttl) }),
        )?;
    }

    Ok(())
}
