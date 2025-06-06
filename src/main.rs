use std::env;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use dotenv::dotenv;
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    let ip = client.get("https://ipv4.icanhazip.com")
        .send()?.text()?.trim().to_string();

    // Get Zone ID
    let zones: ZoneList = client.get("https://dns.hetzner.com/api/v1/zones")
        .header("Auth-API-Token", &api_token)
        .send()?.json()?;

    let zone = zones.zones.iter().find(|z| z.name == zone_name)
        .ok_or("Zone not found")?;

    // Get DNS record
    let records: RecordList = client.get(format!("https://dns.hetzner.com/api/v1/records?zone_id={}", zone.id))
        .header("Auth-API-Token", &api_token)
        .send()?.json()?;

    let record = records.records.iter().find(|r| r.name == record_name && r.record_type == "A")
        .ok_or("Record not found")?;

    if record.value == ip {
        println!("IP unchanged: {}", ip);
        return Ok(());
    }

    println!("Updating {} from {} to {}", record_name, record.value, ip);

    let updated = Record {
        value: ip.clone(),
        ttl: Some(60),
        ..(*record).clone()
    };

    client.put(format!("https://dns.hetzner.com/api/v1/records/{}", record.id))
        .header("Auth-API-Token", &api_token)
        .header("Content-Type", "application/json")
        .json(&updated)
        .send()?;

    println!("Updated DNS record to {}", ip);
    Ok(())
}
