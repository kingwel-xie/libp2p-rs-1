// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! The Kademlia connection protocol upgrade and associated message types.
//!
//! The connection protocol upgrade is provided by [`KadProtocolHandler`], with the
//! request and response types [`KadRequestMsg`] and [`KadResponseMsg`], respectively.
//! The upgrade's output is a `Sink + Stream` of messages. The `Stream` component is used
//! to poll the underlying transport for incoming messages, and the `Sink` component
//! is used to send messages to remote peers.

use std::{convert::TryFrom, time::Duration, time::Instant};
use std::io;
use std::error::Error;
use unsigned_varint::codec;
use bytes::BytesMut;
use codec::UviBytes;
use prost::Message;
use futures::prelude::*;
use futures::channel::{mpsc, oneshot};
use futures_codec::Framed;
use futures::SinkExt;

use async_trait::async_trait;
use async_std::task;

use libp2prs_traits::{ReadEx, WriteEx};
use libp2prs_core::{Multiaddr, PeerId};
use libp2prs_core::upgrade::UpgradeInfo;
use libp2prs_swarm::protocol_handler::{ProtocolHandler, Notifiee, IProtocolHandler};
use libp2prs_swarm::connection::Connection;
use libp2prs_swarm::substream::Substream;
use libp2prs_swarm::Control as SwarmControl;

use crate::{dht_proto as proto, KadError};
use crate::record::{self, Record};

/// The protocol name used for negotiating with multistream-select.
pub const DEFAULT_PROTO_NAME: &[u8] = b"/ipfs/kad/1.0.0";

/// The default maximum size for a varint length-delimited packet.
pub const DEFAULT_MAX_PACKET_SIZE: usize = 16 * 1024;

/// Status of our connection to a node reported by the Kademlia protocol.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum KadConnectionType {
    /// Sender hasn't tried to connect to peer.
    NotConnected = 0,
    /// Sender is currently connected to peer.
    Connected = 1,
    /// Sender was recently connected to peer.
    CanConnect = 2,
    /// Sender tried to connect to peer but failed.
    CannotConnect = 3,
}

impl From<proto::message::ConnectionType> for KadConnectionType {
    fn from(raw: proto::message::ConnectionType) -> KadConnectionType {
        use proto::message::ConnectionType::*;
        match raw {
            NotConnected => KadConnectionType::NotConnected,
            Connected => KadConnectionType::Connected,
            CanConnect => KadConnectionType::CanConnect,
            CannotConnect => KadConnectionType::CannotConnect,
        }
    }
}

impl Into<proto::message::ConnectionType> for KadConnectionType {
    fn into(self) -> proto::message::ConnectionType {
        use proto::message::ConnectionType::*;
        match self {
            KadConnectionType::NotConnected => NotConnected,
            KadConnectionType::Connected => Connected,
            KadConnectionType::CanConnect => CanConnect,
            KadConnectionType::CannotConnect => CannotConnect,
        }
    }
}

/// Information about a peer, as known by the sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KadPeer {
    /// Identifier of the peer.
    pub node_id: PeerId,
    /// The multiaddresses that the sender think can be used in order to reach the peer.
    pub multiaddrs: Vec<Multiaddr>,
    /// How the sender is connected to that remote.
    pub connection_ty: KadConnectionType,
}

// Builds a `KadPeer` from a corresponding protobuf message.
impl TryFrom<proto::message::Peer> for KadPeer {
    type Error = io::Error;

    fn try_from(peer: proto::message::Peer) -> Result<KadPeer, Self::Error> {
        // TODO: this is in fact a CID; not sure if this should be handled in `from_bytes` or
        //       as a special case here
        let node_id = PeerId::from_bytes(peer.id)
            .map_err(|_| invalid_data("invalid peer id"))?;

        let mut addrs = Vec::with_capacity(peer.addrs.len());
        for addr in peer.addrs.into_iter() {
            let as_ma = Multiaddr::try_from(addr).map_err(invalid_data)?;
            addrs.push(as_ma);
        }
        debug_assert_eq!(addrs.len(), addrs.capacity());

        let connection_ty = proto::message::ConnectionType::from_i32(peer.connection)
            .ok_or_else(|| invalid_data("unknown connection type"))?
            .into();

        Ok(KadPeer {
            node_id,
            multiaddrs: addrs,
            connection_ty
        })
    }
}

