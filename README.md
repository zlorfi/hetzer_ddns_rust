# hetzner_ddns

A small utility for domains hosted on Hetzner DNS. It does two things:

- **Dynamic DNS** — points a record at this host's current public IP, updating
  only when the address has actually changed.
- **ACME DNS-01** — creates and removes `_acme-challenge` TXT records, in the
  exact calling convention used by the TrueNAS `shell` authenticator.

Uses the **Hetzner Cloud API** (`api.hetzner.cloud/v1`). The old standalone DNS
API (`dns.hetzner.com/api/v1`) has been retired — it now redirects to the
Hetzner Console and returns HTML, so any tool still pointing at it will fail
with a JSON decode error.

## Requirements

- Rust (2021 edition)
- A Hetzner API token with DNS read/write access
- A DNS zone already hosted at Hetzner

## Configuration

Configuration comes from the environment, optionally via a `.env` file in the
current working directory:

```env
HETZNER_API_TOKEN=your_api_token_here
DNS_FQDN=home.example.com
```

| Variable | Required | Description |
| --- | --- | --- |
| `HETZNER_API_TOKEN` | yes | Hetzner API token, sent as `Authorization: Bearer` |
| `DNS_FQDN` | for `update` | Fully qualified name of the record to keep updated |
| `DNS_ZONE` | no | Zone name override, e.g. `example.com` |

A missing `.env` is not an error as long as the variables are present in the
real environment — this is what makes the TrueNAS integration below work.

`DNS_ZONE` is optional. Without it the tool lists your zones and picks the
longest one that is a suffix of the target name, which handles nested zones
like `a.b.example.com` correctly. Setting it explicitly saves one API call per
run and also works if the token is not permitted to list all zones.

## Build

```sh
cargo build --release
cargo test
```

### Cross-compiling for Linux x86-64

TrueNAS SCALE runs on amd64, so a build from an Apple Silicon Mac has to be
cross-compiled. The dependency tree is pure Rust (`rustls`, no OpenSSL), so a
fully static musl binary drops straight onto the NAS with no runtime deps.

Using Docker, which needs no host toolchain setup:

```sh
docker run --rm --platform linux/amd64 \
  -v "$PWD":/w -w /w -e CARGO_HOME=/w/.cargo-docker \
  rust:1-alpine \
  sh -c 'apk add --no-cache musl-dev && cargo build --release --target x86_64-unknown-linux-musl'
```

The result lands in `target/x86_64-unknown-linux-musl/release/hetzner_ddns`:

```
ELF 64-bit LSB pie executable, x86-64, static-pie linked
```

`strip` reduces it from roughly 6.5 MB to 5.0 MB. Being static, it runs on both
musl and glibc systems — Alpine and Debian alike.

A separate `CARGO_HOME` keeps the container's registry cache out of your host
`~/.cargo`; `.cargo-docker/` is gitignored.

Alternatively, `cargo-zigbuild` avoids Docker entirely:

```sh
brew install zig && cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-musl
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

## Usage

```
Usage: hetzner_ddns [OPTIONS] [COMMAND]

Commands:
  update  Point the DNS_FQDN record at this host's current public IP (default)
  set     Create an ACME DNS-01 challenge TXT record
  unset   Remove an ACME DNS-01 challenge TXT record (idempotent)

Options:
      --ipv6       Update the AAAA (IPv6) record as well
      --ttl <TTL>  TTL to set on updated records (seconds, min 60) [default: 60]
