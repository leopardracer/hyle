use std::{
    collections::HashMap,
    io::ErrorKind,
    marker::PhantomData,
    net::{Ipv4Addr, SocketAddr},
};

use anyhow::Context;
use borsh::{BorshDeserialize, BorshSerialize};
use futures::{
    stream::{SplitSink, SplitStream},
    FutureExt, SinkExt, StreamExt,
};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::codec::{Decoder, Encoder, Framed};

use crate::{
    clock::TimestampMsClock,
    net::{TcpListener, TcpStream},
    tcp::{TcpMessage, TcpMessageCodec},
};
use tracing::{debug, error, trace, warn};

use super::{tcp_client::TcpClient, SocketStream, TcpEvent};

pub struct TcpServer<Codec, Req: Clone + std::fmt::Debug, Res: Clone + std::fmt::Debug> {
    tcp_listener: TcpListener,
    max_frame_length: Option<usize>,
    pool_sender: Sender<TcpEvent<Req>>,
    pool_receiver: Receiver<TcpEvent<Req>>,
    ping_sender: Sender<String>,
    ping_receiver: Receiver<String>,
    sockets: HashMap<String, SocketStream<Res>>,
    _codec: std::marker::PhantomData<Codec>,
}

impl<Codec, Req, Res> TcpServer<Codec, Req, Res>
where
    Codec: Decoder<Item = Req> + Encoder<Res> + Default + Send + 'static,
    <Codec as Decoder>::Error: std::fmt::Debug + Send,
    <Codec as Encoder<Res>>::Error: std::fmt::Debug + Send,
    Req: BorshDeserialize + Clone + Send + 'static + std::fmt::Debug,
    Res: BorshSerialize + Clone + Send + 'static + std::fmt::Debug,
{
    pub async fn start(port: u16, pool_name: String) -> anyhow::Result<Self> {
        Self::start_with_opts(port, None, pool_name).await
    }

    pub async fn start_with_opts(
        port: u16,
        max_frame_length: Option<usize>,
        pool_name: String,
    ) -> anyhow::Result<Self> {
        let tcp_listener = TcpListener::bind(&(Ipv4Addr::UNSPECIFIED, port)).await?;
        let (pool_sender, pool_receiver) = tokio::sync::mpsc::channel(100);
        let (ping_sender, ping_receiver) = tokio::sync::mpsc::channel(100);
        debug!(
            "Starting TcpConnectionPool {}, listening for stream requests on {} with max_frame_len: {:?}",
            &pool_name, port, max_frame_length
        );
        Ok(TcpServer::<Codec, Req, Res> {
            sockets: HashMap::new(),
            max_frame_length,
            tcp_listener,
            pool_sender,
            pool_receiver,
            ping_sender,
            ping_receiver,
            _codec: PhantomData::<Codec>,
        })
    }

    pub async fn listen_next(&mut self) -> Option<TcpEvent<Req>> {
        loop {
            tokio::select! {
                Ok((stream, socket_addr)) = self.tcp_listener.accept() => {
                    let codec = match self.max_frame_length {
                        Some(len) => TcpMessageCodec::<Codec>::new(len),
                        None => TcpMessageCodec::<Codec>::default()
                    };

                    let (sender, receiver) = Framed::new(stream, codec).split();
                    self.setup_stream(sender, receiver, &socket_addr.to_string());
                }

                Some(socket_addr) = self.ping_receiver.recv() => {
                    trace!("Received ping from {}", socket_addr);
                    if let Some(socket) = self.sockets.get_mut(&socket_addr) {
                        socket.last_ping = TimestampMsClock::now();
                    }
                }
                message = self.pool_receiver.recv() => {
                    return message;
                }
            }
        }
    }

    /// Local_addr of the underlying tcp_listener
    pub fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        self.tcp_listener
            .local_addr()
            .context("Getting local_addr from TcpListener in TcpServer")
    }

    /// Adresses of currently connected clients (no health check)
    pub fn connected_clients(&self) -> Vec<String> {
        self.sockets.keys().cloned().collect::<Vec<String>>()
    }

    pub async fn broadcast(&mut self, msg: Res) -> HashMap<String, anyhow::Error> {
        debug!("Broadcasting msg {:?} to all", msg);

        let mut tasks = vec![];

        for (name, socket) in self.sockets.iter_mut() {
            tasks.push(
                socket
                    .sender
                    .send(TcpMessage::Data(msg.clone()))
                    .map(|res| (name.clone(), res)),
            );
        }

        let all = futures::future::join_all(tasks).await;

        HashMap::from_iter(all.into_iter().filter_map(|(client_name, send_result)| {
            send_result.err().map(|error| {
                (
                    client_name.clone(),
                    anyhow::anyhow!("Sending message to client {}: {}", client_name, error),
                )
            })
        }))
    }
    pub async fn send_parallel(
        &mut self,
        socket_addrs: Vec<String>,
        msg: Res,
    ) -> HashMap<String, anyhow::Error> {
        debug!("Broadcasting msg {:?} to all", msg);

        // Getting targetted addrs that are not in the connected sockets list
        let unknown_socket_addrs = {
            let mut res = socket_addrs.clone();
            res.retain(|addr| !self.sockets.contains_key(addr));
            res
        };

        // Send the message to all targets concurrently and wait for them to finish
        let all_sent = {
            let mut tasks = vec![];
            for (name, socket) in self.sockets.iter_mut() {
                tasks.push(
                    socket
                        .sender
                        .send(TcpMessage::Data(msg.clone()))
                        .map(|res| (name.clone(), res)),
                );
            }
            futures::future::join_all(tasks).await
        };

        // Regroup future results in a map keyed with addrs
        let mut result = HashMap::from_iter(all_sent.into_iter().filter_map(
            |(client_name, send_result)| {
                send_result.err().map(|error| {
                    (
                        client_name.clone(),
                        anyhow::anyhow!("Sending message to client {}: {}", client_name, error),
                    )
                })
            },
        ));

        // Filling the map with errors for unknown targets
        for unknown in unknown_socket_addrs {
            result.insert(
                unknown.clone(),
                anyhow::anyhow!("Unknown socket_addr {}", unknown),
            );
        }

        result
    }
    pub async fn send(&mut self, socket_addr: String, msg: Res) -> anyhow::Result<()> {
        debug!("Sending msg {:?} to {}", msg, socket_addr);
        let stream = self
            .sockets
            .get_mut(&socket_addr)
            .context(format!("Retrieving client {}", socket_addr))?;

        stream
            .sender
            .send(TcpMessage::Data(msg))
            .await
            .map_err(|e| anyhow::anyhow!("Sending msg to client {}: {}", socket_addr, e))
    }

    pub async fn ping(&mut self, socket_addr: String) -> anyhow::Result<()> {
        let stream = self
            .sockets
            .get_mut(&socket_addr)
            .context(format!("Retrieving client {}", socket_addr))?;

        stream
            .sender
            .send(TcpMessage::Ping)
            .await
            .map_err(|e| anyhow::anyhow!("Sending ping to client {}: {}", socket_addr, e))
    }

    /// Setup stream in the managed list for a new client
    fn setup_stream(
        &mut self,
        mut sender: SplitSink<Framed<TcpStream, TcpMessageCodec<Codec>>, TcpMessage<Res>>,
        mut receiver: SplitStream<Framed<TcpStream, TcpMessageCodec<Codec>>>,
        socket_addr: &String,
    ) {
        // Start a task to process pings from the peer.
        // We do the processing in the main select! loop to keep things synchronous.
        // This makes it easier to store data in the same struct without mutexing.
        let ping_sender = self.ping_sender.clone();
        let pool_sender = self.pool_sender.clone();
        let cloned_socket_addr = socket_addr.clone();

        // This task is responsible for reception of ping and message.
        // If an error occurs and is not an InvalidData error, we assume the task is to be aborted.
        // If the stream is closed, we also assume the task is to be aborted.
        let abort_receiver_task = tokio::spawn(async move {
            loop {
                match receiver.next().await {
                    Some(Ok(TcpMessage::Ping)) => {
                        _ = ping_sender.send(cloned_socket_addr.clone()).await;
                    }
                    Some(Ok(TcpMessage::Data(data))) => {
                        debug!(
                            "Received data from socket {}: {:?}",
                            cloned_socket_addr, data
                        );
                        _ = pool_sender
                            .send(TcpEvent::Message {
                                dest: cloned_socket_addr.clone(),
                                data,
                            })
                            .await;
                    }
                    Some(Err(err)) => {
                        if err.kind() == ErrorKind::InvalidData {
                            error!("Received invalid data in socket {cloned_socket_addr} event loop: {err}",);
                        } else {
                            // If the error is not invalid data, we can assume the socket is closed.
                            warn!(
                                "Closing socket {} after error: {:?}",
                                cloned_socket_addr,
                                err.kind()
                            );
                            // Send an event indicating the connection is closed due to an error
                            _ = pool_sender
                                .send(TcpEvent::Error {
                                    dest: cloned_socket_addr.clone(),
                                    error: err.to_string(),
                                })
                                .await;
                            break;
                        }
                    }
                    None => {
                        // If we reach here, the stream has been closed.
                        warn!("Socket {} closed", cloned_socket_addr);
                        // Send an event indicating the connection is closed
                        _ = pool_sender
                            .send(TcpEvent::Closed {
                                dest: cloned_socket_addr.clone(),
                            })
                            .await;
                        break;
                    }
                }
            }
        });

        let (sender_snd, mut sender_recv) = tokio::sync::mpsc::channel(1000);

        let abort_sender_task = tokio::spawn({
            let cloned_socket_addr = socket_addr.clone();
            async move {
                while let Some(msg) = sender_recv.recv().await {
                    if let Err(e) = sender.send(msg).await {
                        error!("Sending message to peer {}: {}", cloned_socket_addr, e);
                        break;
                    }
                }
            }
        });

        tracing::debug!("Socket {} connected", socket_addr);
        // Store socket in the list.
        self.sockets.insert(
            socket_addr.to_string(),
            SocketStream {
                last_ping: TimestampMsClock::now(),
                sender: sender_snd,
                abort_sender_task,
                abort_receiver_task,
            },
        );
    }

    pub fn setup_client(&mut self, addr: String, tcp_client: TcpClient<Codec, Res, Req>) {
        let (sender, receiver) = tcp_client.split();
        self.setup_stream(sender, receiver, &addr);
    }

    pub fn drop_peer_stream(&mut self, peer_ip: String) {
        if let Some(peer_stream) = self.sockets.remove(&peer_ip) {
            peer_stream.abort_sender_task.abort();
            peer_stream.abort_receiver_task.abort();
            tracing::debug!("Client {} dropped & disconnected", peer_ip);
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::time::Duration;

    use crate::{
        tcp::{TcpEvent, TcpMessage},
        tcp_client_server,
    };

    use anyhow::Result;
    use borsh::{BorshDeserialize, BorshSerialize};
    use futures::TryStreamExt;

    #[derive(BorshDeserialize, BorshSerialize, Clone, Debug, PartialEq, Eq)]
    pub struct DataAvailabilityRequest(pub usize);

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub enum DataAvailabilityEvent {
        SignedBlock(String),
    }

    tcp_client_server! {
        DataAvailability,
        request: crate::tcp::tcp_server::tests::DataAvailabilityRequest,
        response: crate::tcp::tcp_server::tests::DataAvailabilityEvent
    }

    #[tokio::test]
    async fn tcp_test() -> Result<()> {
        let mut server = codec_data_availability::start_server(2345).await?;

        let mut client = codec_data_availability::connect("me".to_string(), "0.0.0.0:2345").await?;

        // Ping
        client.ping().await?;

        // Send data to server
        client.send(DataAvailabilityRequest(2)).await?;

        tokio::time::sleep(Duration::from_secs(1)).await;

        let d = match server.listen_next().await.unwrap() {
            TcpEvent::Message { data, .. } => data,
            _ => panic!("Expected a Message event"),
        };

        assert_eq!(DataAvailabilityRequest(2), d);
        assert!(server.pool_receiver.try_recv().is_err());

        // From server to client
        _ = server
            .broadcast(DataAvailabilityEvent::SignedBlock("blabla".to_string()))
            .await;

        assert_eq!(
            client.receiver.try_next().await.unwrap().unwrap(),
            TcpMessage::Data(DataAvailabilityEvent::SignedBlock("blabla".to_string()))
        );

        let client_socket_addr = server.connected_clients().first().unwrap().clone();

        server.ping(client_socket_addr).await?;

        assert_eq!(
            client.receiver.try_next().await.unwrap().unwrap(),
            TcpMessage::Ping
        );

        Ok(())
    }

    tcp_client_server! {
        bytes,
        request: Vec<u8>,
        response: Vec<u8>
    }

    #[tokio::test]
    async fn tcp_with_max_frame_length() -> Result<()> {
        let mut server = codec_bytes::start_server_with_opts(0, Some(100)).await?;

        let mut client = codec_bytes::connect_with_opts(
            "me".to_string(),
            Some(100),
            format!("0.0.0.0:{}", server.local_addr().unwrap().port()),
        )
        .await?;

        // Send data to server
        // A vec will be prefixed with 4 bytes (u32) containing the size of the payload
        // Here we reach 99 bytes < 100
        client.send(vec![0b_0; 95]).await?;

        let data = match server.listen_next().await.unwrap() {
            TcpEvent::Message { data, .. } => data,
            _ => panic!("Expected a Message event"),
        };

        assert_eq!(data.len(), 95);
        assert!(server.pool_receiver.try_recv().is_err());

        // Send data to server
        // Here we reach 100 bytes, it should explode the limit
        let sent = client.send(vec![0b_0; 96]).await;
        assert!(sent.is_err_and(|e| e.to_string().contains("frame size too big")));

        let mut client_relaxed = codec_bytes::connect(
            "me".to_string(),
            format!("0.0.0.0:{}", server.local_addr().unwrap().port()),
        )
        .await?;

        // Should be ok server side
        client_relaxed.send(vec![0b_0; 95]).await?;

        let data = match server.listen_next().await.unwrap() {
            TcpEvent::Message { data, .. } => data,
            _ => panic!("Expected a Message event"),
        };
        assert_eq!(data.len(), 95);

        // Should explode server side
        client_relaxed.send(vec![0b_0; 96]).await?;

        let received_data = server.listen_next().await;
        assert!(received_data.is_some_and(|tcp_event| matches!(tcp_event, TcpEvent::Closed { .. })));

        Ok(())
    }
}
