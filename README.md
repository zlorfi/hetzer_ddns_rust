# hetzner_ddns

A small dynamic DNS updater for domains hosted on Hetzner.

It looks up your current public IP, compares it to the existing DNS record, and
updates the record only when it has actually changed.

Uses the **Hetzner Cloud API** (`api.hetzner.cloud/v1`). The old standalone DNS
API (`dns.hetzner.com/api/v1`) has been retired — it now redirects to the
Hetzner Console and returns HTML, so any tool still pointing at it will fail
with a JSON decode error.

## Requirements

- Rust (2021 edition)
- A Hetzner API token with DNS read/write access
- A DNS zone already hosted at Hetzner, with the record you want to update
  **already existing** (this tool updates records, it does not create them)

## Setup

Create a `.env` file next to the binary:

```env
HETZNER_API_TOKEN=your_api_token_here
DNS_FQDN=home.example.com
```

| Variable | Required | Description |
| --- | --- | --- |
| `HETZNER_API_TOKEN` | yes | Hetzner API token, sent as `Authorization: Bearer` |
| `DNS_FQDN` | yes | Fully qualified name of the record to update |
| `DNS_ZONE` | no | Zone name override, e.g. `example.com` |

`DNS_ZONE` is optional. Without it the tool lists your zones and picks the
longest one that is a suffix of `DNS_FQDN`, which handles nested names like
`a.b.example.com` correctly. Setting it explicitly saves one API call per run
and also works if your token is not permitted to list all zones.

To update the zone apex, set `DNS_FQDN` to the zone name itself
(e.g. `DNS_FQDN=example.com`); the record is then addressed as `@`.

## Build

```sh
cargo build --release
```

## Usage

```sh
./target/release/hetzner_ddns
```

```
Usage: hetzner_ddns [OPTIONS]

Options:
      --ipv6       Update the AAAA (IPv6) record as well
      --ttl <TTL>  TTL to set on updated records (seconds, min 60) [default: 60]
  -h, --help       Print help
  -V, --version    Print version
```

IPv6 is opt-in. With `--ipv6` the tool also updates the `AAAA` record; if no
public IPv6 address can be determined it skips that step instead of failing.

Example output:

```
🌍 Zone: example.com, record: home
🔄 Updating A record from 203.0.113.10 to 198.51.100.4
✅ A record updated.
ℹ️ Skipping AAAA update (use --ipv6 to enable).
```

Note that `--ttl` is applied on every run, so the record TTL will be changed to
match the flag even when the IP itself is unchanged. Pass the value your zone
already uses if you do not want it altered.

## Running periodically

The binary is a one-shot command — pair it with cron or a systemd timer.

```cron
*/5 * * * * cd /opt/hetzner_ddns && ./hetzner_ddns --ipv6 >> /var/log/hetzner_ddns.log 2>&1
```

The `.env` file is read from the current working directory, so `cd` into the
right place first.

## Exit codes

`0` on success, `1` on any error (missing `.env`, missing variables, zone or
record not found, or a non-2xx API response). API failures include the HTTP
status, URL, and response body to make diagnosis straightforward.

## Notes

- Public IP is detected via `ipv4.icanhazip.com` / `ipv6.icanhazip.com`.
- Records are managed as RRSets; an update replaces the record set with the
  single detected address.
- Keep `.env` out of version control — it is already in `.gitignore`.