impl Into<proto::message::Peer> for KadPeer {
    fn into(self) -> proto::message::Peer {
        proto::message::Peer {
            id: self.node_id.into_bytes(),
            addrs: self.multiaddrs.into_iter().map(|a| a.to_vec()).collect(),
            connection: {
                let ct: proto::message::ConnectionType = self.connection_ty.into();
                ct as i32
            }
        }
    }
}


type ProtocolId = &'static [u8];



/// Configuration for a Kademlia protocol handler.
#[derive(Debug, Clone)]
pub struct KademliaProtocolConfig {
    protocol_name: ProtocolId,
    /// Maximum allowed size of a packet.
    max_packet_size: usize,
}

impl KademliaProtocolConfig {
    /// Returns the configured protocol name.
    pub fn protocol_name(&self) -> ProtocolId {
        self.protocol_name.clone()
    }

    /// Modifies the protocol name used on the wire. Can be used to create incompatibilities
    /// between networks on purpose.
    pub fn set_protocol_name(&mut self, name: ProtocolId) {
        self.protocol_name = name;
    }

    /// Modifies the maximum allowed size of a single Kademlia packet.
    pub fn set_max_packet_size(&mut self, size: usize) {
        self.max_packet_size = size;
    }
}

impl Default for KademliaProtocolConfig {
    fn default() -> Self {
        KademliaProtocolConfig {
            protocol_name: DEFAULT_PROTO_NAME,
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
        }
    }
}


/// The Protocol Handler of Kademlia DHT.
#[derive(Debug, Clone)]
pub struct KadProtocolHandler {
    /// The configuration of the protocol handler.
    config: KademliaProtocolConfig,
    /// If false, we deny incoming requests.
    allow_listening: bool,
    /// Time after which we close an idle connection.
    idle_timeout: Duration,

    message_tx: mpsc::UnboundedSender<ProtocolEvent<u32>>,
}

impl KadProtocolHandler {
    /// Make a new KadProtocolHandler.
    pub fn new (config: KademliaProtocolConfig, message_tx: mpsc::UnboundedSender<ProtocolEvent<u32>>) -> Self {
        KadProtocolHandler {
            config,
            allow_listening: false,
            idle_timeout: Duration::from_secs(10),
            message_tx,
        }
    }

    /// Returns the configured protocol name.
    pub fn protocol_name(&self) -> &[u8] {
        &self.config.protocol_name
    }

    /// Modifies the maximum allowed size of a single Kademlia packet.
    pub fn set_max_packet_size(&mut self, size: usize) {
        self.config.max_packet_size = size;
    }
}


impl UpgradeInfo for KadProtocolHandler {
    type Info = ProtocolId;

    fn protocol_info(&self) -> Vec<Self::Info> {
        vec![self.config.protocol_name]
    }
}

impl Notifiee for KadProtocolHandler {
    fn connected(&mut self, conn: &mut Connection) {
        let peer_id = conn.remote_peer();
        let mut tx = self.message_tx.clone();
        task::spawn(async move {
            let _ = tx.send(ProtocolEvent::PeerConnected(peer_id)).await;
        });
    }
    fn disconnected(&mut self, conn: &mut Connection) {
        let peer_id = conn.remote_peer();
        let mut tx = self.message_tx.clone();
        task::spawn(async move {
            let _ = tx.send(ProtocolEvent::PeerDisconnected(peer_id)).await;
        });
    }
}

