use std::{env, net::SocketAddr};

use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{
        HeaderMap, Request, Response, StatusCode,
        header::{CACHE_CONTROL, HOST},
        response::Builder,
    },
    response::IntoResponse,
    routing::get,
};
use dotenvy::dotenv;
use futures_util::StreamExt;
use redis::{AsyncCommands, aio::ConnectionManager};
use reqwest::Client;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct Config {
    port: u16,
    minio_endpoint: String,
    minio_bucket: String,
    redis_addr: String,
    redis_password: String,
}

impl Config {
    fn from_env() -> Result<Self> {
        dotenv().ok();
        let port = env::var("PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(8080);

        let mut redis_addr =
            env::var("REDIS_MASTER").context("missing required env var REDIS_MASTER")?;
        if !redis_addr.contains(':') {
            warn!(
                "redis address missing port, defaulting to :6379 for {}",
                redis_addr
            );
            redis_addr = format!("{redis_addr}:6379");
        }

        let minio_endpoint =
            env::var("MINIO_ENDPOINT").context("missing required env var MINIO_ENDPOINT")?;
        let minio_bucket =
            env::var("MINIO_BUCKET").context("missing required env var MINIO_BUCKET")?;
        let redis_password = env::var("REDIS_PASS").unwrap_or_default();

        if minio_endpoint.is_empty() || minio_bucket.is_empty() || redis_addr.is_empty() {
            return Err(anyhow!(
                "missing required env vars: MINIO_ENDPOINT, MINIO_BUCKET, REDIS_MASTER"
            ));
        }

        Ok(Self {
            port,
            minio_endpoint,
            minio_bucket,
            redis_addr,
            redis_password,
        })
    }

    fn redis_url(&self) -> String {
        if self.redis_password.is_empty() {
            format!("redis://{}/0", self.redis_addr)
        } else {
            format!("redis://:{}@{}/0", self.redis_password, self.redis_addr)
        }
    }
}

#[derive(Clone)]
struct AppState {
    config: Config,
    redis: ConnectionManager,
    client: Client,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let config = Config::from_env()?;
    info!(
        port = config.port,
        minio_endpoint = %config.minio_endpoint,
        minio_bucket = %config.minio_bucket,
        redis = %config.redis_addr,
        "starting server"
    );

    let redis_client =
        redis::Client::open(config.redis_url()).context("failed to build redis client")?;
    let redis = ConnectionManager::new(redis_client)
        .await
        .context("failed to connect to redis")?;

    let client = Client::builder()
        .user_agent("static-site-server-rs/0.1")
        .build()
        .context("failed to build http client")?;

    let state = AppState {
        config: config.clone(),
        redis,
        client,
    };

    let app = Router::new()
        .route("/healthz", get(health))
        .fallback(static_site_router)
        .with_state(state);

    let addr: SocketAddr = format!("0.0.0.0:{}", config.port)
        .parse()
        .context("invalid listen address")?;
    info!("listening on {}", addr);

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    axum::serve(listener, app).await.context("server error")?;

    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

async fn static_site_router(State(state): State<AppState>, req: Request<Body>) -> Response<Body> {
    let host = match req
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
    {
        Some(host) if !host.is_empty() => host.to_owned(),
        _ => return plain(StatusCode::BAD_REQUEST, "missing host header"),
    };

    let mut redis = state.redis.clone();
    let exists: Option<String> = match redis.get(&host).await {
        Ok(value) => value,
        Err(err) => {
            error!(%host, ?err, "redis lookup failed");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "upstream error");
        }
    };
    if exists.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let mut path = req.uri().path().to_owned();
    if path.is_empty() || path == "/" {
        path = "/index.html".to_string();
    }

    let upstream = format!(
        "http://{}/{}/uploads/{}{}",
        state.config.minio_endpoint, state.config.minio_bucket, host, path
    );
    info!(method = ?req.method(), %path, %upstream, "proxying request");

    let mut builder = state.client.get(&upstream);
    for (name, value) in req.headers().iter() {
        builder = builder.header(name, value);
    }

    let upstream_resp = match builder.send().await {
        Ok(resp) => resp,
        Err(err) => {
            error!(%upstream, ?err, "upstream fetch failed");
            return plain(StatusCode::BAD_GATEWAY, "upstream unavailable");
        }
    };

    build_response(upstream_resp)
}

fn build_response(upstream_resp: reqwest::Response) -> Response<Body> {
    let status = upstream_resp.status();
    let mut response_builder = Response::builder().status(status);

    copy_headers(upstream_resp.headers(), &mut response_builder);
    response_builder = response_builder.header(CACHE_CONTROL, "public, max-age=3600");

    let stream = upstream_resp
        .bytes_stream()
        .map(|chunk| chunk.map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err)));
    let body = Body::from_stream(stream);

    match response_builder.body(body) {
        Ok(response) => response,
        Err(err) => {
            error!(?err, "failed to build downstream response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn copy_headers(headers: &HeaderMap, builder: &mut Builder) {
    if let Some(dest) = builder.headers_mut() {
        for (name, value) in headers.iter() {
            dest.append(name, value.clone());
        }
    }
}

fn plain(status: StatusCode, body: impl Into<String>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(Body::from(body.into()))
        .unwrap_or_else(|err| {
            error!(?err, "failed to build error response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })
}
