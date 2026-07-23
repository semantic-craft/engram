# HTTPS via a reverse proxy

> engram does **not** terminate TLS itself, by design. This page is
> the operator's guide to fronting it with a mature TLS terminator
> (Caddy, Cloudflare Tunnel, nginx) so tokens and `/web` cookies
> travel encrypted between clients and the server. Default install
> stays plain HTTP on loopback — no change for existing users on
> upgrade.

## When you don't need this

Skip TLS entirely if you're in one of these shapes — the security
budget is better spent elsewhere:

- **Single-user, stdio MCP transport.** `claude mcp add engram -- engram serve --transport stdio` never touches the network. No TLS to worry about.
- **Loopback-only HTTP server**, single user, no `/web` access from another machine. `127.0.0.1:49374` is unreachable from outside the host; TLS protects nothing here that the kernel's loopback boundary doesn't already.
- **Local dev / one-off experiments.** Bring TLS in when the deployment shape calls for it; not before.

The single-user happy path documented in the README's Quick Start is
this case. Most engram installs never need a proxy.

## When you do need this

Add a TLS-terminating proxy in front of engram when any of these
apply:

- **Multi-user mode is on** (`[auth].token_pepper` set, users created via `engram user add`). Per-user tokens travel between clients and the server — sniffable over plain HTTP on the LAN. See [`docs/users.md`](users.md).
- **The server is bound beyond loopback** (`ENGRAM_BIND=0.0.0.0:49374` or a LAN-routable IP). Anyone on the network segment sees plaintext token traffic and `/web` cookies.
- **You access `/web` from a different machine** than the one running engram. The browser session cookie set after Basic auth lives in the clear over HTTP.
- **You're exposing engram beyond the LAN.** Cloudflare Tunnel or a public-domain Caddy with Let's Encrypt are the two patterns most homelab operators land on.

engram will warn at startup when it binds to a non-loopback address
without auth, and again (one-shot) on the first request that didn't
arrive via `X-Forwarded-Proto: https`. The warnings are advisory —
the server doesn't refuse to start over plain HTTP. The decision to
add TLS is yours; this page is the recipes.

## Pick a path

| Path | Best for | What's needed externally |
|---|---|---|
| **Caddy + public domain + Let's Encrypt** | Operators with a domain name + port 80/443 reachable from the internet (most homelabs behind a forwarding router). | DNS A/AAAA record pointing at your IP. |
| **Caddy + internal CA (LAN-only)** | LAN-only multi-user, no public exposure. Each client machine has to trust Caddy's root cert once. | One-time root cert install per client. |
| **Cloudflare Tunnel** | "I don't want to open ports on my router" — outbound-only tunnel, TLS terminated at Cloudflare's edge. | A Cloudflare account (free tier works) + a domain on Cloudflare. |
| **External cert files (Caddy or nginx)** | You already have a corporate or homelab CA issuing certs to your services. | The cert/key files, however your environment produces them. |
| **nginx** | You already run nginx for other services and want one config language. | Same as Caddy: a domain or files. |