#[async_trait]
impl ProtocolHandler for KadProtocolHandler {
    async fn handle(&mut self, mut stream: Substream, _info: <Self as UpgradeInfo>::Info) -> Result<(), Box<dyn Error>> {
        let source = stream.remote_peer();
        log::trace!("Kad Handler receive packet from {}", source);
        loop {
            let packet = stream.read_one(self.config.max_packet_size).await?;
            let request = proto::Message::decode(&packet[..]).map_err(|_| KadError::Decode)?;
            log::trace!("Kad handler recv : {:?}", request);

            let request = proto_to_req_msg(request)?;

            // For AddProvider request, KadResponse is not needed
            let (tx, rx) = oneshot::channel();
            let evt = ProtocolEvent::KadRequest { request, source: source.clone(), reply: tx };
            self.message_tx.send(evt).await?;
            let response = rx.await??;

            if let Some(response) = response {
                // handle response messages
                let proto_struct = resp_msg_to_proto(response);
                let mut buf = Vec::with_capacity(proto_struct.encoded_len());
                proto_struct.encode(&mut buf).expect("Vec<u8> provides capacity as needed");

                let _= stream.write2(&buf).await?;
            }
        }
    }

    fn box_clone(&self) -> IProtocolHandler {
        Box::new(self.clone())
    }
}

/// Kademlia message sender.
///
/// The message sender actually sends a Kad request message and waits for the correct response
/// message.
pub(crate) struct KadMessageSender {
    stream: Substream,
    config: KademliaProtocolConfig,
}

impl KadMessageSender {
    pub(crate) async fn build(mut swarm: SwarmControl, peer: PeerId, config: KademliaProtocolConfig) -> Result<Self, KadError> {
        let stream = swarm.new_stream(peer, vec!(config.protocol_name())).await?;
        Ok(Self {
            stream,
            config
        })
    }

    pub(crate) async fn close(&mut self) -> Result<(), KadError> {
        self.stream.close2().await.map_err(io::Error::into)
    }

    async fn send_request(&mut self, request: KadRequestMsg) -> Result<KadResponseMsg, KadError>
    {
        let proto_struct = req_msg_to_proto(request);
        let mut buf = Vec::with_capacity(proto_struct.encoded_len());
        proto_struct.encode(&mut buf).expect("Vec<u8> provides capacity as needed");
        self.stream.write2(&buf).await?;

        let packet = self.stream.read_one(4096).await?;
        let response = proto::Message::decode(&packet[..]).map_err(|_| KadError::Decode)?;
        log::trace!("Kad handler recv : {:?}", response);

        let response = proto_to_resp_msg(response)?;

        Ok(response)
    }

    pub(crate) async fn send_find_node(&mut self, key: record::Key) -> Result<Vec<KadPeer>, KadError>
    {
        let req = KadRequestMsg::FindNode { key };
        let rsp = self.send_request(req).await?;
        match rsp {
            KadResponseMsg::FindNode { closer_peers } => Ok(closer_peers),
            _ => Err(KadError::UnexpectedMessage("wrong message type received when FindNode"))
        }
    }

    pub(crate) async fn send_get_providers(&mut self, key: record::Key) -> Result<(Vec<KadPeer>, Vec<KadPeer>), KadError>
    {
        let req = KadRequestMsg::GetProviders { key };
        let rsp = self.send_request(req).await?;
        match rsp {
            KadResponseMsg::GetProviders { closer_peers, provider_peers } => Ok((closer_peers, provider_peers)),
            _ => Err(KadError::UnexpectedMessage("wrong message type received when GetProviders"))
        }
    }

    pub(crate) async fn send_get_value(&mut self, key: record::Key) -> Result<(Vec<KadPeer>, Option<Record>), KadError>
    {
        let req = KadRequestMsg::GetValue { key };
        let rsp = self.send_request(req).await?;
        match rsp {
            KadResponseMsg::GetValue { record, closer_peers } => Ok((closer_peers, record)),
            _ => Err(KadError::UnexpectedMessage("wrong message type received when GetValue"))
        }
    }

}


