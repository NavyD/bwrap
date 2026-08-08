use anyhow::{Result, bail};
use axum::{
    Router,
    extract::{Request, State},
    middleware,
    response::Response,
};
use axum_reverse_proxy::ReverseProxy;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::{
    net, process,
    sync::{Mutex, watch},
    time,
};
use url::Url;

#[derive(Debug)]
pub struct BWAgentConfig {
    pub idle_lock_timeout: time::Duration,
    pub bw_path: PathBuf,
    pub listen_url: String,
}

pub async fn start(args: BWAgentConfig) -> Result<()> {
    let bw_serve_url = get_bw_serve_url().await?;
    // 启动 bw serve 进程并检查问题
    let bw_child = spawn_bw_serve(&args.bw_path, &bw_serve_url).await?;

    let listen_url = args.listen_url.parse::<Url>()?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let state = AppState {
        args: Arc::new(args),
        idle_lock_task: Arc::new(Mutex::new(None)),
        bw_serve_child: Arc::new(Mutex::new(Some(bw_child))),
        bw_serve_url: Arc::new(bw_serve_url),
        shutdown_tx,
    };
    let app = build_router(state).await?;

    let addr =
        listen_url.socket_addrs(|| listen_url.port_or_known_default())?;
    tracing::info!(addr = ?addr, "tcp serving");
    let listener = net::TcpListener::bind(&*addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.changed().await;
        })
        .await?;
    Ok(())
}

async fn shutdown_handler(State(s): State<AppState>) -> axum::http::StatusCode {
    let _ = s.shutdown_tx.send(true);
    axum::http::StatusCode::OK
}

async fn build_router(state: AppState) -> Result<Router> {
    let mut app = Router::<AppState>::new();
    app = app.route("/__bwrap/shutdown", axum::routing::post(shutdown_handler));
    app = match state.bw_serve_url.scheme() {
        "http" | "https" => {
            app.merge(ReverseProxy::new("/", state.bw_serve_url.as_str()))
        }
        _ => bail!("Unsupported scheme for url {}", state.bw_serve_url),
    };
    let app = app
        .layer(axum_reverse_proxy::RetryLayer::with_delay(
            5,
            time::Duration::from_millis(500),
        ))
        .layer(middleware::map_request_with_state(
            state.clone(),
            middleware_start_bw_serve,
        ))
        .layer(middleware::map_response_with_state(
            state.clone(),
            middleware_idle_lock,
        ))
        .with_state(state.clone());
    Ok(app)
}

async fn get_bw_serve_url() -> Result<Url> {
    let hostname = "127.0.0.1";
    let port = net::TcpListener::bind((hostname, 0))
        .await?
        .local_addr()?
        .port();
    let url = format!("http://{}:{}", hostname, port).parse::<Url>()?;
    Ok(url)
}

async fn spawn_bw_serve(bw_path: &Path, url: &Url) -> Result<process::Child> {
    let mut args =
        vec!["serve", "--hostname", url.host_str().unwrap_or("localhost")];
    let port_str = url.port_or_known_default().map(|v| v.to_string());
    if let Some(p) = &port_str {
        args.extend_from_slice(&["--port", p]);
    }

    tracing::info!(
        bw_path = ?bw_path,
        url = %url,
        args = ?args,
        "spawning bw serve"
    );
    let child = process::Command::new(bw_path)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    Ok(child)
}

#[derive(Debug, Clone)]
struct AppState {
    idle_lock_task: Arc<Mutex<Option<IdleLockTask>>>,
    bw_serve_child: Arc<Mutex<Option<process::Child>>>,
    args: Arc<BWAgentConfig>,
    bw_serve_url: Arc<Url>,
    shutdown_tx: watch::Sender<bool>,
}

#[derive(Debug)]
struct IdleLockTask {
    deadline_tx: watch::Sender<time::Instant>,
}

async fn middleware_start_bw_serve<B>(
    State(s): State<AppState>,
    req: Request<B>,
) -> Request<B> {
    let mut lock = s.bw_serve_child.lock().await;
    // 如果 bw serve 进程不存在或已终止
    if lock
        .as_mut()
        .map(|v| v.try_wait().ok().flatten().is_some())
        .unwrap_or(true)
    {
        match spawn_bw_serve(&s.args.bw_path, &s.bw_serve_url).await {
            Ok(d) => {
                *lock = Some(d);
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to spawn bw serve");
            }
        }
    }
    req
}
async fn middleware_idle_lock<B>(
    State(mut s): State<AppState>,
    resp: Response<B>,
) -> Response<B> {
    idle_lock(&mut s).await;
    resp
}

async fn idle_lock(s: &mut AppState) {
    let mut lock = s.idle_lock_task.lock().await;
    let deadline = time::Instant::now() + s.args.idle_lock_timeout;
    if let Some(t) = &*lock {
        let Err(e) = t.deadline_tx.send(deadline) else {
            return;
        };
        tracing::info!(error = %e, state = ?s, "redo idle lock task when sending error")
    }

    let (tx, mut rx) = watch::channel(deadline);
    let s = s.clone();
    tokio::spawn(async move {
        let mut deadline = *rx.borrow();
        loop {
            tracing::trace!(deadline = ?deadline, "idle sleepping");
            tokio::select! {
                _ = time::sleep_until(deadline) => {
                    tracing::debug!("idle task completed");
                    break;
                }
                Ok(()) = rx.changed() => {
                    deadline = *rx.borrow();
                    tracing::debug!(
                        deadline = ?deadline,
                        "received new deadline"
                    );
                }
            }
        }
        // 移除 bw serve 进程
        let mut lock = s.bw_serve_child.lock().await;
        tracing::debug!("killing child");
        if let Some(mut child) = lock.take()
            && let Err(e) = child.kill().await
        {
            tracing::error!(error = %e, "failed to killing bw serve daemon");
        }
    });
    *lock = Some(IdleLockTask { deadline_tx: tx });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use std::time::Duration;

    fn test_state(shutdown_tx: watch::Sender<bool>) -> AppState {
        AppState {
            args: Arc::new(BWAgentConfig {
                idle_lock_timeout: Duration::from_secs(60),
                bw_path: PathBuf::from("/nonexistent/bw"),
                listen_url: "http://localhost:8087".to_string(),
            }),
            idle_lock_task: Arc::new(Mutex::new(None)),
            bw_serve_child: Arc::new(Mutex::new(None)),
            bw_serve_url: Arc::new("http://127.0.0.1:1".parse().unwrap()),
            shutdown_tx,
        }
    }

    #[tokio::test]
    async fn shutdown_handler_sends_signal() {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let state = test_state(shutdown_tx);
        let status = shutdown_handler(State(state)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(shutdown_rx.changed().await.is_ok());
        assert!(*shutdown_rx.borrow());
    }

    #[tokio::test]
    async fn shutdown_route_returns_200_and_stops_server() {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let state = test_state(shutdown_tx);
        let app = build_router(state).await.unwrap();
        let listener = net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.changed().await;
                })
                .await
                .unwrap();
        });
        let resp = reqwest::Client::new()
            .post(format!("http://{}/__bwrap/shutdown", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
