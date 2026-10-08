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

The server runs in `keys` mode: clients send `Authorization: Bearer <client key>`, and miners connect with:

```sh
pool-miner --pool https://pool.example.com --token <miner token>
```

Status: `curl -H "Authorization: Bearer <admin token>" https://pool.example.com/admin/v1/status`.

To update, run `git pull && docker compose up -d --build`. The catalog is baked into the image from `config/models.json`. To change it without rebuilding, mount a file over `/app/config/models.json`, then restart.

Cloudflare's proxy (orange cloud) passes WebSockets and SSE through. Miners send a heartbeat every 10 s, which stays well inside Cloudflare's 100 s idle limit.

## Current deployment

`https://pool.example.com` runs from `/root/ai-pool/deploy` on 203.0.113.10, which also hosts example.com. On that host, HAProxy owns 80/443. It relays the hostnames in `/etc/haproxy/allowed-hosts.lst` and passes everything else, over the PROXY protocol, to the example Traefik on `127.0.0.1:8080/8443`. So the pool needed no HAProxy or Traefik changes: just its Traefik labels on `traefik_default`. The server has 2 GB of RAM, so the image is built elsewhere and loaded with `docker save | ssh … docker load`. The secrets live only in `/root/ai-pool/deploy/.env` (mode 600).