/*
impl<C> ProtocolHandler<C> for KadProtocolHandler
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    type Output = KadInStreamSink<C>;
    type Future = future::Ready<Result<Self::Output, io::Error>>;
    type Error = io::Error;

    fn upgrade_inbound(self, incoming: C, _: Self::Info) -> Self::Future {
        let mut codec = UviBytes::default();
        codec.set_max_len(self.max_packet_size);

        future::ok(
            Framed::new(incoming, codec)
                .err_into()
                .with::<_, _, fn(_) -> _, _>(|response| {
                    let proto_struct = resp_msg_to_proto(response);
                    let mut buf = Vec::with_capacity(proto_struct.encoded_len());
                    proto_struct.encode(&mut buf).expect("Vec<u8> provides capacity as needed");
                    future::ready(Ok(io::Cursor::new(buf)))
                })
                .and_then::<_, fn(_) -> _>(|bytes| {
                    let request = match proto::Message::decode(bytes) {
                        Ok(r) => r,
                        Err(err) => return future::ready(Err(err.into()))
                    };
                    future::ready(proto_to_req_msg(request))
                }),
        )
    }
}

impl<C> OutboundUpgrade<C> for KadProtocolHandler
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    type Output = KadOutStreamSink<C>;
    type Future = future::Ready<Result<Self::Output, io::Error>>;
    type Error = io::Error;

    fn upgrade_outbound(self, incoming: C, _: Self::Info) -> Self::Future {
        let mut codec = UviBytes::default();
        codec.set_max_len(self.max_packet_size);

        future::ok(
            Framed::new(incoming, codec)
                .err_into()
                .with::<_, _, fn(_) -> _, _>(|request| {
                    let proto_struct = req_msg_to_proto(request);
                    let mut buf = Vec::with_capacity(proto_struct.encoded_len());
                    proto_struct.encode(&mut buf).expect("Vec<u8> provides capacity as needed");
                    future::ready(Ok(io::Cursor::new(buf)))
                })
                .and_then::<_, fn(_) -> _>(|bytes| {
                    let response = match proto::Message::decode(bytes) {
                        Ok(r) => r,
                        Err(err) => return future::ready(Err(err.into()))
                    };
                    future::ready(proto_to_resp_msg(response))
                }),
        )
    }
}
*/
/// Sink of responses and stream of requests.
pub type KadInStreamSink<S> = KadStreamSink<S, KadResponseMsg, KadRequestMsg>;

/// Sink of requests and stream of responses.
pub type KadOutStreamSink<S> = KadStreamSink<S, KadRequestMsg, KadResponseMsg>;

pub type KadStreamSink<S, A, B> = stream::AndThen<
    sink::With<
        stream::ErrInto<Framed<S, UviBytes<io::Cursor<Vec<u8>>>>, io::Error>,
        io::Cursor<Vec<u8>>,
        A,
        future::Ready<Result<io::Cursor<Vec<u8>>, io::Error>>,
        fn(A) -> future::Ready<Result<io::Cursor<Vec<u8>>, io::Error>>,
    >,
    future::Ready<Result<B, io::Error>>,
    fn(BytesMut) -> future::Ready<Result<B, io::Error>>,
>;

/// Request that we can send to a peer or that we received from a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KadRequestMsg {
    /// Ping request.
    Ping,

    /// Request for the list of nodes whose IDs are the closest to `key`. The number of nodes
    /// returned is not specified, but should be around 20.
    FindNode {
        /// The key for which to locate the closest nodes.
        key: record::Key,
    },

    /// Same as `FindNode`, but should also return the entries of the local providers list for
    /// this key.
    GetProviders {
        /// Identifier being searched.
        key: record::Key,
    },

    /// Indicates that this list of providers is known for this key.
    AddProvider {
        /// Key for which we should add providers.
        key: record::Key,
        /// Known provider for this key.
        provider: KadPeer,
    },

    /// Request to get a value from the dht records.
    GetValue {
        /// The key we are searching for.
        key: record::Key,
    },

    /// Request to put a value into the dht records.
    PutValue {
        record: Record,
    }
}

/// Response that we can send to a peer or that we received from a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KadResponseMsg {
    /// Ping response.
    Pong,

    /// Response to a `FindNode`.
    FindNode {
        /// Results of the request.
        closer_peers: Vec<KadPeer>,
    },

    /// Response to a `GetProviders`.
    GetProviders {
        /// Nodes closest to the key.
        closer_peers: Vec<KadPeer>,
        /// Known providers for this key.
        provider_peers: Vec<KadPeer>,
    },

    /// Response to a `GetValue`.
    GetValue {
        /// Result that might have been found
        record: Option<Record>,
        /// Nodes closest to the key
        closer_peers: Vec<KadPeer>,
    },

    /// Response to a `PutValue`.
    PutValue {
        /// The key of the record.
        key: record::Key,
        /// Value of the record.
        value: Vec<u8>,
    },
}

