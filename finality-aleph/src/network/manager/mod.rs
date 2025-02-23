use codec::{Decode, Encode};

use crate::{
    crypto::Signature,
    network::{Data, Multiaddress},
    NodeIndex, SessionId,
};

mod compatibility;
mod connections;
mod discovery;
mod service;
mod session;

pub use compatibility::VersionedAuthentication;
use connections::Connections;
pub use discovery::{Discovery, DiscoveryMessage};
pub use service::{
    Config as ConnectionManagerConfig, Service as ConnectionManager, SessionCommand,
    IO as ConnectionIO,
};
pub use session::{Handler as SessionHandler, HandlerError as SessionHandlerError};
/// Data validators use to authenticate themselves for a single session
/// and disseminate their addresses.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct AuthData<M: Multiaddress> {
    addresses: Vec<M>,
    node_id: NodeIndex,
    session_id: SessionId,
}

impl<M: Multiaddress> AuthData<M> {
    pub fn session(&self) -> SessionId {
        self.session_id
    }

    pub fn creator(&self) -> NodeIndex {
        self.node_id
    }

    pub fn addresses(&self) -> Vec<M> {
        self.addresses.clone()
    }
}

/// A full authentication, consisting of a signed AuthData.
pub type Authentication<M> = (AuthData<M>, Signature);

/// Data inside session, sent to validator network.
pub type DataInSession<D> = (D, SessionId);

impl<D: Data, M: Multiaddress> From<DataInSession<D>> for NetworkData<D, M> {
    fn from(data: DataInSession<D>) -> Self {
        NetworkData::Data(data.0, data.1)
    }
}

/// The data that should be sent to the network service.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum NetworkData<D: Data, M: Multiaddress> {
    Meta(DiscoveryMessage<M>),
    Data(D, SessionId),
}
