use std::fmt::{Display, Error as FmtError, Formatter};

use aleph_primitives::AuthorityId;
use futures::{
    channel::{mpsc, oneshot},
    StreamExt,
};
use log::{debug, info, trace};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    crypto::AuthorityPen,
    validator_network::{
        handshake::{v0_handshake_incoming, v0_handshake_outgoing, HandshakeError},
        heartbeat::{heartbeat_receiver, heartbeat_sender},
        io::{receive_data, send_data, ReceiveError, SendError},
        Data, Splittable,
    },
};

/// Defines the protocol for communication.
#[derive(Debug, PartialEq, Eq)]
pub enum Protocol {
    /// The current version of the protocol.
    V0,
}

/// Protocol error.
#[derive(Debug)]
pub enum ProtocolError {
    /// Error during performing a handshake.
    HandshakeError(HandshakeError),
    /// Sending failed.
    SendError(SendError),
    /// Receiving failed.
    ReceiveError(ReceiveError),
    /// Heartbeat stopped.
    CardiacArrest,
    /// Channel to the parent service closed.
    NoParentConnection,
    /// Data channel closed.
    NoUserConnection,
}

impl Display for ProtocolError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        use ProtocolError::*;
        match self {
            HandshakeError(e) => write!(f, "handshake error: {}", e),
            SendError(e) => write!(f, "send error: {}", e),
            ReceiveError(e) => write!(f, "receive error: {}", e),
            CardiacArrest => write!(f, "heartbeat stopped"),
            NoParentConnection => write!(f, "cannot send result to service"),
            NoUserConnection => write!(f, "cannot send data to user"),
        }
    }
}

impl From<HandshakeError> for ProtocolError {
    fn from(e: HandshakeError) -> Self {
        ProtocolError::HandshakeError(e)
    }
}

impl From<SendError> for ProtocolError {
    fn from(e: SendError) -> Self {
        ProtocolError::SendError(e)
    }
}

impl From<ReceiveError> for ProtocolError {
    fn from(e: ReceiveError) -> Self {
        ProtocolError::ReceiveError(e)
    }
}

/// Receives data from the parent service and sends it over the network.
/// Exits when the parent channel is closed, or if the network connection is broken.
async fn sending<D: Data, S: AsyncWrite + Unpin + Send>(
    mut sender: S,
    mut data_from_user: mpsc::UnboundedReceiver<D>,
) -> Result<(), ProtocolError> {
    loop {
        sender = match data_from_user.next().await {
            Some(data) => send_data(sender, data).await?,
            // We have been closed by the parent service, all good.
            None => return Ok(()),
        };
    }
}

/// Performs the handshake, and then keeps sending data received from the parent service.
/// Exits on parent request, or in case of broken or dead network connection.
async fn v0_outgoing<D: Data, S: Splittable>(
    stream: S,
    authority_pen: AuthorityPen,
    peer_id: AuthorityId,
    result_for_parent: mpsc::UnboundedSender<(AuthorityId, Option<mpsc::UnboundedSender<D>>)>,
) -> Result<(), ProtocolError> {
    trace!(target: "validator-network", "Extending hand to {}.", peer_id);
    let (sender, receiver) = v0_handshake_outgoing(stream, authority_pen, peer_id.clone()).await?;
    info!(target: "validator-network", "Outgoing handshake with {} finished successfully.", peer_id);
    let (data_for_network, data_from_user) = mpsc::unbounded::<D>();
    result_for_parent
        .unbounded_send((peer_id.clone(), Some(data_for_network)))
        .map_err(|_| ProtocolError::NoParentConnection)?;

    let sending = sending(sender, data_from_user);
    let heartbeat = heartbeat_receiver(receiver);

    debug!(target: "validator-network", "Starting worker for sending to {}.", peer_id);
    loop {
        tokio::select! {
            _ = heartbeat => return Err(ProtocolError::CardiacArrest),
            result = sending => return result,
        }
    }
}

/// Receives data from the network and sends it to the parent service.
/// Exits when the parent channel is closed, or if the network connection is broken.
async fn receiving<D: Data, S: AsyncRead + Unpin + Send>(
    mut stream: S,
    data_for_user: mpsc::UnboundedSender<D>,
) -> Result<(), ProtocolError> {
    loop {
        let (old_stream, data) = receive_data(stream).await?;
        stream = old_stream;
        data_for_user
            .unbounded_send(data)
            .map_err(|_| ProtocolError::NoUserConnection)?;
    }
}