/// Converts a `KadRequestMsg` into the corresponding protobuf message for sending.
fn req_msg_to_proto(kad_msg: KadRequestMsg) -> proto::Message {
    match kad_msg {
        KadRequestMsg::Ping => proto::Message {
            r#type: proto::message::MessageType::Ping as i32,
            .. proto::Message::default()
        },
        KadRequestMsg::FindNode { key } => proto::Message {
            r#type: proto::message::MessageType::FindNode as i32,
            key: key.to_vec(),
            cluster_level_raw: 10,
            .. proto::Message::default()
        },
        KadRequestMsg::GetProviders { key } => proto::Message {
            r#type: proto::message::MessageType::GetProviders as i32,
            key: key.to_vec(),
            cluster_level_raw: 10,
            .. proto::Message::default()
        },
        KadRequestMsg::AddProvider { key, provider } => proto::Message {
            r#type: proto::message::MessageType::AddProvider as i32,
            cluster_level_raw: 10,
            key: key.to_vec(),
            provider_peers: vec![provider.into()],
            .. proto::Message::default()
        },
        KadRequestMsg::GetValue { key } => proto::Message {
            r#type: proto::message::MessageType::GetValue as i32,
            cluster_level_raw: 10,
            key: key.to_vec(),
            .. proto::Message::default()
        },
        KadRequestMsg::PutValue { record } => proto::Message {
            r#type: proto::message::MessageType::PutValue as i32,
            record: Some(record_to_proto(record)),
            .. proto::Message::default()
        }
    }
}

/// Converts a `KadResponseMsg` into the corresponding protobuf message for sending.
fn resp_msg_to_proto(kad_msg: KadResponseMsg) -> proto::Message {
    match kad_msg {
        KadResponseMsg::Pong => proto::Message {
            r#type: proto::message::MessageType::Ping as i32,
            .. proto::Message::default()
        },
        KadResponseMsg::FindNode { closer_peers } => proto::Message {
            r#type: proto::message::MessageType::FindNode as i32,
            cluster_level_raw: 9,
            closer_peers: closer_peers.into_iter().map(KadPeer::into).collect(),
            .. proto::Message::default()
        },
        KadResponseMsg::GetProviders { closer_peers, provider_peers } => proto::Message {
            r#type: proto::message::MessageType::GetProviders as i32,
            cluster_level_raw: 9,
            closer_peers: closer_peers.into_iter().map(KadPeer::into).collect(),
            provider_peers: provider_peers.into_iter().map(KadPeer::into).collect(),
            .. proto::Message::default()
        },
        KadResponseMsg::GetValue { record, closer_peers } => proto::Message {
            r#type: proto::message::MessageType::GetValue as i32,
            cluster_level_raw: 9,
            closer_peers: closer_peers.into_iter().map(KadPeer::into).collect(),
            record: record.map(record_to_proto),
            .. proto::Message::default()
        },
        KadResponseMsg::PutValue { key, value } => proto::Message {
            r#type: proto::message::MessageType::PutValue as i32,
            key: key.to_vec(),
            record: Some(proto::Record {
                key: key.to_vec(),
                value,
                .. proto::Record::default()
            }),
            .. proto::Message::default()
        }
    }
}

