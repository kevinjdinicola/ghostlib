
use std::sync::{Arc};
use bytes::Bytes;
use tokio::sync::{mpsc};
use tokio::sync::mpsc::{Receiver, Sender};
use crate::data::{BlobHash, Node};
use anyhow::Result;
use iroh::blobs::Hash;
use tracing::debug;
use crate::dispatch::blob::BlobDataState::{Failed, Loaded, Loading};

#[derive(uniffi::Enum)]
pub enum BlobDataState {
    Empty,
    Loading,
    Loaded(BlobHash, Vec<u8>),
    Failed(String)
}

#[uniffi::export(with_foreign)]
pub trait BlobDataResponder: Send + Sync {
    fn update(&self, state: BlobDataState);
    fn hash(&self) -> Option<BlobHash>;
}

#[derive(uniffi::Object)]
pub struct BlobDataDispatcher {
    bdr_tx: Sender<Arc<dyn BlobDataResponder>>,
}

async fn worker_loop(node: Node, mut bdr_rx: Receiver<Arc<dyn BlobDataResponder>>) {
    // this will die when the tx is dropped, nice little actor
    debug!("BlobDataDispatcher worker loop started");

    while let Some(bdr) = bdr_rx.recv().await {
        let nclone = node.clone();
        tokio::spawn(async move {
            hydrate_responder(nclone, bdr).await
        });
    }
    debug!("BlobDataDispatcher worker ending");
}

async fn hydrate_responder(node: Node, bdr: Arc<dyn BlobDataResponder>) {
    if let Some(bh) = bdr.hash() {
        bdr.update(Loading);

        let res: Result<Bytes> = load_bytes(&node, bh.as_bytes().into()).await;
        match res {
            Ok(b) => {
                bdr.update(Loaded(bh, b.into()))
            }
            Err(e) => {
                bdr.update(Failed(e.to_string()))
            }
        }
    }
}

async fn load_bytes(node: &Node, hash: Hash) -> Result<Bytes> {
    let mut r = node.blobs.read(hash).await?;
    let b = r.read_to_bytes().await?;
    Ok(b)
}



impl BlobDataDispatcher {
    pub fn new(node: Node) -> BlobDataDispatcher {
        let (tx, rx) = mpsc::channel(16);

        tokio::spawn(async move {
            worker_loop(node, rx).await;
        });

        BlobDataDispatcher {
            bdr_tx: tx
        }
    }
}

#[uniffi::export]
impl BlobDataDispatcher {
    pub fn hydrate(&self, bdr: Arc<dyn BlobDataResponder>) {
        if bdr.hash().is_some() {
            self.bdr_tx.try_send(bdr).expect("Failed to send BDR");
        }
    }
}
