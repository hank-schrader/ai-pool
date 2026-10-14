# Deploying the pool server

`docker-compose.yml` runs `pool-server` behind an **existing Traefik**, the way
example.com is set up: Traefik owns ports 80/443, redirects HTTP to
HTTPS, and gets a Let's Encrypt certificate through its Cloudflare DNS
challenge (`myresolver`). The pool container publishes no ports. It joins
Traefik's Docker network, and Traefik routes `POOL_HOST` to it, including the
miners' WebSocket and SSE streams.

```sh
git clone https://github.com/hank-schrader/ai-pool /opt/ai-pool
cd /opt/ai-pool/deploy
cp .env.example .env
docker network ls                  # find Traefik's network, set TRAEFIK_NETWORK
openssl rand -hex 32               # one per client key / miner token / admin token
$EDITOR .env
docker compose up -d --build     # or load a prebuilt image: docker save ai-pool-server:latest | ssh host docker load, then docker compose up -d
curl https://pool.example.com/readyz
```

Queue limits: `POOL_QUEUE_CAPACITY_GLOBAL` / `POOL_QUEUE_CAPACITY_PER_MODEL` unset (the default) mean automatic, 4 x the slots of the connected miners (at least 16); a number is a fixed operator cap. `POOL_MAX_ACTIVE_PER_CLIENT` / `POOL_MAX_QUEUED_PER_CLIENT` unset mean no per-client limit; a number is a fixed cap. Dispatch is FIFO. The container is limited to `POOL_MEMORY_LIMIT` (default 1g). See the main README.

The server runs in `keys` mode: clients send `Authorization: Bearer <client key>`. Miners need a token from `POOL_MINER_TOKENS`, unless `POOL_MINER_AUTH=open` lets anyone connect:

```sh
pool-miner --pool https://pool.example.com                         # open pool
pool-miner --pool https://pool.example.com --token <miner token>   # token pool
```

Status: `curl -H "Authorization: Bearer <admin token>" https://pool.example.com/admin/v1/status`.

The pool also serves a public live status page at `/`, with totals from `/v1/stats` and live SSE at `/v1/stats/stream`.

To update, run `git pull && docker compose up -d --build`. The catalog is baked into the image from `config/models.json`. To change it without rebuilding, mount a file over `/app/config/models.json`, then restart.

Cloudflare's proxy (orange cloud) passes WebSockets and SSE through. Miners send a heartbeat every 10 s, which stays well inside Cloudflare's 100 s idle limit.

## Current deployment

`https://pool.example.com` runs from `/root/ai-pool/deploy` on 203.0.113.10, which also hosts example.com. On that host, HAProxy owns 80/443. It relays the hostnames in `/etc/haproxy/allowed-hosts.lst` and passes everything else, over the PROXY protocol, to the example Traefik on `127.0.0.1:8080/8443`. So the pool needed no HAProxy or Traefik changes: just its Traefik labels on `traefik_default`. The server has 2 GB of RAM, so the image is built elsewhere and loaded with `docker save | ssh … docker load`. The secrets live only in `/root/ai-pool/deploy/.env` (mode 600). Since 2026-10-08 it runs with `POOL_MINER_AUTH=open`: miners connect anonymously, while clients still need an API key.

## Status page operations and development

The statistics stream cap is `POOL_MAX_STATS_STREAMS` (positive integer, default
128). Configure it in the server environment; excess connections receive 503 with
`Retry-After: 30`. The SSE response uses `no-store, no-transform`, with 15-second
comment keep-alives. Disable proxy buffering and caching on `/v1/stats/stream`.
The REST fallback uses `public, max-age=5`; HTML uses `no-cache`, and generated
hashed assets use a one-year immutable cache. See [the protocol](../docs/protocol.md)
for the version 1 snapshot/tick schema, completed-bucket semantics, and sequencing.
A restart changes the epoch UUID and clears in-memory history; clients replace
old data with a new snapshot. Every connection begins with a snapshot and ignores
`Last-Event-ID`.

Production assets are committed in `crates/pool-server/static/dist/` and embedded
by Rust's build script. Docker and release builds require no Node. To regenerate:

```sh
cd web
nvm install                 # .nvmrc pins Node 24.13.0
nvm use
npm install --global npm@11.6.2
npm ci
npm run check
npm test
npm run build               # clean production build, then <= 50 KB gzip budget
cd ..
git status --short --untracked-files=all -- crates/pool-server/static/dist
```

Commit source changes, lockfile, and all generated asset additions/removals
together. CI rebuilds and rejects any tracked or untracked dist difference.

For local development, start a separate loopback server (local auth) on port
8080, then run `npm run dev` in `web/`. Vite proxies `/v1` to
`http://127.0.0.1:8080`. Use a free port and adjust `web/vite.config.ts`'s local
proxy if another server is already using 8080. Vite's development asset handling
is separate from the embedded production page.

Before deployment, check the **production** page in a browser: no CSP violations
or inline styles; Live updates about once per second; light/dark/auto themes,
mobile layout, range buttons, both sparklines, legend, end label, tooltip and
keyboard controls (arrows, Shift+arrows, Home, End, Escape); table text includes
so-far and incomplete-startup labels. Disconnect networking and confirm Offline
with retained data and polling; restore it and confirm snapshot replacement.
Hide/show the tab and inspect network requests for stream closure/reconnection.
Restart a local server and confirm that its new epoch replaces old history.
Check `/`, its generated JS/CSS URLs, `/v1/stats`, and `curl -N` against
`/v1/stats/stream` for initial snapshot, ticks, and cache/security headers.
