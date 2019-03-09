use super::packet::*;
use bytes::BytesMut;
use futures::{
    future::err,
    sync::mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
    sync::oneshot,
    Future, Sink, Stream,
};
use hashbrown::HashMap;
use interledger_packet::{ErrorCode, Fulfill, Packet, Prepare, Reject, RejectBuilder};
use interledger_service::*;
use parking_lot::{Mutex, RwLock};
use rand::random;
use std::{
    io::{Error as IoError, ErrorKind},
    iter::IntoIterator,
    marker::PhantomData,
    sync::Arc,
};
use stream_cancel::Valved;
use tokio_executor::spawn;
use tokio_tcp::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tungstenite::{error::Error as WebSocketError, Message};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

type IlpResultChannel = oneshot::Sender<Result<Fulfill, Reject>>;
type IncomingRequestBuffer<A> = UnboundedReceiver<(A, u32, Prepare)>;

#[derive(Clone)]
pub struct BtpOutgoingService<T, A: Account> {
    // TODO support multiple connections per account
    connections: Arc<RwLock<HashMap<A::AccountId, UnboundedSender<Message>>>>,
    pending_outgoing: Arc<Mutex<HashMap<u32, IlpResultChannel>>>,
    pending_incoming: Arc<Mutex<Option<IncomingRequestBuffer<A>>>>,
    incoming_sender: UnboundedSender<(A, u32, Prepare)>,
    next_outgoing: T,
}

impl<T, A> BtpOutgoingService<T, A>
where
    T: OutgoingService<A> + Clone,
    A: Account + 'static,
{
    pub fn new(next_outgoing: T) -> Self {
        let (incoming_sender, incoming_receiver) = unbounded();
        BtpOutgoingService {
            connections: Arc::new(RwLock::new(HashMap::new())),
            pending_outgoing: Arc::new(Mutex::new(HashMap::new())),
            pending_incoming: Arc::new(Mutex::new(Some(incoming_receiver))),
            incoming_sender,
            next_outgoing,
        }
    }

    pub(crate) fn add_connection(&self, account: A, connection: WsStream) {
        let account_id = account.id();

        // Set up a channel to forward outgoing packets to the WebSocket connection
        let (tx, rx) = unbounded();
        let (sink, stream) = connection.split();
        let (close_connection, stream) = Valved::new(stream);
        let forward_to_connection = sink
            .send_all(
                rx.map_err(|_err| {
                    WebSocketError::from(IoError::from(ErrorKind::ConnectionAborted))
                }),
            )
            .then(move |_| {
                debug!("Finished forwarding to WebSocket stream");
                drop(close_connection);
                Ok(())
            });

        // Set up a listener to handle incoming packets from the WebSocket connection
        // TODO do we need all this cloning?
        let pending_requests = self.pending_outgoing.clone();
        let incoming_sender = self.incoming_sender.clone();
        let handle_incoming = stream.map_err(|_err| ()).for_each(move |message| {
          // Handle the packets based on whether they are an incoming request or a response to something we sent
          match parse_ilp_packet(message) {
            Ok((request_id, Packet::Prepare(prepare))) => {
                incoming_sender.clone().unbounded_send((account.clone(), request_id, prepare))
                    .map_err(|err| error!("Unable to buffer incoming request: {:?}", err))
            },
            Ok((request_id, Packet::Fulfill(fulfill))) => {
              if let Some(channel) = (*pending_requests.lock()).remove(&request_id) {
                channel.send(Ok(fulfill)).map_err(|fulfill| error!("Error forwarding Fulfill packet back to the Future that sent the Prepare: {:?}", fulfill))
              } else {
                warn!("Got Fulfill packet that does not match an outgoing Prepare we sent: {:?}", fulfill);
                Ok(())
              }
            }
            Ok((request_id, Packet::Reject(reject))) => {
              if let Some(channel) = (*pending_requests.lock()).remove(&request_id) {
                channel.send(Err(reject)).map_err(|reject| error!("Error forwarding Reject packet back to the Future that sent the Prepare: {:?}", reject))
              } else {
                warn!("Got Reject packet that does not match an outgoing Prepare we sent: {:?}", reject);
                Ok(())
              }
            },
            Err(_) => {
              debug!("Unable to parse ILP packet from BTP packet (if this is the first time this appears, the packet was probably the auth response)");
              // TODO Send error back
              Ok(())
            }
          }
        });

        let connections = self.connections.clone();
        let handle_connection = handle_incoming
            .select(forward_to_connection)
            .then(move |_| {
                let mut connections = connections.write();
                connections.remove(&account_id);
                debug!(
                    "WebSocket connection closed for account {} ({} connections still open)",
                    account_id,
                    connections.len()
                );
                Ok(())
            });
        spawn(handle_connection);

        // Save the sender side of the channel so we have a way to forward outgoing requests to the WebSocket
        self.connections.write().insert(account_id, tx);
    }

    pub fn handle_incoming<S>(self, incoming_handler: S) -> BtpService<S, T, A>
    where
        S: IncomingService<A> + Clone + Send + 'static,
    {
        // Any connections that were added to the BtpOutgoingService will just buffer
        // the incoming Prepare packets they get in self.pending_incoming
        // Now that we're adding an incoming handler, this will spawn a task to read
        // all Prepare packets from the buffer, handle them, and send the responses back
        let mut incoming_handler_clone = incoming_handler.clone();
        let connections_clone = self.connections.clone();
        let handle_pending_incoming = self
            .pending_incoming
            .lock()
            .take()
            .expect("handle_incoming can only be called once")
            .for_each(move |(account, request_id, prepare)| {
                let account_id = account.id();
                let connections_clone = connections_clone.clone();
                incoming_handler_clone
                    .handle_request(IncomingRequest {
                        from: account,
                        prepare,
                    })
                    .then(move |result| {
                        let packet = match result {
                            Ok(fulfill) => Packet::Fulfill(fulfill),
                            Err(reject) => Packet::Reject(reject),
                        };
                        let message = ilp_packet_to_ws_message(request_id, packet);
                        connections_clone
                            .read()
                            .get(&account_id)
                            .expect(
                                "No connection for account (something very strange has happened)",
                            )
                            .clone()
                            .unbounded_send(message)
                            .map_err(|err| {
                                error!(
                                    "Error sending response to account: {} {:?}",
                                    account_id, err
                                )
                            })
                    })
            });
        spawn(handle_pending_incoming);

        BtpService {
            outgoing: self,
            incoming_handler_type: PhantomData,
        }
    }
}

