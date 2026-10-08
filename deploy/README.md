# Deploying the pool server

`docker-compose.yml` runs `pool-server` behind an **existing Traefik**, the way
metalloobrabotka.online is set up: Traefik owns ports 80/443, redirects HTTP to
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
docker compose up -d --build
curl https://ai.metalloobrabotka.online/readyz
```

The server runs in `keys` mode: clients send `Authorization: Bearer <client key>`, and miners connect with:

```sh
pool-miner --pool https://ai.metalloobrabotka.online --token <miner token>
```

Status: `curl -H "Authorization: Bearer <admin token>" https://ai.metalloobrabotka.online/admin/v1/status`.

To update, run `git pull && docker compose up -d --build`. The catalog is baked into the image from `config/models.json`. To change it without rebuilding, mount a file over `/app/config/models.json`, then restart.

Cloudflare's proxy (orange cloud) passes WebSockets and SSE through. Miners send a heartbeat every 10 s, which stays well inside Cloudflare's 100 s idle limit.
