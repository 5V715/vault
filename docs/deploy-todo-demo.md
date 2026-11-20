# Deploy the to-do demo (`vault-todo-demo` image)

The `vault-todo-demo` image runs the [to-do example](../examples/todo/README.md) on a
public host, so that a vault on a phone can pair with it from anywhere. It holds two
things:

- **vault-relay**, the mailbox relay at `/mailbox/<id>` (and `/healthz`);
- **the release page** (QR only, no copy button), served by the same vault-relay with
  `--static-dir`.

Both share one port (8080), plain HTTP. **TLS comes from a reverse proxy in front**:
the page is `https://todo.example` and its relay `wss://todo.example/mailbox/...`.
Examples for [Traefik](#traefik-docker-compose) and [Caddy](#caddy) follow.

The image is not published to a registry. Build it from a checkout:

```sh
docker buildx build -f Dockerfile.todo-demo -t vault-todo-demo --load .
# another platform (the image is built for linux/amd64, linux/arm64, linux/arm/v7):
docker buildx build -f Dockerfile.todo-demo --platform linux/arm64 -t vault-todo-demo --load .
```

The final stage is `scratch`. It contains `/vault-relay` and the page files in
`/srv/todo`, and no shell. It runs as UID 65532, writes nothing, and works with a
read-only root filesystem (`--read-only`, `read_only: true`).

## Configuration

| Variable | Flag | |
|---|---|---|
| `PUBLIC_ORIGIN` | `--public-origin` | **Required.** The origin the proxy serves the page at, e.g. `https://todo.example` (`https`; `http` only for `localhost`, `127.0.0.1`, `[::1]`). Without it the container exits at once with status 2 and says why. |
| `RELAY_ORIGIN` | `--relay-origin` | Optional. The relay the page dials. Default: the same host, `wss://<PUBLIC_ORIGIN host>`, which is this container. Give another relay as `wss://host[:port]`. That relay must allow `PUBLIC_ORIGIN`. |
| `VAULT_RELAY_ALLOW_ORIGIN` | `--allow-origin` | Optional. The browser origins allowed to open a mailbox. Default: `PUBLIC_ORIGIN`, so the page's own origin is the whole allowlist. Any other `Origin` gets HTTP 403. |
| | `--trust-proxy` | Take the client address for the per-IP limits from `X-Forwarded-For`. Pass it behind a proxy, see below. |
| `RUST_LOG` | | Log level, default `info`. |

A variable that is set but empty (`RELAY_ORIGIN=`) is an error, not "unset". Pass
flags after the image name (`docker run ... vault-todo-demo --trust-proxy`), or as the
compose `command`. The entrypoint already has `--static-dir /srv/todo`.

### `--trust-proxy` and `X-Forwarded-For`

The relay limits open mailboxes per client address. Behind a proxy, every connection
comes from the proxy's address, so all users would share one limit. With
`--trust-proxy` the relay uses the **first** `X-Forwarded-For` entry instead.

That is only safe if a client cannot choose that entry. Two conditions:

1. **The proxy overwrites `X-Forwarded-For`** for clients it does not trust, instead
   of appending to what they sent. Traefik (without
   `forwardedHeaders.trustedIPs` or `forwardedHeaders.insecure` on the entrypoint) and
   Caddy (without `trusted_proxies`) do this by default. If you add a CDN or load
   balancer in front, trust only its address ranges there.
2. **Nothing reaches port 8080 except the proxy.** In the examples the container
   publishes no port (Traefik) or only on `127.0.0.1` (Caddy). If 8080 is reachable
   from outside, anyone can send their own `X-Forwarded-For` and dodge the limit;
   then leave `--trust-proxy` out.

Origin checks do not depend on this: the `Origin` header comes from the browser and
the proxy passes it through unchanged.

## What the page gets

Every page response carries:

```
Content-Security-Policy: default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data:; connect-src 'self' wss://todo.example; base-uri 'none'; form-action 'none'; frame-ancestors 'none'
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
Cross-Origin-Opener-Policy: same-origin
Cache-Control: no-store
```

vault-relay computes the policy once at start from `PUBLIC_ORIGIN` and the relay
origin. The relay's WebSocket origin is the `connect-src`.

`/config.json` tells the page its relay, written from the same configuration:

```json
{"relay":"wss://todo.example"}
```

The page is built with `build.mjs --runtime-config`. It has no relay compiled in, so
one image works for any host. At start it fetches `/config.json` (same origin, no
credentials, no redirects). It accepts only a canonical `wss://host[:port]` (`ws://`
only for a loopback relay from an `http` page) and shows "Could not load the relay
configuration" otherwise.

**The one deliberate CSP deviation.** The to-do plan's policy has
`connect-src <relay>` only. The image adds `'self'` so that the page can fetch
`/config.json`. Everything else is the same policy. `'self'` lets the page
connect to its own origin, which in the default setup is this container anyway. It
opens no channel to any other host. This applies in image mode only. The localhost
and LAN builds (`build.mjs --relay ...`, `scripts/showcase.sh`) still compile the
relay in and keep `connect-src <relay>`. A runtime-config build has no `<meta>` CSP
(it cannot know the relay), so serve it only with vault-relay, which sends the header
on every response, including 404s. `serve.mjs` refuses such a build.

The static files are served read-only, `GET` and `HEAD` only:

- no directory listings;
- no dot-files;
- no percent-encoded or backslash paths;
- no symlinked files, and nothing outside `/srv/todo`;
- known types only.

`/mailbox/...` and `/healthz` are the relay's, as in plain `vault-relay`.

## Traefik (docker compose)

Traefik terminates TLS with a Let's Encrypt certificate and forwards to the container
over the compose network. WebSocket upgrades pass through a Traefik router without
extra configuration.

```yaml
# compose.yaml; DNS for todo.example points at this host, ports 80 and 443 are open.
services:
  traefik:
    image: traefik:v3 # pin a version and digest in production: traefik:v3.x.y@sha256:...
    command:
      - --providers.docker=true
      - --providers.docker.exposedbydefault=false
      - --entrypoints.web.address=:80
      - --entrypoints.web.http.redirections.entrypoint.to=websecure
      - --entrypoints.web.http.redirections.entrypoint.scheme=https
      - --entrypoints.websecure.address=:443
      - --certificatesresolvers.le.acme.email=admin@todo.example
      - --certificatesresolvers.le.acme.storage=/letsencrypt/acme.json
      - --certificatesresolvers.le.acme.tlschallenge=true
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
      - ./letsencrypt:/letsencrypt
    restart: unless-stopped

  todo:
    image: vault-todo-demo # built as above
    command: ["--trust-proxy"]
    environment:
      PUBLIC_ORIGIN: https://todo.example
    # No `ports:`: only Traefik reaches 8080 (see --trust-proxy above).
    read_only: true
    cap_drop: [ALL]
    security_opt: ["no-new-privileges:true"]
    labels:
      - traefik.enable=true
      - traefik.http.routers.todo.rule=Host(`todo.example`)
      - traefik.http.routers.todo.entrypoints=websecure
      - traefik.http.routers.todo.tls.certresolver=le
      - traefik.http.services.todo.loadbalancer.server.port=8080
    restart: unless-stopped
```

`docker compose up -d`, then check `https://todo.example/healthz` (`ok`) and
`https://todo.example/config.json`.

## Caddy

Caddy gets a certificate on its own and proxies WebSocket upgrades by default.

```sh
docker run -d --name todo --read-only --cap-drop ALL --security-opt no-new-privileges \
  -p 127.0.0.1:8080:8080 -e PUBLIC_ORIGIN=https://todo.example \
  vault-todo-demo --trust-proxy
```

```caddyfile
todo.example {
	reverse_proxy 127.0.0.1:8080
}
```

## Pair a phone

1. **Create the vault** for this page. Bind it to the app `vault-todo` and the
   page's origin:

   ```sh
   vaultctl init --app vault-todo --origin https://todo.example --dir ~/todo-vault
   ```

   On a phone this is the vault app's "create vault" for that app and origin, once the
   app is released. Until then, use `vaultctl` and `vault-dev` on a laptop as the phone
   stand-in. `init` shows the recovery key once: write it down.
2. **Open `https://todo.example`** in a browser and click **Connect to vault**. The page
   shows a QR code.
3. **Scan the QR** with the vault app. With the laptop stand-in, decode the QR instead
   (for example with a screenshot and `zbarimg --raw shot.png`, then delete the
   screenshot) and pass the text to vault-dev within about 60 seconds:

   ```sh
   vault-dev serve --dir ~/todo-vault --pair 'vault-pair:v1:...'
   ```

   vault-dev dials `wss://todo.example` with the public root certificates, so it
   needs no `--ca-file`.
4. **Compare the 6-digit codes** on both sides, approve in the vault (vault-dev: `y
   <prompt number>`), then click **Codes match** in the page.

The pairing text is a secret while the QR is shown. Anyone who has it can ask your
vault for a session. You still approve every session after comparing the codes.

The vault is bound to the exact origin. If you move the page to another host, create
the vault for the new origin.
