use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use zbus::address::Address;
use zbus::connection::{Builder, Connection};
use zbus::message::Message;

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Direction {
    FromApp,
    ToApp,
}

type Tap = Arc<dyn Fn(u64, Direction, &Message) + Send + Sync>;

/// The broker socket is used for service and control connections. Only the
/// separate app listener goes through `relay`, which is the observation point.
pub(crate) struct Endpoint {
    pub(crate) address: String,
    pub(crate) peer_address: Address,
    shutdown: CancellationToken,
    accept: JoinHandle<()>,
    broker: JoinHandle<()>,
    _directory: TempDir,
}

impl Endpoint {
    pub(crate) async fn start(tap: Tap) -> Result<Self, String> {
        let directory = tempfile::Builder::new()
            .prefix("wmc-dbus-")
            .tempdir_in("/tmp")
            .map_err(|error| error.to_string())?;
        let app_path = directory.path().join("app");
        let listener = UnixListener::bind(&app_path).map_err(|error| error.to_string())?;
        let broker_address = format!("unix:path={}", directory.path().join("bus").display());
        let shutdown = CancellationToken::new();
        let startup = shutdown.clone().drop_guard();
        let cancel_broker = shutdown.clone();
        let (ready, receive) = oneshot::channel();
        // busd uses zruntime. Drive its broker on a thread in this process.
        let broker = tokio::task::spawn_blocking(move || {
            zruntime::block_on(async move {
                let mut broker = match busd::bus::Bus::for_address(Some(&broker_address)).await {
                    Ok(broker) => broker,
                    Err(error) => {
                        let _ = ready.send(Err(error.to_string()));
                        return;
                    }
                };
                if ready.send(Ok(broker.address().clone())).is_ok() {
                    tokio::select! {
                        _ = cancel_broker.cancelled() => {},
                        result = broker.run() => {
                            if let Err(error) = result {
                                tracing::warn!(%error, "private D-Bus broker stopped");
                            }
                            cancel_broker.cancel();
                        },
                    }
                }
                let _ = broker.cleanup().await;
            })
        });
        let peer_address = tokio::time::timeout(Duration::from_secs(5), receive)
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())??;
        startup.disarm();
        let upstream = peer_address.clone();
        let stop_accept = shutdown.clone();
        let accept = tokio::spawn(async move {
            let mut relays = JoinSet::new();
            let mut next_id = 0u64;
            loop {
                tokio::select! {
                    _ = stop_accept.cancelled() => break,
                    _ = relays.join_next(), if !relays.is_empty() => {},
                    connection = listener.accept(), if relays.len() < 64 => {
                        let Ok((stream, _)) = connection else { break; };
                        let tap = tap.clone();
                        let upstream = upstream.clone();
                        next_id += 1;
                        let id = next_id;
                        relays.spawn(async move {
                            if let Err(error) = relay(stream, upstream, id, tap).await {
                                tracing::debug!(%error, "D-Bus app connection ended");
                            }
                        });
                    }
                }
            }
            relays.shutdown().await;
        });
        Ok(Self {
            address: format!("unix:path={}", app_path.display()),
            peer_address,
            shutdown,
            accept,
            broker,
            _directory: directory,
        })
    }

    pub(crate) async fn close(mut self) {
        self.shutdown.cancel();
        let _ = (&mut self.accept).await;
        let _ = (&mut self.broker).await;
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.accept.abort();
        self.shutdown.cancel();
    }
}

async fn relay(
    app: tokio::net::UnixStream,
    address: Address,
    id: u64,
    tap: Tap,
) -> zbus::Result<()> {
    let app = Builder::unix_stream(app.into_std()?)
        .server(
            address
                .guid()
                .ok_or_else(|| zbus::Error::Address("D-Bus broker has no GUID".into()))?
                .to_owned(),
        )
        .p2p()
        .build_message_stream();
    // A peer-to-peer client handshake leaves Hello to the real app connection.
    let bus = Builder::address(address).p2p().build_message_stream();
    let (app_stream, bus_stream) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::try_join!(app, bus) })
            .await
            .map_err(|_| {
                zbus::Error::InputOutput(Arc::new(std::io::Error::other(
                    "D-Bus handshake timed out",
                )))
            })??;
    let app = Connection::from(&app_stream);
    let bus = Connection::from(&bus_stream);
    tokio::select! {
        result = forward(app_stream, bus, id, Direction::FromApp, &tap) => result,
        result = forward(bus_stream, app, id, Direction::ToApp, &tap) => result,
    }
}

async fn forward(
    mut source: zbus::MessageStream,
    sink: Connection,
    id: u64,
    direction: Direction,
    tap: &Tap,
) -> zbus::Result<()> {
    while let Some(message) = source.next().await {
        let message = message?;
        // Queue the observation before forwarding, so a reply cannot be seen
        // before its call even when the two directions run independently.
        tap(id, direction, &message);
        tokio::time::timeout(Duration::from_secs(10), sink.send(&message))
            .await
            .map_err(|_| {
                zbus::Error::InputOutput(Arc::new(std::io::Error::other(
                    "D-Bus relay send timed out",
                )))
            })??;
    }
    Ok(())
}