```

### Dynamic DNS

```sh
./hetzner_ddns              # update A record
./hetzner_ddns --ipv6       # also update AAAA
```

`update` requires the record to already exist — it updates records but does not
create them. IPv6 is opt-in; if no public IPv6 address can be determined the
AAAA step is skipped rather than failing.

To manage the zone apex, set `DNS_FQDN` to the zone name itself; the record is
then addressed as `@`.

Note that `--ttl` is applied on every run, so the record TTL changes to match
the flag even when the IP is unchanged. Pass the value your zone already uses
if you do not want it altered.

Pair it with cron or a systemd timer:

```cron
*/5 * * * * cd /opt/hetzner_ddns && ./hetzner_ddns --ipv6 >> /var/log/hetzner_ddns.log 2>&1
```

### ACME DNS-01

```sh
./hetzner_ddns set   example.com _acme-challenge.example.com <token>
./hetzner_ddns unset example.com _acme-challenge.example.com <token>
```

The first argument is the certificate domain. It is ignored — the zone is
derived from the validation name — but is accepted because TrueNAS passes it.

## TrueNAS integration

TrueNAS has **no native Hetzner DNS authenticator**. As of 25.10 the built-in
list is `cloudflare`, `digitalocean`, `route53`, `OVH`, and `shell`. Requests to
add more providers have consistently been declined, and `shell` is the intended
extension point.

Note that installing a certbot plugin such as `certbot-dns-hetzner` onto
TrueNAS does not work: the root filesystem is read-only, dev-mode changes do not
survive upgrades, and the authenticator list is a static Python list with no
plugin discovery. A `shell` script is the supported path.

### Setup

1. Copy the Linux amd64 binary (see [Cross-compiling](#cross-compiling-for-linux-x86-64))
   to a dataset **on a pool**. TrueNAS enforces this — scripts on the boot
   filesystem are rejected by `check_path_resides_within_volume`.

   ```sh
   scp target/x86_64-unknown-linux-musl/release/hetzner_ddns \
       truenas:/mnt/tank/scripts/hetzner-acme
   chmod 755 /mnt/tank/scripts/hetzner-acme
   ```

2. The script runs as an unprivileged user (default `nobody`) with an
   unspecified working directory, so a `.env` file will not be found. Wrap the
   binary to supply the token:

   ```sh
   #!/bin/sh
   # /mnt/tank/scripts/hetzner-acme.sh
   export HETZNER_API_TOKEN="your_api_token_here"
   export DNS_ZONE="example.com"
   exec /mnt/tank/scripts/hetzner-acme "$@"
   ```

   ```sh
   chmod 755 /mnt/tank/scripts/hetzner-acme.sh
   chmod 644 /mnt/tank/scripts/hetzner-acme     # token lives in the wrapper
   ```

   The wrapper contains a credential, so restrict it — `chmod 700` plus an owner
   the authenticator runs as, or accept that `nobody` must be able to read it.
   Consider a dedicated user rather than `nobody`.

3. In **Credentials → Certificates → ACME DNS Authenticators**, add one with
   Authenticator `shell`:

   | Field | Value |
   | --- | --- |
   | Script | `/mnt/tank/scripts/hetzner-acme.sh` |
   | User | `nobody` (or a dedicated user) |
   | Timeout | `60` |
   | Propagation delay | `60` |

4. Create an ACME certificate against a CSR and select this authenticator.

TrueNAS invokes the script as:

```
<script> set   <domain> <validation_name> <validation_content>
<script> unset <domain> <validation_name> <validation_content>
```

which matches the `set` / `unset` subcommands directly.

## Implementation notes

Behaviors that are easy to get wrong against this API, all verified against a
live zone:

- **TXT values must be double-quoted.** Unquoted values are rejected with
  `422 invalid_input`. The tool quotes and escapes automatically, and leaves
  already-quoted input alone.
- **`set` adds rather than replaces.** A wildcard plus base-domain certificate
  produces two simultaneous challenges on the same name; replacing the RRSet
  would invalidate one of them.
- **`unset` is idempotent.** A missing RRSet or a missing value both return
  `404`, which is treated as success so cleanup never fails a renewal.
- **Removing the last record deletes the RRSet**, so no separate cleanup is needed.
- **Mutations are asynchronous.** Each returns an action that the tool polls to
  completion, so success is not reported before the change has been applied.
- Zone resolution uses longest-suffix matching on label boundaries, so
  `notexample.com` will not match zone `example.com`.

## Exit codes

`0` on success, `1` on any error (missing credentials, zone or record not found,
a failed Hetzner action, or a non-2xx API response). API failures include the
HTTP status, URL, and response body.

## Notes

- Public IP is detected via `ipv4.icanhazip.com` / `ipv6.icanhazip.com`.
- Keep `.env` and any wrapper script containing a token out of version control.
  `.env` is already in `.gitignore`.
