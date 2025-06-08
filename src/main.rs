use std::env;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use dotenv::dotenv;
use clap::Parser;
use dotenv::Error as DotenvError;

#[derive(Deserialize)]
struct Zone {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct ZoneList {
    zones: Vec<Zone>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct Record {
    id: String,
    #[serde(rename = "type")]
    record_type: String,
    name: String,
    value: String,
    zone_id: String,
    ttl: Option<u32>,
}

#[derive(Deserialize)]
struct RecordList {
    records: Vec<Record>,
}

#[derive(Parser, Debug)]
#[command(name = "hetzner-ddns", version, about = "Dynamic DNS updater for Hetzner")]
struct Cli {
    /// Update the AAAA (IPv6) record as well
    #[arg(long)]
    ipv6: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Cli::parse();
    let update_ipv6 = args.ipv6;

    match dotenv() {
        Ok(_) => {} // .env loaded
        Err(DotenvError::Io(ref e)) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("❌ Error: .env file not found. Please create one with DNS_FQDN=...");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("❌ Error loading .env file: {}", e);
            std::process::exit(1);
        }
    }
    //dotenv().ok();
    let api_token = env::var("HETZNER_API_TOKEN").map_err(|_| "❌ Missing HETZNER_API_TOKEN in environment (check .env file)")?;
    let dns_fqdn = env::var("DNS_FQDN").map_err(|_| "❌ Missing DNS_FQDN in environment (check .env file)")?;

    // Split domain from record
    let parts: Vec<&str> = dns_fqdn.split('.').collect();
    if parts.len() < 2 {
        return Err("DNS_FQDN must be a valid FQDN (e.g. dyndns.example.com)".into());
    }

    let record_name = parts[0].to_string();
    let zone_name = parts[1..].join(".");

    let client = Client::new();

    // Fetch public IPs
    let ip4 = client.get("https://ipv4.icanhazip.com").send()?.text()?.trim().to_string();
    let ip6 = client.get("https://ipv6.icanhazip.com").send().ok()
        .and_then(|r| r.text().ok())
        .map(|s| s.trim().to_string());

    // Get Zone ID
    let zones: ZoneList = client.get("https://dns.hetzner.com/api/v1/zones")
        .header("Auth-API-Token", &api_token)
        .send()?.json()?;

    let zone = zones.zones.iter().find(|z| z.name == zone_name)
        .ok_or("❌ Zone not found")?;

    // Get DNS record
    let records: RecordList = client.get(format!("https://dns.hetzner.com/api/v1/records?zone_id={}", zone.id))
        .header("Auth-API-Token", &api_token)
        .send()?.json()?;

        // --- IPv4 (A) Record ---
    if let Some(record4) = records.records.iter().find(|r| r.name == record_name && r.record_type == "A") {
        if record4.value != ip4 {
            println!("🔄 Updating A record from {} to {}", record4.value, ip4);
            let updated4 = Record {
                value: ip4.clone(),
                ttl: Some(60),
                ..record4.to_owned()
            };

            client.put(format!("https://dns.hetzner.com/api/v1/records/{}", record4.id))
                .header("Auth-API-Token", &api_token)
                .header("Content-Type", "application/json")
                .json(&updated4)
                .send()?;
            println!("✅ A record updated.");
        } else {
            println!("✅ A record already up to date: {}", ip4);
        }
    } else {
        println!("⚠️  A record not found.");
    }

   // --- IPv6 (AAAA) Record ---
    if update_ipv6 {
        if let Some(ip6) = ip6 {
            if let Some(record6) = records.records.iter().find(|r| r.name == record_name && r.record_type == "AAAA") {
                if record6.value != ip6 {
                    println!("🔄 Updating AAAA record from {} to {}", record6.value, ip6);
                    let updated6 = Record {
                        value: ip6.clone(),
                        ttl: Some(60),
                        ..record6.to_owned()
                    };

                    client.put(format!("https://dns.hetzner.com/api/v1/records/{}", record6.id))
                        .header("Auth-API-Token", &api_token)
                        .header("Content-Type", "application/json")
                        .json(&updated6)
                        .send()?;
                    println!("✅ AAAA record updated.");
                } else {
                    println!("✅ AAAA record already up to date: {}", ip6);
                }
            } else {
                println!("⚠️  AAAA record not found.");
            }
        } else {
            println!("ℹ️  No public IPv6 address found. Skipping AAAA update.");
        }
    } else {
        println!("ℹ️  Skipping AAAA update (use --ipv6 to enable).");
    }

    Ok(())
}