Each path runs your TLS terminator natively alongside `engram serve`
(or on any host that can reach engram's loopback port) — see
[Installing the proxy](#installing-the-proxy). The sections below walk
through each.

---

## Path 1 — Caddy + public domain + Let's Encrypt

Cleanest path when you have a domain and port 80/443 reachable. Caddy
auto-issues + auto-renews from Let's Encrypt with no operator
involvement after first start. Run Caddy on the same host as engram
(see [Installing the proxy](#installing-the-proxy)).

### Run engram

Bind engram to loopback — Caddy is the only process that listens on the
public interface and forwards to it:

```bash
engram serve --transport http --bind 127.0.0.1:49374
```

### Caddyfile

A complete one, three lines that actually matter:

```caddyfile
memory.example.com {
    reverse_proxy 127.0.0.1:49374
}
```

Caddy will:

1. Solve the HTTP-01 ACME challenge on first request to that hostname.
2. Issue a Let's Encrypt cert.
3. Renew automatically 30 days before expiry.
4. Forward `Authorization: Bearer ...` headers (and your auth flow) untouched.
5. Set `X-Forwarded-Proto: https` and `X-Forwarded-For: <client-ip>` automatically.

### engram environment

Set these in engram's process environment — export them in the shell or
service that launches `engram serve`, or set the equivalent keys in the
`config.toml` inside its data dir:

```bash
ENGRAM_AUTH_TOKEN=...long-random-token-from-generate-auth-token...
ENGRAM_ALLOWED_HOSTS=memory.example.com,localhost,127.0.0.1
```

The `ENGRAM_ALLOWED_HOSTS` must include the public hostname or
engram's DNS-rebinding guard will refuse Caddy's forwarded
requests.

### Hosting under a subpath

If engram shares a hostname with other apps, keep the prefix when proxying
and tell engram about it:

```bash
ENGRAM_BASE_PATH=/wiki
```

```caddyfile
memory.example.com {
    handle /wiki/* {
        reverse_proxy 127.0.0.1:49374
    }
}
```

Do **not** use `handle_path /wiki/*` for this deployment: it strips `/wiki`
before forwarding, while engram intentionally serves all routes under the
configured prefix. With the example above, clients use:

```bash
engram install-mcp   --client claude-code --apply \
    --server-url "https://memory.example.com/wiki/mcp" --auth-token "$ENGRAM_AUTH_TOKEN"
engram install-hooks --agent  claude-code --apply \
    --server-url "https://memory.example.com/wiki" --auth-token "$ENGRAM_AUTH_TOKEN"
```

The built-in browser is then at `https://memory.example.com/wiki/web`; add
`ENGRAM_WEB_SLUG=/` if you want the browser or custom `--web-ui-dir` SPA at
`https://memory.example.com/wiki` itself.

**Safety rules on both flags.** `ENGRAM_BASE_PATH` and
`ENGRAM_WEB_SLUG` go through the same normaliser. Segments must be
RFC 3986 unreserved characters (`[A-Za-z0-9-._~]`). Dot-segments
(`.` / `..`) are rejected — they mean "current" and "parent" at a
segment boundary, so accepting them would let a typo turn the prefix
into traversal. Anything outside the unreserved set falls back to a
root mount, and the startup log says why. The trailing-slash redirect
at `{base_path}{web_slug}/` keeps the query string on its way to the
canonical form.

### MCP client config (Claude Code shown — others follow the same shape)

```bash
engram install-mcp   --client claude-code --apply \
    --server-url "https://memory.example.com/mcp" --auth-token "$ENGRAM_AUTH_TOKEN"
engram install-hooks --agent  claude-code --apply \
    --server-url "https://memory.example.com" --auth-token "$ENGRAM_AUTH_TOKEN"
```

`https://` flips on, the token rides in `Authorization: Bearer`, and
Caddy's cert is browser/curl/MCP-client trusted everywhere because
Let's Encrypt is in every system trust store.

### What can go wrong

- **Port 80 not reachable from the internet** → ACME fails. Symptom: Caddy logs `Get "https://acme-v02.api.letsencrypt.org/...": ...` errors. Fix: forward 80 and 443 from your router to the Caddy host, OR switch to Cloudflare Tunnel (Path 3) which doesn't need open ports.
- **DNS not propagated yet** → first cert issuance fails with `unauthorized: ...DNS name does not have any address`. Fix: wait, or check the A record points at your public IP.
- **Cert renews silently fail months later** → Caddy logs the failure but you don't read Caddy logs. Fix: tail Caddy's own log (it records every renewal attempt and error) or front the endpoint with an uptime check.

---

## Path 2 — Caddy with internal CA (LAN-only)

You don't have a public domain or you don't want to expose anything
to the internet. Caddy's internal CA generates a per-server root cert
the operator installs **once** into each client machine's OS trust
store. Same wire shape as Path 1, no internet dependency, no port
forward.

### Caddyfile

```caddyfile
{
    local_certs   # tells Caddy to use the internal CA instead of LE
}

homelab.local, 192.168.1.50 {
    reverse_proxy 127.0.0.1:49374
}
```

List every name + IP clients will use (browser, MCP client, curl)
in the site address. Caddy puts all of them in the cert's SAN.

### The trust-install step (the load-bearing one)

Caddy's internal-CA root cert lives in Caddy's data dir on the host
running Caddy:

- macOS: `~/Library/Application Support/Caddy/pki/authorities/local/root.crt`
- Windows: `%AppData%\Caddy\pki\authorities\local\root.crt`

On the Caddy host, `caddy trust` installs it into that host's own trust
store. For every **other** client machine, copy `root.crt` off the host
and install it into that client's trust store:

| OS | Command |
|---|---|
| macOS | `sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain caddy-root.crt` |
| Linux (Debian/Ubuntu) | `sudo cp caddy-root.crt /usr/local/share/ca-certificates/ && sudo update-ca-certificates` |
| Linux (Arch/openSUSE) | `sudo trust anchor --store caddy-root.crt` |
| Windows | `certutil -addstore -f "Root" caddy-root.crt` (Administrator PowerShell) |
| iOS / Android | Email the file to the device, open it, install as a profile in Settings → General → VPN & Device Management. Then **also** explicitly trust it under Settings → General → About → Certificate Trust Settings. |

**The warning that has to be loud**: if you skip the trust-install
step on a client, that client will either refuse TLS connections
(MCP clients, curl) or train the operator to click through warnings
(browsers). In the latter case **you have neither HTTP's transparency
nor HTTPS's protection** — you have a security theatre cert that
makes everyone less safe. Install the root cert on every client
machine you connect from, or use Path 1 / Path 3 instead.

### Same environment + client config shape as Path 1

Substitute `https://homelab.local` (or whichever SAN you set) for the
public domain in the [engram environment](#engram-environment) and
client-config steps. Everything else is identical.

---

## Path 3 — Cloudflare Tunnel

Cloudflare's `cloudflared` daemon establishes an outbound-only tunnel
to Cloudflare's edge. **No ports open on your router**, no public IP
needed, TLS terminated at the Cloudflare edge with their cert. Pairs
particularly well with the homelab multi-user case because the trust
story is "Cloudflare is the CA" — universally trusted, no per-client
install dance.

### One-time Cloudflare setup

1. Have a domain on Cloudflare (the registrar can be elsewhere; the DNS must be on Cloudflare).
2. In the Cloudflare dashboard, go to **Zero Trust → Networks → Tunnels** → **Create a tunnel** → name it `engram-homelab` (or whatever) → save.
3. Cloudflare gives you a long token string. Save it for the install step.
4. Add a public hostname to the tunnel: `memory.example.com` → service `http://127.0.0.1:49374`. Save.
5. (Optional but recommended) Wrap the hostname in a **Cloudflare Access** application — Cloudflare's zero-trust SSO sits in front of the tunnel and you get human auth via Google/GitHub/etc. **on top of** engram's bearer token.

### Run engram + the tunnel

Bind engram to loopback and install `cloudflared` as a native service
pointed at it:

```bash
engram serve --transport http --bind 127.0.0.1:49374
cloudflared service install <TUNNEL_TOKEN_FROM_DASHBOARD>
```

`cloudflared service install` registers a LaunchDaemon on macOS or a
Windows service — it starts on boot, opens no inbound ports, and stores
the token in its own service config. The tunnel forwards to
`http://127.0.0.1:49374` (the dashboard step above); Cloudflare manages
the CNAME automatically, so there's no other DNS to configure. See
[Installing the proxy](#installing-the-proxy) for getting `cloudflared`
onto the host.

### engram environment

```bash
ENGRAM_AUTH_TOKEN=...long-random-token...
ENGRAM_ALLOWED_HOSTS=memory.example.com,localhost,127.0.0.1
```

### Client config

Same as Path 1:

```bash
engram install-mcp   --client claude-code --apply \
    --server-url "https://memory.example.com/mcp" --auth-token "$ENGRAM_AUTH_TOKEN"
```

### What can go wrong

- **Token leak**. Anyone with the tunnel token can run a tunnel for your hostname. Treat it like a secret — `cloudflared service install` writes it into its service config; keep that config readable only by the service account, and don't paste the token into anything you commit.
- **Tunnel down + cf cached old DNS** → Cloudflare returns 502 for a few minutes after restart. Usually self-heals.
- **Access policies confused with bearer auth**. Cloudflare Access (the optional SSO layer) is a separate layer from engram's bearer token. Both run; both must pass. If Access blocks a request, engram never sees it.

---

## Path 4 — External cert files (Caddy or nginx)

You already have a CA issuing certs to your services (corporate PKI,
homelab Vault, anything). You don't want Caddy issuing its own.

### Caddyfile

```caddyfile
memory.example.com {
    tls /path/to/certs/memory.crt /path/to/certs/memory.key
    reverse_proxy 127.0.0.1:49374
}
```

Point the `tls` directive at the cert and key files wherever your
environment writes them. Caddy hot-reloads the cert when those files
change — no restart required.

### nginx equivalent

```nginx
server {
    listen 443 ssl http2;
    server_name memory.example.com;

    ssl_certificate     /path/to/certs/memory.crt;
    ssl_certificate_key /path/to/certs/memory.key;

    location / {
        proxy_pass http://127.0.0.1:49374;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        # MCP Streamable HTTP transport is request-response; chunked
        # bodies and SSE both rely on the next two lines.
        proxy_http_version 1.1;
        proxy_set_header Connection "";
    }
}
```

The `proxy_http_version 1.1` + empty `Connection` are required for
MCP's Streamable HTTP transport to stream correctly.

---

## Installing the proxy

engram ships as a native binary for macOS (Apple Silicon) and Windows.
Run your TLS terminator natively on the same host as `engram serve` — or
on any box that can reach engram's loopback port — and point it at
`127.0.0.1:49374`.

**Caddy.** Install with `brew install caddy` on macOS or
`winget install CaddyServer.Caddy` on Windows. Drop the Caddyfile at
Caddy's canonical path (`/opt/homebrew/etc/Caddyfile` on macOS) or pass
it explicitly with `caddy run --config ./Caddyfile`. On macOS
`brew services start caddy` keeps it running in the background; on
Windows run `caddy run` under a service manager so it survives reboots.
Everything else (Let's Encrypt, internal CA, external certs) works the
same regardless of host OS — Caddy doesn't care what's in front of it.

**Cloudflare Tunnel.** Install with `brew install cloudflared` (macOS)
or `winget install Cloudflare.cloudflared` (Windows), then
`cloudflared service install <TUNNEL_TOKEN>` registers it as a
LaunchDaemon (macOS) or a Windows service. Same shape as Path 3.

---

## What engram does to support being behind a proxy

Nothing special — the server intentionally generates no absolute URLs
in responses, so it doesn't matter whether `https://` or `http://`
sits in front. The bearer token middleware reads `Authorization`
directly off the request, which proxies forward verbatim. The
`/api/v1` ETag is computed from request-independent fields. The
`/web` cookie set after Basic auth uses `SameSite=Lax` without
`Secure`, which lets it ride either transport — when fronted with
HTTPS, modern browsers automatically tighten the cookie to the proxy
origin's secure flag set anyway.

The only thing to mind: **`ENGRAM_ALLOWED_HOSTS` must include the
public hostname**, not just `localhost`. The host-allowlist middleware
runs before any header rewriting, so it sees the proxy's forwarded
`Host: memory.example.com` and would reject it otherwise.

## Don't paper over the security gap

Three things to actively avoid:

1. **Don't disable the allowed-hosts guard.** It's the DNS-rebinding defence; pruning it because the proxy "should be" filtering is exactly the kind of "the other layer handles it" assumption that ships bugs. Add the public hostname; don't widen to `*`.
2. **Don't skip the trust-install step in Path 2.** The temptation is to add `-k` (curl) or `--insecure` (MCP clients that support it) "just to get it working." If you do, you have a security theatre cert: TLS without authentication, which is worse than HTTP with the bearer because it looks safe and isn't.
3. **Don't run cloudflared with `--no-tls-verify`.** You won't need it — engram speaks plain HTTP on loopback, so there's no origin cert for cloudflared to verify. If you're reaching for the flag, something else is misconfigured.

If you can't take one of these paths cleanly, the honest answer is
"keep engram loopback-only" or "front it with the proxy you
already trust." The configuration that gives operators the wrong
mental model — looking secure, not being secure — is worse than
either.