impl<T, A> OutgoingService<A> for BtpOutgoingService<T, A>
where
    T: OutgoingService<A> + Clone,
    A: Account + 'static,
{
    type Future = BoxedIlpFuture;

    fn send_request(&mut self, request: OutgoingRequest<A>) -> Self::Future {
        if let Some(connection) = (*self.connections.read()).get(&request.to.id()) {
            let request_id = random::<u32>();

            match connection.unbounded_send(ilp_packet_to_ws_message(
                request_id,
                Packet::Prepare(request.prepare),
            )) {
                Ok(_) => {
                    let (sender, receiver) = oneshot::channel();
                    (*self.pending_outgoing.lock()).insert(request_id, sender);
                    Box::new(
                        receiver
                            .map_err(|_| {
                                RejectBuilder {
                                    code: ErrorCode::T00_INTERNAL_ERROR,
                                    message: &[],
                                    triggered_by: &[],
                                    data: &[],
                                }
                                .build()
                            })
                            .and_then(|result| match result {
                                Ok(fulfill) => Ok(fulfill),
                                Err(reject) => Err(reject),
                            }),
                    )
                }
                Err(send_error) => {
                    error!("Error sending websocket message: {:?}", send_error);
                    let reject = RejectBuilder {
                        code: ErrorCode::T00_INTERNAL_ERROR,
                        message: &[],
                        triggered_by: &[],
                        data: &[],
                    }
                    .build();
                    Box::new(err(reject))
                }
            }
        } else {
            debug!(
                "No open connection for account: {}, forwarding request to the next service",
                request.to.id()
            );
            Box::new(self.next_outgoing.send_request(request))
        }
    }
}

