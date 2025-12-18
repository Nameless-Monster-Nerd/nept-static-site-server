# static-site-server-rs

Rust rewrite of the Go static site proxy. It mirrors the same flow:

- Reads `MINIO_ENDPOINT`, `MINIO_BUCKET`, `REDIS_MASTER`, optional `REDIS_PASS`, and `PORT` (default `8080`).
- Uses the request `Host` header as the Redis key; 404s when the key is missing.
- Proxies GETs to `http://{MINIO_ENDPOINT}/{MINIO_BUCKET}/uploads/{host}{path}`, defaulting `/` to `/index.html`.
- Forwards inbound headers to the upstream fetch and copies upstream headers back, setting `Cache-Control: public, max-age=3600`.

## Running

The app loads environment variables from `.env` (searched in the current directory and its parents), so it can reuse the existing file in the repository root.

```bash
cd static-site-server-rs
cargo run --release
```

## Notes

- If `REDIS_MASTER` lacks a port, `:6379` is appended to match the Go behavior.
- Redis auth uses `REDIS_PASS` when provided. Database index is fixed to `0`.