/// Performs the handshake, and then keeps sending data received from the network to the parent service.
/// Exits on parent request, or in case of broken or dead network connection.
async fn v0_incoming<D: Data, S: Splittable>(
    stream: S,
    authority_pen: AuthorityPen,
    result_for_parent: mpsc::UnboundedSender<(AuthorityId, oneshot::Sender<()>)>,
    data_for_user: mpsc::UnboundedSender<D>,
) -> Result<(), ProtocolError> {
    trace!(target: "validator-network", "Waiting for extended hand...");
    let (sender, receiver, peer_id) = v0_handshake_incoming(stream, authority_pen).await?;
    info!(target: "validator-network", "Incoming handshake with {} finished successfully.", peer_id);

    let (tx_exit, exit) = oneshot::channel();
    result_for_parent
        .unbounded_send((peer_id.clone(), tx_exit))
        .map_err(|_| ProtocolError::NoParentConnection)?;

    let receiving = receiving(receiver, data_for_user);
    let heartbeat = heartbeat_sender(sender);

    debug!(target: "validator-network", "Starting worker for receiving from {}.", peer_id);
    loop {
        tokio::select! {
            _ = heartbeat => return Err(ProtocolError::CardiacArrest),
            result = receiving => return result,
            _ = exit => return Ok(()),
        }
    }
}

impl Protocol {
    /// Launches the proper variant of the protocol (receiver half).
    pub async fn manage_incoming<D: Data, S: Splittable>(
        &self,
        stream: S,
        authority_pen: AuthorityPen,
        result_for_service: mpsc::UnboundedSender<(AuthorityId, oneshot::Sender<()>)>,
        data_for_user: mpsc::UnboundedSender<D>,
    ) -> Result<(), ProtocolError> {
        use Protocol::*;
        match self {
            V0 => v0_incoming(stream, authority_pen, result_for_service, data_for_user).await,
        }
    }