#[derive(Clone)]
pub struct BtpService<S, T, A: Account> {
    outgoing: BtpOutgoingService<T, A>,
    incoming_handler_type: PhantomData<S>,
}

impl<S, T, A> BtpService<S, T, A>
where
    S: IncomingService<A> + Clone + Send + 'static,
    T: OutgoingService<A> + Clone,
    A: Account + 'static,
{
    pub(crate) fn new(incoming_handler: S, next_outgoing: T) -> Self {
        let (incoming_sender, incoming_receiver) = unbounded();
        BtpOutgoingService {
            connections: Arc::new(RwLock::new(HashMap::new())),
            pending_outgoing: Arc::new(Mutex::new(HashMap::new())),
            // pending_incoming is only needed when the service is created as a BtpOutgoingService
            pending_incoming: Arc::new(Mutex::new(Some(incoming_receiver))),
            // incoming_handler,
            incoming_sender,
            next_outgoing,
        }
        .handle_incoming(incoming_handler)
    }

    pub(crate) fn add_connection(&self, account: A, connection: WsStream) {
        self.outgoing.add_connection(account, connection)
    }
}

impl<S, T, A> OutgoingService<A> for BtpService<S, T, A>
where
    T: OutgoingService<A> + Clone + Send + 'static,
    A: Account + 'static,
{
    type Future = BoxedIlpFuture;

    fn send_request(&mut self, request: OutgoingRequest<A>) -> Self::Future {
        self.outgoing.send_request(request)
    }
}

fn parse_ilp_packet(message: Message) -> Result<(u32, Packet), ()> {
    if let Message::Binary(data) = message {
        let (request_id, ilp_data) = match BtpPacket::from_bytes(&data) {
            Ok(BtpPacket::Message(message)) => {
                let ilp_data = message
                    .protocol_data
                    .into_iter()
                    .find(|proto| proto.protocol_name == "ilp")
                    .ok_or(())?
                    .data;
                (message.request_id, ilp_data)
            }
            Ok(BtpPacket::Response(response)) => {
                let ilp_data = response
                    .protocol_data
                    .into_iter()
                    .find(|proto| proto.protocol_name == "ilp")
                    .ok_or(())?
                    .data;
                (response.request_id, ilp_data)
            }
            Ok(BtpPacket::Error(error)) => {
                error!("Got BTP error: {:?}", error);
                return Err(());
            }
            Err(err) => {
                error!("Error parsing BTP packet: {:?}", err);
                return Err(());
            }
        };
        if let Ok(packet) = Packet::try_from(BytesMut::from(ilp_data)) {
            Ok((request_id, packet))
        } else {
            Err(())
        }
    } else {
        error!("Got a non-binary WebSocket message");
        Err(())
    }
}

fn ilp_packet_to_ws_message(request_id: u32, packet: Packet) -> Message {
    match packet {
        Packet::Prepare(prepare) => {
            let data = BytesMut::from(prepare).to_vec();
            let btp_packet = BtpMessage {
                request_id,
                protocol_data: vec![ProtocolData {
                    protocol_name: "ilp".to_string(),
                    content_type: ContentType::ApplicationOctetStream,
                    data,
                }],
            };
            Message::binary(btp_packet.to_bytes())
        }
        Packet::Fulfill(fulfill) => {
            let data = BytesMut::from(fulfill).to_vec();
            let btp_packet = BtpResponse {
                request_id,
                protocol_data: vec![ProtocolData {
                    protocol_name: "ilp".to_string(),
                    content_type: ContentType::ApplicationOctetStream,
                    data,
                }],
            };
            Message::binary(btp_packet.to_bytes())
        }
        Packet::Reject(reject) => {
            let data = BytesMut::from(reject).to_vec();
            let btp_packet = BtpResponse {
                request_id,
                protocol_data: vec![ProtocolData {
                    protocol_name: "ilp".to_string(),
                    content_type: ContentType::ApplicationOctetStream,
                    data,
                }],
            };
            Message::binary(btp_packet.to_bytes())
        }
    }
}