/// Converts a received protobuf message into a corresponding `KadRequestMsg`.
///
/// Fails if the protobuf message is not a valid and supported Kademlia request message.
fn proto_to_req_msg(message: proto::Message) -> Result<KadRequestMsg, io::Error> {
    let msg_type = proto::message::MessageType::from_i32(message.r#type)
        .ok_or_else(|| invalid_data(format!("unknown message type: {}", message.r#type)))?;

    match msg_type {
        proto::message::MessageType::Ping => Ok(KadRequestMsg::Ping),
        proto::message::MessageType::PutValue => {
            let record = record_from_proto(message.record.unwrap_or_default())?;
            Ok(KadRequestMsg::PutValue { record })
        }
        proto::message::MessageType::GetValue => {
            Ok(KadRequestMsg::GetValue { key: record::Key::from(message.key) })
        }
        proto::message::MessageType::FindNode => {
            Ok(KadRequestMsg::FindNode { key: record::Key::from(message.key) })
        }
        proto::message::MessageType::GetProviders => {
            Ok(KadRequestMsg::GetProviders { key: record::Key::from(message.key)})
        }
        proto::message::MessageType::AddProvider => {
            // TODO: for now we don't parse the peer properly, so it is possible that we get
            //       parsing errors for peers even when they are valid; we ignore these
            //       errors for now, but ultimately we should just error altogether
            let provider = message.provider_peers
                .into_iter()
                .find_map(|peer| KadPeer::try_from(peer).ok());

            if let Some(provider) = provider {
                let key = record::Key::from(message.key);
                Ok(KadRequestMsg::AddProvider { key, provider })
            } else {
                Err(invalid_data("AddProvider message with no valid peer."))
            }
        }
    }
}

/// Converts a received protobuf message into a corresponding `KadResponseMessage`.
///
/// Fails if the protobuf message is not a valid and supported Kademlia response message.
fn proto_to_resp_msg(message: proto::Message) -> Result<KadResponseMsg, io::Error> {
    let msg_type = proto::message::MessageType::from_i32(message.r#type)
        .ok_or_else(|| invalid_data(format!("unknown message type: {}", message.r#type)))?;

    match msg_type {
        proto::message::MessageType::Ping => Ok(KadResponseMsg::Pong),
        proto::message::MessageType::GetValue => {
            let record =
                if let Some(r) = message.record {
                    Some(record_from_proto(r)?)
                } else {
                    None
                };

            let closer_peers = message.closer_peers.into_iter()
                .filter_map(|peer| KadPeer::try_from(peer).ok())
                .collect();

            Ok(KadResponseMsg::GetValue { record, closer_peers })
        }

        proto::message::MessageType::FindNode => {
            let closer_peers = message.closer_peers.into_iter()
                .filter_map(|peer| KadPeer::try_from(peer).ok())
                .collect();

            Ok(KadResponseMsg::FindNode { closer_peers })
        }

        proto::message::MessageType::GetProviders => {
            let closer_peers = message.closer_peers.into_iter()
                .filter_map(|peer| KadPeer::try_from(peer).ok())
                .collect();

            let provider_peers = message.provider_peers.into_iter()
                .filter_map(|peer| KadPeer::try_from(peer).ok())
                .collect();

            Ok(KadResponseMsg::GetProviders {
                closer_peers,
                provider_peers,
            })
        }

        proto::message::MessageType::PutValue => {
            let key = record::Key::from(message.key);
            let rec = message.record.ok_or_else(|| {
                invalid_data("received PutValue message with no record")
            })?;

            Ok(KadResponseMsg::PutValue {
                key,
                value: rec.value
            })
        }

        proto::message::MessageType::AddProvider =>
            Err(invalid_data("received an unexpected AddProvider message"))
    }
}

fn record_from_proto(record: proto::Record) -> Result<Record, io::Error> {
    let key = record::Key::from(record.key);
    let value = record.value;

    let publisher =
        if !record.publisher.is_empty() {
            PeerId::from_bytes(record.publisher)
                .map(Some)
                .map_err(|_| invalid_data("Invalid publisher peer ID."))?
        } else {
            None
        };

    let expires =
        if record.ttl > 0 {
            Some(Instant::now() + Duration::from_secs(record.ttl as u64))
        } else {
            None
        };

    Ok(Record { key, value, publisher, expires })
}

fn record_to_proto(record: Record) -> proto::Record {
    proto::Record {
        key: record.key.to_vec(),
        value: record.value,
        publisher: record.publisher.map(PeerId::into_bytes).unwrap_or_default(),
        ttl: record.expires
            .map(|t| {
                let now = Instant::now();
                if t > now {
                    (t - now).as_secs() as u32
                } else {
                    1 // because 0 means "does not expire"
                }
            })
            .unwrap_or(0),
        time_received: String::new()
    }
}

/// Creates an `io::Error` with `io::ErrorKind::InvalidData`.
fn invalid_data<E>(e: E) -> io::Error
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>
{
    io::Error::new(io::ErrorKind::InvalidData, e)
}

#[cfg(test)]
mod tests {

    /*// TODO: restore
    use self::libp2p_tcp::TcpConfig;
    use self::tokio::runtime::current_thread::Runtime;
    use futures::{Future, Sink, Stream};
    use libp2p_core::{PeerId, PublicKey, Transport};
    use multihash::{encode, Hash};
    use protocol::{KadConnectionType, KadPeer, KadProtocolHandler};
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn correct_transfer() {
        // We open a server and a client, send a message between the two, and check that they were
        // successfully received.

        test_one(KadMsg::Ping);
        test_one(KadMsg::FindNodeReq {
            key: PeerId::random(),
        });
        test_one(KadMsg::FindNodeRes {
            closer_peers: vec![KadPeer {
                node_id: PeerId::random(),
                multiaddrs: vec!["/ip4/100.101.102.103/tcp/20105".parse().unwrap()],
                connection_ty: KadConnectionType::Connected,
            }],
        });
        test_one(KadMsg::GetProvidersReq {
            key: encode(Hash::SHA2256, &[9, 12, 0, 245, 245, 201, 28, 95]).unwrap(),
        });
        test_one(KadMsg::GetProvidersRes {
            closer_peers: vec![KadPeer {
                node_id: PeerId::random(),
                multiaddrs: vec!["/ip4/100.101.102.103/tcp/20105".parse().unwrap()],
                connection_ty: KadConnectionType::Connected,
            }],
            provider_peers: vec![KadPeer {
                node_id: PeerId::random(),
                multiaddrs: vec!["/ip4/200.201.202.203/tcp/1999".parse().unwrap()],
                connection_ty: KadConnectionType::NotConnected,
            }],
        });
        test_one(KadMsg::AddProvider {
            key: encode(Hash::SHA2256, &[9, 12, 0, 245, 245, 201, 28, 95]).unwrap(),
            provider_peer: KadPeer {
                node_id: PeerId::random(),
                multiaddrs: vec!["/ip4/9.1.2.3/udp/23".parse().unwrap()],
                connection_ty: KadConnectionType::Connected,
            },
        });
        // TODO: all messages

        fn test_one(msg_server: KadMsg) {
            let msg_client = msg_server.clone();
            let (tx, rx) = mpsc::channel();

            let bg_thread = thread::spawn(move || {
                let transport = TcpConfig::new().with_upgrade(KadProtocolHandler);

                let (listener, addr) = transport
                    .listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap())
                    .unwrap();
                tx.send(addr).unwrap();

                let future = listener
                    .into_future()
                    .map_err(|(err, _)| err)
                    .and_then(|(client, _)| client.unwrap().0)
                    .and_then(|proto| proto.into_future().map_err(|(err, _)| err).map(|(v, _)| v))
                    .map(|recv_msg| {
                        assert_eq!(recv_msg.unwrap(), msg_server);
                        ()
                    });
                let mut rt = Runtime::new().unwrap();
                let _ = rt.block_on(future).unwrap();
            });

            let transport = TcpConfig::new().with_upgrade(KadProtocolHandler);

            let future = transport
                .dial(rx.recv().unwrap())
                .unwrap()
                .and_then(|proto| proto.send(msg_client))
                .map(|_| ());
            let mut rt = Runtime::new().unwrap();
            let _ = rt.block_on(future).unwrap();
            bg_thread.join().unwrap();
        }
    }*/
}


/// Event produced by the Kademlia handler.
#[derive(Debug)]
pub enum ProtocolEvent<TUserData> {
    /// A new connection from peer_id is opened.
    ///
    /// This notification comes from Protocol Notifiee trait.
    PeerConnected(PeerId),
    /// A connection from peer_id is closed.
    ///
    /// This notification comes from Protocol Notifiee trait.
    PeerDisconnected(PeerId),
    /// A new peer found when trying to lookup a 'Key' or receiving a
    /// query from peer.
    ///
    /// This notification comes from either a query or a forced activity.
    /// The 'bool' parameter 'true' means if it comes from a query.
    KadPeerFound(PeerId, bool),
    /// A Kad peer stopped when it is being connected/queried.
    ///
    /// This notification comes from either a query activity.
    KadPeerStopped(PeerId),
    /// The configured protocol name has been confirmed by the peer through
    /// a successfully negotiated substream.
    ///
    /// This event is only emitted once by a handler upon the first
    /// successfully negotiated inbound or outbound substream and
    /// indicates that the connected peer participates in the Kademlia
    /// overlay network identified by the configured protocol name.
    // TODO: ConnectedPoint
    ProtocolConfirmed { endpoint: u32/*ConnectedPoint*/ },

    /// Kad request message from remote peer.
    ///
    KadRequest {
        /// Request message, decoded from ProtoBuf
        request: KadRequestMsg,
        /// Source of the message, which is the Peer Id of the remote.
        source: PeerId,
        /// Reply oneshot channel.
        ///
        /// Note that AddProvider doesn't require response. This is why
        /// KadResponseMsg is wrapped by Option<T>
        reply: oneshot::Sender<Result<Option<KadResponseMsg>, KadError>>
    },

    /// Response to an `KademliaHandlerIn::FindNodeReq`.
    FindNodeRes {
        /// Results of the request.
        closer_peers: Vec<KadPeer>,
        /// The user data passed to the `FindNodeReq`.
        user_data: TUserData,
    },

    /// Response to an `KademliaHandlerIn::GetProvidersReq`.
    GetProvidersRes {
        /// Nodes closest to the key.
        closer_peers: Vec<KadPeer>,
        /// Known providers for this key.
        provider_peers: Vec<KadPeer>,
        /// The user data passed to the `GetProvidersReq`.
        user_data: TUserData,
    },

    /// An error happened when performing a query.
    QueryError {
        /// The error that happened.
        error: KadError,
        /// The user data passed to the query.
        user_data: TUserData,
    },

    /// Response to a `KademliaHandlerIn::GetRecord`.
    GetRecordRes {
        /// The result is present if the key has been found
        record: Option<Record>,
        /// Nodes closest to the key.
        closer_peers: Vec<KadPeer>,
        /// The user data passed to the `GetValue`.
        user_data: TUserData,
    },

    /// Response to a request to store a record.
    PutRecordRes {
        /// The key of the stored record.
        key: record::Key,
        /// The value of the stored record.
        value: Vec<u8>,
        /// The user data passed to the `PutValue`.
        user_data: TUserData,
    }
}


/// Process a Kademlia message that's supposed to be a response to one of our requests.
fn process_kad_response<TUserData>(
    event: KadResponseMsg,
    user_data: TUserData,
) -> ProtocolEvent<TUserData> {
    // TODO: must check that the response corresponds to the request
    match event {
        KadResponseMsg::Pong => {
            // We never send out pings.
            ProtocolEvent::QueryError {
                error: KadError::UnexpectedMessage("We never send out pings"),
                user_data,
            }
        }
        KadResponseMsg::FindNode { closer_peers } => {
            ProtocolEvent::FindNodeRes {
                closer_peers,
                user_data,
            }
        },
        KadResponseMsg::GetProviders {
            closer_peers,
            provider_peers,
        } => ProtocolEvent::GetProvidersRes {
            closer_peers,
            provider_peers,
            user_data,
        },
        KadResponseMsg::GetValue {
            record,
            closer_peers,
        } => ProtocolEvent::GetRecordRes {
            record,
            closer_peers,
            user_data,
        },
        KadResponseMsg::PutValue { key, value, .. } => {
            ProtocolEvent::PutRecordRes {
                key,
                value,
                user_data,
            }
        }
    }
}
