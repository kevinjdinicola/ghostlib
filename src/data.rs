// module to contain things that control how i deal with and interact with data layers and persistence
// im gonna put some iroh sugar in here

use std::{fmt, ptr};
use std::fmt::{Debug, Display, Formatter};

use anyhow::anyhow;
use iroh::base32;
use iroh::bytes::Hash;
use iroh::client::{BlobAddOutcome, BlobsClient, Doc as IrohDoc};
use iroh::node::Node as IrohNode;
use iroh::rpc_protocol::{ProviderRequest, ProviderResponse, ProviderService};
use iroh::sync::{AuthorId, NamespaceId};
use iroh::sync::store::Query;
use quic_rpc::ServiceConnection;
use quic_rpc::transport::flume::FlumeConnection;
use serde::{Deserialize, Serialize};
use uniffi::export;


// global 'store' type for how im dealing with nodes everywhere
pub type Store = iroh::bytes::store::fs::Store;
pub type Node = IrohNode<Store>;
pub type Doc = IrohDoc<FlumeConnection<ProviderResponse, ProviderRequest>>;


// #[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Hash, Clone, Copy)]
// pub struct WideId([u8; 32]);

// defined in this way so that it can be a sized type, stored on the stack
// not requiring additional allocations and easily passable into uniffi
// its really just a silly way of writing a [u8; 32]
#[repr(C)]
#[derive(Serialize, Deserialize, PartialEq, Eq, Hash, Clone, Copy)]
#[derive(uniffi::Record)]
pub struct WideId {
    p1: u64,
    p2: u64,
    p3: u64,
    p4: u64,
}

#[uniffi::export]
pub fn wideid_to_string(wide_id: WideId) -> String {
    format!("{wide_id}")
}

impl WideId {
    pub fn to_bytes(self) -> [u8; 32] {
        self.into()
    }
    pub fn as_bytes(&self) -> [u8; 32] {
        self.into()
    }
}

impl From<[u8; 32]> for WideId {
    fn from(value: [u8; 32]) ->  Self {
        unsafe {
            ptr::read(value.as_ptr() as *const Self)
        }
    }
}

impl From<WideId> for [u8; 32] {
    fn from(value: WideId) ->  Self {
        unsafe {
            ptr::read((&value as *const WideId) as *const Self)
        }
    }
}
impl From<&WideId> for [u8; 32] {
    fn from(value: &WideId) ->  Self {
        unsafe {
            ptr::read((value as *const WideId) as *const Self)
        }
    }
}

impl From<WideId> for AuthorId {
    fn from(value: WideId) -> Self {
        AuthorId::from(<[u8; 32] as Into<AuthorId>>::into(value.to_bytes().into()))
    }
}
impl From<&WideId> for AuthorId {
    fn from(value: &WideId) -> Self {
        AuthorId::from(<[u8; 32] as Into<AuthorId>>::into(value.to_bytes()))
    }
}

impl Display for WideId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", base32::fmt(self.to_bytes()))
    }
}

impl Debug for WideId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", base32::fmt(self.to_bytes()))
    }
}

impl From<AuthorId> for WideId {
    fn from(value: AuthorId) -> Self {
        value.to_bytes().into()
    }
}

impl From<NamespaceId> for WideId {
    fn from(value: NamespaceId) -> Self {
        value.to_bytes().into()
    }
}
impl From<WideId> for NamespaceId {
    fn from(value: WideId) -> Self {
        value.to_bytes().into()
    }
}




pub type PublicKey = WideId;
pub type ExchangeId = WideId;


pub async fn load_from_doc_at_key<T: for<'a> Deserialize<'a>>(node: &Node, doc: &Doc, pk: Option<&PublicKey>, key: &str) -> anyhow::Result<T> {
    let e = if let Some(pk) = pk {
        doc.get_exact(pk.into(), key, false).await?.ok_or_else(|| anyhow!("not here"))?
    } else {
        doc.get_one(Query::key_exact(key)).await?.ok_or_else(|| anyhow!("not here"))?
    };
    Ok(node.blobs.deserialize_read_blob(e.content_hash()).await?)
}
pub async fn save_on_doc_as_key<T: Serialize>(node: &Node, doc: &Doc, pk: &PublicKey, key: &str, data: T) -> anyhow::Result<()> {
    let new_data = node.blobs.serialize_write_blob(data).await?;
    doc.set_hash(pk.into(), String::from(key), new_data.hash, new_data.size).await?;
    Ok(())
}
#[allow(async_fn_in_trait)]
pub trait SerializingBlobsClient {

    async fn deserialize_read_blob<T: for<'a> Deserialize<'a>>(&self, hash: Hash) -> anyhow::Result<T>;

    async fn serialize_write_blob<T: Serialize>(&self, data: T) -> anyhow::Result<BlobAddOutcome>;
}

impl<C> SerializingBlobsClient for BlobsClient<C>
    where
        C: ServiceConnection<ProviderService>,
{
    async fn deserialize_read_blob<T: for<'a> Deserialize<'a>>(&self, hash: Hash) -> anyhow::Result<T> {
        let bytes = self.read_to_bytes(hash).await?;
        let r = flexbuffers::Reader::get_root(bytes.iter().as_slice()).unwrap();
        let decoded = T::deserialize(r).expect("Deserialization failed");
        Ok(decoded)
    }

    async fn serialize_write_blob<T: Serialize>(&self, data: T) -> anyhow::Result<BlobAddOutcome> {
        let mut s = flexbuffers::FlexbufferSerializer::new();
        data.serialize(&mut s).expect("Serialization failure");
        self.add_bytes(s.take_buffer()).await
    }
}






