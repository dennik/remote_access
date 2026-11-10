//! Network front end: LAN discovery, TLS, viewer login and session dispatch.

use crate::config::{self, Store};
use crate::{platform, session};
use anyhow::{anyhow, bail, Result};
use rd_common::proto::{read_msg, write_msg, AuthMsg, DiscoveryReply};
use rd_common::{auth, tls, DISCOVERY_REQUEST};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::AbortHandle;
use tokio_rustls::server::TlsStream;
use tokio::net::TcpStream;

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Foreground mode: the session runs in this process, in the current user's desktop.
pub fn run_console(store: Store, bind: &str, port: u16) -> Result<()> {
    platform::set_dpi_aware();
    let runtime = tokio::runtime::Runtime::new()?;
    let addr: SocketAddr = format!("{bind}:{port}").parse()?;
    runtime.block_on(serve(store, addr, None, |stream, store| async move {
        session::run(stream, std::sync::Arc::new(move |line: &str| store.log(line))).await
    }))
}

/// Accepts viewers, logs them in, and hands each session to `handler`.
/// `discovery` is (UDP port, TCP port to advertise).
pub async fn serve<F, Fut>(store: Store, addr: SocketAddr, discovery: Option<(u16, u16)>, handler: F) -> Result<()>
where
    F: Fn(TlsStream<TcpStream>, Store) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let identity = store.identity()?;
    let fingerprint = Arc::new(identity.fingerprint.clone());
    let acceptor = tls::acceptor(identity.cert_der, identity.key_der)?;
    let listener = TcpListener::bind(addr).await?;
    println!("listening on {}", listener.local_addr()?);

    if let Some((udp_port, tcp_port)) = discovery {
        let reply = DiscoveryReply { name: config::hostname(), port: tcp_port, fingerprint: fingerprint.to_string() };
        tokio::spawn(answer_discovery(udp_port, reply));
    }

    // One viewer at a time: a new login takes over from the previous one.
    let current: Arc<Mutex<Option<AbortHandle>>> = Arc::new(Mutex::new(None));

    loop {
        let (socket, peer) = listener.accept().await?;
        let _ = socket.set_nodelay(true);
        let (acceptor, store, fingerprint, handler, current) =
            (acceptor.clone(), store.clone(), fingerprint.clone(), handler.clone(), current.clone());

        tokio::spawn(async move {
            let login = tokio::time::timeout(AUTH_TIMEOUT, async {
                let mut stream = acceptor.accept(socket).await?;
                let key = authenticate(&mut stream, &store, &fingerprint).await?;
                Ok::<_, anyhow::Error>((stream, key))
            })
            .await;

            let (stream, key) = match login {
                Ok(Ok(ok)) => ok,
                Ok(Err(e)) => {
                    store.log(&format!("{peer} login failed: {e}"));
                    return;
                }
                Err(_) => {
                    store.log(&format!("{peer} login timed out"));
                    return;
                }
            };

            let key = hex::encode(&key[..8]);
            store.log(&format!("{peer} connected with key {key}"));
            let log_store = store.clone();
            let task = tokio::spawn(handler(stream, log_store));
            if let Some(old) = current.lock().unwrap().replace(task.abort_handle()) {
                old.abort();
            }
            let end = match task.await {
                Ok(Ok(())) => "disconnected".to_string(),
                Ok(Err(e)) => format!("disconnected: {e}"),
                Err(_) => "replaced by a newer session".to_string(),
            };
            store.log(&format!("{peer} {end}"));
        });
    }
}

async fn authenticate<S>(stream: &mut S, store: &Store, fingerprint: &str) -> Result<[u8; 32]>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).map_err(|e| anyhow!("system RNG failed: {e}"))?;
    write_msg(stream, &AuthMsg::Challenge { nonce, name: config::hostname() }).await?;

    let AuthMsg::Response { public_key, signature } = read_msg(stream).await? else {
        bail!("expected a login response");
    };
    // Read per login, so adding a key doesn't need a service restart.
    let allowed = store.authorized_keys()?.contains(&public_key);
    if !allowed || !auth::verify(&public_key, &nonce, fingerprint, &signature) {
        write_msg(stream, &AuthMsg::Denied).await?;
        bail!("key {} is not authorized", hex::encode(&public_key[..8]));
    }
    write_msg(stream, &AuthMsg::Accepted).await?;
    Ok(public_key)
}

async fn answer_discovery(port: u16, reply: DiscoveryReply) {
    let Ok(socket) = UdpSocket::bind(("0.0.0.0", port)).await else { return };
    let Ok(body) = postcard::to_stdvec(&reply) else { return };
    let mut buf = [0u8; 32];
    loop {
        let Ok((n, from)) = socket.recv_from(&mut buf).await else { continue };
        if &buf[..n] == DISCOVERY_REQUEST {
            let _ = socket.send_to(&body, from).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("rdagent-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::new(dir)
    }

    async fn run_login(store: &Store, seed: [u8; 32], sign_fp: &str) -> (Result<[u8; 32]>, AuthMsg) {
        let (mut server, mut client) = tokio::io::duplex(4096);
        let server_store = store.clone();
        let agent = tokio::spawn(async move { authenticate(&mut server, &server_store, "real-fp").await });

        let AuthMsg::Challenge { nonce, .. } = read_msg(&mut client).await.unwrap() else { panic!("no challenge") };
        let response = AuthMsg::Response { public_key: auth::public_key(&seed), signature: auth::sign(&seed, &nonce, sign_fp) };
        write_msg(&mut client, &response).await.unwrap();
        let verdict: AuthMsg = read_msg(&mut client).await.unwrap();
        (agent.await.unwrap(), verdict)
    }

    #[tokio::test]
    async fn only_listed_keys_signing_the_real_fingerprint_get_in() {
        let store = temp_store("login");
        let good = auth::generate_seed().unwrap();
        store.add_authorized_keys(&[hex::encode(auth::public_key(&good))]).unwrap();

        let (result, verdict) = run_login(&store, good, "real-fp").await;
        assert!(result.is_ok());
        assert!(matches!(verdict, AuthMsg::Accepted));

        let (result, verdict) = run_login(&store, auth::generate_seed().unwrap(), "real-fp").await;
        assert!(result.is_err(), "unlisted key");
        assert!(matches!(verdict, AuthMsg::Denied));

        let (result, verdict) = run_login(&store, good, "mitm-fp").await;
        assert!(result.is_err(), "signature made for another certificate");
        assert!(matches!(verdict, AuthMsg::Denied));
    }

    #[tokio::test]
    async fn empty_key_list_denies_everyone() {
        let store = temp_store("empty");
        let (result, verdict) = run_login(&store, auth::generate_seed().unwrap(), "real-fp").await;
        assert!(result.is_err());
        assert!(matches!(verdict, AuthMsg::Denied));
    }
}