    /// Launches the proper variant of the protocol (sender half).
    pub async fn manage_outgoing<D: Data, S: Splittable>(
        &self,
        stream: S,
        authority_pen: AuthorityPen,
        peer_id: AuthorityId,
        result_for_service: mpsc::UnboundedSender<(AuthorityId, Option<mpsc::UnboundedSender<D>>)>,
    ) -> Result<(), ProtocolError> {
        use Protocol::*;
        match self {
            V0 => v0_outgoing(stream, authority_pen, peer_id, result_for_service).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use aleph_primitives::AuthorityId;
    use futures::{
        channel::{mpsc, mpsc::UnboundedReceiver, oneshot},
        pin_mut, FutureExt, StreamExt,
    };

    use super::{Protocol, ProtocolError};
    use crate::{
        crypto::AuthorityPen,
        validator_network::{
            mock::{keys, MockSplittable},
            Data,
        },
    };

    async fn prepare<D: Data>() -> (
        AuthorityId,
        AuthorityPen,
        AuthorityId,
        AuthorityPen,
        impl futures::Future<Output = Result<(), ProtocolError>>,
        impl futures::Future<Output = Result<(), ProtocolError>>,
        UnboundedReceiver<D>,
        UnboundedReceiver<(AuthorityId, oneshot::Sender<()>)>,
        UnboundedReceiver<(AuthorityId, Option<mpsc::UnboundedSender<D>>)>,
    ) {
        let (stream_incoming, stream_outgoing) = MockSplittable::new(4096);
        let (id_incoming, pen_incoming) = keys().await;
        let (id_outgoing, pen_outgoing) = keys().await;
        assert_ne!(id_incoming, id_outgoing);
        let (incoming_result_for_service, result_from_incoming) =
            mpsc::unbounded::<(AuthorityId, oneshot::Sender<()>)>();
        let (outgoing_result_for_service, result_from_outgoing) = mpsc::unbounded();
        let (data_for_user, data_from_incoming) = mpsc::unbounded::<D>();
        let incoming_handle = Protocol::V0.manage_incoming(
            stream_incoming,
            pen_incoming.clone(),
            incoming_result_for_service,
            data_for_user,
        );
        let outgoing_handle = Protocol::V0.manage_outgoing(
            stream_outgoing,
            pen_outgoing.clone(),
            id_incoming.clone(),
            outgoing_result_for_service,
        );
        (
            id_incoming,
            pen_incoming,
            id_outgoing,
            pen_outgoing,
            incoming_handle,
            outgoing_handle,
            data_from_incoming,
            result_from_incoming,
            result_from_outgoing,
        )
    }

    #[tokio::test]
    async fn send_data() {
        let (
            _id_incoming,
            _pen_incoming,
            _id_outgoing,
            _pen_outgoing,
            incoming_handle,
            outgoing_handle,
            mut data_from_incoming,
            _result_from_incoming,
            mut result_from_outgoing,
        ) = prepare::<Vec<i32>>().await;
        let incoming_handle = incoming_handle.fuse();
        let outgoing_handle = outgoing_handle.fuse();
        pin_mut!(incoming_handle);
        pin_mut!(outgoing_handle);
        let _data_for_outgoing = tokio::select! {
            _ = &mut incoming_handle => panic!("incoming process unexpectedly finished"),
            _ = &mut outgoing_handle => panic!("outgoing process unexpectedly finished"),
            result = result_from_outgoing.next() => {
                let (_, maybe_data_for_outgoing) = result.expect("outgoing should have resturned Some");
                let data_for_outgoing = maybe_data_for_outgoing.expect("successfully connected");
                data_for_outgoing
                    .unbounded_send(vec![4, 3, 43])
                    .expect("should send");
                data_for_outgoing
                    .unbounded_send(vec![2, 1, 3, 7])
                    .expect("should send");
                data_for_outgoing
            },
        };
        tokio::select! {
            _ = &mut incoming_handle => panic!("incoming process unexpectedly finished"),
            _ = &mut outgoing_handle => panic!("outgoing process unexpectedly finished"),
            v = data_from_incoming.next() => {
                assert_eq!(v, Some(vec![4, 3, 43]));
            },
        };
        tokio::select! {
            _ = &mut incoming_handle => panic!("incoming process unexpectedly finished"),
            _ = &mut outgoing_handle => panic!("outgoing process unexpectedly finished"),
            v = data_from_incoming.next() => {
                assert_eq!(v, Some(vec![2, 1, 3, 7]));
            },
        };
    }

    #[tokio::test]
    async fn closed_by_parent_service() {
        let (
            _id_incoming,
            _pen_incoming,
            id_outgoing,
            _pen_outgoing,
            incoming_handle,
            outgoing_handle,
            _data_from_incoming,
            mut result_from_incoming,
            _result_from_outgoing,
        ) = prepare::<Vec<i32>>().await;
        let incoming_handle = incoming_handle.fuse();
        let outgoing_handle = outgoing_handle.fuse();
        pin_mut!(incoming_handle);
        pin_mut!(outgoing_handle);
        tokio::select! {
            _ = &mut incoming_handle => panic!("incoming process unexpectedly finished"),
            _ = &mut outgoing_handle => panic!("outgoing process unexpectedly finished"),
            received = result_from_incoming.next() => {
                // we drop the exit oneshot channel, thus finishing incoming_handle
                let (received_id, _) = received.expect("should receive");
                assert_eq!(received_id, id_outgoing);
            },
        };
        incoming_handle
            .await
            .expect("closed manually, should finish with no error");
    }

    #[tokio::test]
    async fn parent_service_dead() {
        let (
            _id_incoming,
            _pen_incoming,
            _id_outgoing,
            _pen_outgoing,
            incoming_handle,
            outgoing_handle,
            _data_from_incoming,
            result_from_incoming,
            _result_from_outgoing,
        ) = prepare::<Vec<i32>>().await;
        std::mem::drop(result_from_incoming);
        let incoming_handle = incoming_handle.fuse();
        let outgoing_handle = outgoing_handle.fuse();
        pin_mut!(incoming_handle);
        pin_mut!(outgoing_handle);
        tokio::select! {
            e = &mut incoming_handle => match e {
                Err(ProtocolError::NoParentConnection) => (),
                Err(e) => panic!("unexpected error: {}", e),
                Ok(_) => panic!("successfully finished when parent dead"),
            },
            _ = &mut outgoing_handle => panic!("outgoing process unexpectedly finished"),
        };
    }

    #[tokio::test]
    async fn parent_user_dead() {
        let (
            _id_incoming,
            _pen_incoming,
            _id_outgoing,
            _pen_outgoing,
            incoming_handle,
            outgoing_handle,
            data_from_incoming,
            _result_from_incoming,
            mut result_from_outgoing,
        ) = prepare::<Vec<i32>>().await;
        std::mem::drop(data_from_incoming);
        let incoming_handle = incoming_handle.fuse();
        let outgoing_handle = outgoing_handle.fuse();
        pin_mut!(incoming_handle);
        pin_mut!(outgoing_handle);
        let _data_for_outgoing = tokio::select! {
            _ = &mut incoming_handle => panic!("incoming process unexpectedly finished"),
            _ = &mut outgoing_handle => panic!("outgoing process unexpectedly finished"),
            result = result_from_outgoing.next() => {
                let (_, maybe_data_for_outgoing) = result.expect("outgoing should have resturned Some");
                let data_for_outgoing = maybe_data_for_outgoing.expect("successfully connected");
                data_for_outgoing
                    .unbounded_send(vec![2, 1, 3, 7])
                    .expect("should send");
                data_for_outgoing
            },
        };
        tokio::select! {
            e = &mut incoming_handle => match e {
                Err(ProtocolError::NoUserConnection) => (),
                Err(e) => panic!("unexpected error: {}", e),
                Ok(_) => panic!("successfully finished when user dead"),
            },
            _ = &mut outgoing_handle => panic!("outgoing process unexpectedly finished"),
        };
    }

    #[tokio::test]
    async fn sender_dead_before_handshake() {
        let (
            _id_incoming,
            _pen_incoming,
            _id_outgoing,
            _pen_outgoing,
            incoming_handle,
            outgoing_handle,
            _data_from_incoming,
            _result_from_incoming,
            _result_from_outgoing,
        ) = prepare::<Vec<i32>>().await;
        std::mem::drop(outgoing_handle);
        match incoming_handle.await {
            Err(ProtocolError::HandshakeError(_)) => (),
            Err(e) => panic!("unexpected error: {}", e),
            Ok(_) => panic!("successfully finished when connection dead"),
        };
    }

    #[tokio::test]
    async fn sender_dead_after_handshake() {
        let (
            _id_incoming,
            _pen_incoming,
            _id_outgoing,
            _pen_outgoing,
            incoming_handle,
            outgoing_handle,
            _data_from_incoming,
            mut result_from_incoming,
            _result_from_outgoing,
        ) = prepare::<Vec<i32>>().await;
        let incoming_handle = incoming_handle.fuse();
        pin_mut!(incoming_handle);
        let (_, _exit) = tokio::select! {
            _ = &mut incoming_handle => panic!("incoming process unexpectedly finished"),
            _ = outgoing_handle => panic!("outgoing process unexpectedly finished"),
            out = result_from_incoming.next() => out.expect("should receive"),
        };
        // outgoing_handle got consumed by tokio::select!, the sender is dead
        match incoming_handle.await {
            Err(ProtocolError::ReceiveError(_)) => (),
            Err(e) => panic!("unexpected error: {}", e),
            Ok(_) => panic!("successfully finished when connection dead"),
        };
    }

    #[tokio::test]
    async fn receiver_dead_before_handshake() {
        let (
            _id_incoming,
            _pen_incoming,
            _id_outgoing,
            _pen_outgoing,
            incoming_handle,
            outgoing_handle,
            _data_from_incoming,
            _result_from_incoming,
            _result_from_outgoing,
        ) = prepare::<Vec<i32>>().await;
        std::mem::drop(incoming_handle);
        match outgoing_handle.await {
            Err(ProtocolError::HandshakeError(_)) => (),
            Err(e) => panic!("unexpected error: {}", e),
            Ok(_) => panic!("successfully finished when connection dead"),
        };
    }

    #[tokio::test]
    async fn receiver_dead_after_handshake() {
        let (
            _id_incoming,
            _pen_incoming,
            _id_outgoing,
            _pen_outgoing,
            incoming_handle,
            outgoing_handle,
            _data_from_incoming,
            mut result_from_incoming,
            _result_from_outgoing,
        ) = prepare::<Vec<i32>>().await;
        let outgoing_handle = outgoing_handle.fuse();
        pin_mut!(outgoing_handle);
        let (_, _exit) = tokio::select! {
            _ = incoming_handle => panic!("incoming process unexpectedly finished"),
            _ = &mut outgoing_handle => panic!("outgoing process unexpectedly finished"),
            out = result_from_incoming.next() => out.expect("should receive"),
        };
        // incoming_handle got consumed by tokio::select!, the receiver is dead
        match outgoing_handle.await {
            // We never get the SendError variant here, because we did not send anything
            // through data_for_outgoing.
            Err(ProtocolError::CardiacArrest) => (),
            Err(e) => panic!("unexpected error: {}", e),
            Ok(_) => panic!("successfully finished when connection dead"),
        };
    }

    #[tokio::test]
    async fn receiver_dead_after_handshake_try_send_error() {
        let (
            _id_incoming,
            _pen_incoming,
            _id_outgoing,
            _pen_outgoing,
            incoming_handle,
            outgoing_handle,
            _data_from_incoming,
            mut result_from_incoming,
            _result_from_outgoing,
        ) = prepare::<Vec<i32>>().await;
        let outgoing_handle = outgoing_handle.fuse();
        pin_mut!(outgoing_handle);
        let (_, _exit) = tokio::select! {
            _ = incoming_handle => panic!("incoming process unexpectedly finished"),
            _ = &mut outgoing_handle => panic!("outgoing process unexpectedly finished"),
            out = result_from_incoming.next() => out.expect("should receive"),
        };
        match outgoing_handle.await {
            Err(ProtocolError::CardiacArrest) => (),
            Err(e) => panic!("unexpected error: {}", e),
            Ok(_) => panic!("successfully finished when connection dead"),
        };
    }
}
