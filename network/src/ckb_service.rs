#![allow(clippy::needless_pass_by_value)]

use crate::ckb_protocol::CKBProtocolOutput;
use crate::ckb_protocol_handler::DefaultCKBProtocolContext;
use crate::peer_store::{Behaviour, Status};
use crate::protocol::Protocol;
use crate::protocol_service::ProtocolService;
use crate::CKBProtocolHandler;
use crate::Network;
use crate::PeerId;
use faketime::unix_time_as_millis;
use futures::future::{self, Future};
use futures::Stream;
use libp2p::core::{Endpoint, Multiaddr, UniqueConnecState};
use log::{error, info};
use std::boxed::Box;
use std::io::{Error as IoError, ErrorKind as IoErrorKind};
use std::sync::Arc;
use tokio;

pub struct CKBService;

impl CKBService {
    fn handle_protocol_connection(
        network: Arc<Network>,
        peer_id: PeerId,
        protocol_output: CKBProtocolOutput<Arc<CKBProtocolHandler>>,
        addr: Multiaddr,
    ) -> Box<Future<Item = (), Error = IoError> + Send> {
        let protocol_id = protocol_output.protocol_id;
        let protocol_handler = protocol_output.protocol_handler;
        let protocol_version = protocol_output.protocol_version;
        let endpoint = protocol_output.endpoint;
        // get peer protocol_connection
        let protocol_connec = {
            let result = match endpoint {
                Endpoint::Dialer => {
                    network.try_outbound_ckb_protocol_connec(&peer_id, protocol_id, addr)
                }
                Endpoint::Listener => {
                    network.try_inbound_ckb_protocol_connec(&peer_id, protocol_id, addr)
                }
            };
            if let Err(err) = result {
                return Box::new(future::err(IoError::new(
                    IoErrorKind::Other,
                    format!("handle ckb_protocol connection error: {}", err),
                ))) as Box<Future<Item = (), Error = IoError> + Send>;
            }
            result.unwrap()
        };
        if protocol_connec.state() == UniqueConnecState::Full {
            error!(
                target: "network",
                "we already connected peer {:?} with {:?}, stop handling",
                peer_id, protocol_id
            );
            return Box::new(future::ok(())) as Box<_>;
        }

        let peer_index = match network.get_peer_index(&peer_id) {
            Some(peer_index) => peer_index,
            None => {
                return Box::new(future::err(IoError::new(
                    IoErrorKind::Other,
                    format!("can't find peer {:?}", peer_id),
                )));
            }
        };

        let protocol_future = {
            let handling_future = protocol_output.incoming_stream.for_each({
                let network = Arc::clone(&network);
                let protocol_handler = Arc::clone(&protocol_handler);
                let peer_id = peer_id.clone();
                move |data| {
                    network.modify_peer(&peer_id, |peer| {
                        peer.last_message_time = Some(unix_time_as_millis())
                    });
                    let protocol_handler = Arc::clone(&protocol_handler);
                    let network = Arc::clone(&network);
                    let handle_received = future::lazy(move || {
                        protocol_handler.received(
                            Box::new(DefaultCKBProtocolContext::new(network, protocol_id)),
                            peer_index,
                            &data,
                        );
                        Ok(())
                    });
                    tokio::spawn(handle_received);
                    Ok(())
                }
            });
            protocol_connec
                .tie_or_stop(
                    (protocol_output.outgoing_msg_channel, protocol_version),
                    handling_future,
                )
                .then({
                    let network = Arc::clone(&network);
                    let peer_id = peer_id.clone();
                    let protocol_handler = Arc::clone(&protocol_handler);
                    let protocol_id = protocol_id;
                    move |val| {
                        info!(
                            target: "network",
                            "Disconnect! peer {:?} protocol_id {:?} reason {:?}",
                            peer_id, protocol_id, val
                        );
                        {
                            let mut peer_store = network.peer_store().write();
                            peer_store.report(&peer_id, Behaviour::UnexpectedDisconnect);
                            peer_store.update_status(&peer_id, Status::Disconnected);
                        }
                        protocol_handler.disconnected(
                            Box::new(DefaultCKBProtocolContext::new(
                                Arc::clone(&network),
                                protocol_id,
                            )),
                            peer_index,
                        );
                        network.drop_peer(&peer_id);
                        val
                    }
                })
        };

        info!(
            target: "network",
            "Connected to peer {:?} with protocol_id {:?} version {}",
            peer_id, protocol_id, protocol_version
        );
        {
            let mut peer_store = network.peer_store().write();
            peer_store.report(&peer_id, Behaviour::Connect);
            peer_store.update_status(&peer_id, Status::Connected);
        }
        {
            let handle_connected = future::lazy(move || {
                protocol_handler.connected(
                    Box::new(DefaultCKBProtocolContext::new(
                        Arc::clone(&network),
                        protocol_id,
                    )),
                    peer_index,
                );
                Ok(())
            });
            tokio::spawn(handle_connected);
        }
        Box::new(protocol_future) as Box<_>
    }
}

impl<T: Send> ProtocolService<T> for CKBService {
    type Output = CKBProtocolOutput<Arc<CKBProtocolHandler>>;
    fn convert_to_protocol(
        peer_id: Arc<PeerId>,
        addr: &Multiaddr,
        output: Self::Output,
    ) -> Protocol<T> {
        Protocol::CKBProtocol(output, PeerId::clone(&peer_id), addr.to_owned())
    }
    fn handle(
        &self,
        network: Arc<Network>,
        protocol: Protocol<T>,
    ) -> Box<Future<Item = (), Error = IoError> + Send> {
        match protocol {
            Protocol::CKBProtocol(output, peer_id, addr) => {
                let handling_future =
                    Self::handle_protocol_connection(network, peer_id, output, addr);
                Box::new(handling_future) as Box<Future<Item = _, Error = _> + Send>
            }
            _ => Box::new(future::ok(())) as Box<Future<Item = _, Error = _> + Send>,
        }
    }
}
