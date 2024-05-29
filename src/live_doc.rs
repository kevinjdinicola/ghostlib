use std::collections::HashMap;
use std::future::Future;
use std::pin::{pin, Pin};
use std::sync::{Arc, Weak};

use crate::identity::Identification;
use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use futures_util::StreamExt;
use iroh::blobs::Hash;
use iroh::client::docs::{Entry, LiveEvent};
use iroh::docs::ContentStatus;
use iroh::docs::store::{Query, SortBy, SortDirection};
use tokio::select;
use tokio::sync::mpsc::{Sender, Receiver};
use tokio::task::JoinHandle;
use crate::data::{BlobsSerializer, Doc, Node, PublicKey};
use crate::exchange::Message;
use crate::live_doc::LiveActorEvent::{NeighborDown, NeighborUp};

pub struct IdentificationReadWriter {
    pub key_path: &'static str,
    pub doc: Doc,
    pub node: Node,
}

pub struct MessagesReadWriter {
    pub key_prefix: &'static str,
    pub doc: Doc,
    pub node: Node,
}



impl IdentificationReadWriter {

    pub async fn get_all(&self) -> Result<Vec<Identification>> {
        let mut stream = self.doc.get_many(Query::key_exact(self.key_path)).await?;
        let mut output: Vec<Identification> = vec![];
        while let Some(Ok(entry)) = stream.next().await {
            output.push(self.node.deserialize_read_blob(entry.content_hash()).await?);
        }
        Ok(output)
    }

    pub async fn content_ready(&self, key: &str, entry: &Entry) -> Result<Option<Identification>> {
        if !key.eq(self.key_path) {
            return Ok(None)
        }
        let iden: Identification = self.node.deserialize_read_blob(entry.content_hash()).await?;
        Ok(Some(iden))
    }

}

impl MessagesReadWriter {
    pub async fn get_all(&self) -> Result<Vec<Message>> {
        let mut stream = self.doc.get_many(
            Query::key_prefix(self.key_prefix)
            .sort_by(SortBy::KeyAuthor, SortDirection::Asc)
        ).await?;

        let mut output: Vec<Message> = vec![];
        while let Some(Ok(entry)) = stream.next().await {
            output.push(self.node.deserialize_read_blob(entry.content_hash()).await?);
        }
        Ok(output)
    }

    pub async fn content_ready(&self, key: &str, entry: &Entry) -> Result<Option<Message>> {

        if !key.starts_with(self.key_prefix) {
            return Ok(None)
        }
        let msg: Message = self.node.deserialize_read_blob(entry.content_hash()).await?;
        Ok(Some(msg))
    }

}

// #[async_trait]
// impl ContentReadyReceiver for IdentificationReadWriter<'_> {
//     type ContentItem = Identification;
//
//     async fn handle_ready_content(&self, key: &str, entry: &Entry) -> Result<Option<Self::ContentItem>> {
//         if !key.eq(self.key_path) {
//             return Ok(None)
//         }
//         let iden: Identification = self.node.deserialize_read_blob(entry.content_hash()).await?;
//         Ok(Some(iden))
//     }
// }


#[derive(Debug)]
pub enum LiveActorEvent {
    ContentReady(Entry),
    NeighborUp(PublicKey),
    NeighborDown(PublicKey),
}


pub struct ContentReadyProcessor {
    entry_hashes: HashMap<Hash, Entry>,
}

impl ContentReadyProcessor {
    pub fn new() -> ContentReadyProcessor {
        ContentReadyProcessor {
            entry_hashes: HashMap::new(),
        }
    }

    pub fn process_event(&mut self, le: LiveEvent) -> Result<Option<Entry>> {
        match le {
            LiveEvent::InsertLocal { entry } => {
                let key = std::str::from_utf8(entry.key())?;
                Ok(Some(entry))
            }
            LiveEvent::InsertRemote { entry, content_status, .. } => {
                let key = std::str::from_utf8(entry.key())?;
                match content_status {
                    ContentStatus::Complete => {
                        Ok(Some(entry))
                    }
                    _ => {
                        self.entry_hashes.insert(entry.content_hash(), entry);
                        Ok(None)
                    }
                }
            }
            LiveEvent::ContentReady { hash } => {
                if let Some(entry) = self.entry_hashes.remove(&hash) {
                    let key = std::str::from_utf8(entry.key())?;
                    Ok(Some(entry))
                } else {
                    Ok(None)
                }
            }
            _ => { Ok(None) }
        }
    }

}

pub struct LiveEventActor {
    outbox: Sender<LiveActorEvent>,
    entry_hashes: HashMap<Hash, Entry>,
}

impl LiveEventActor {

    pub fn new(outbox: Sender<LiveActorEvent>) -> LiveEventActor {
        LiveEventActor {
            outbox,
            entry_hashes: HashMap::new(),
        }
    }

    pub async fn run(mut self, doc: &Doc) -> Result<()> {
        let mut stream = doc.subscribe().await?;
        tokio::spawn(async move {
            // let mut stream = doc.subscribe().await?;
            self.entry_hashes = HashMap::new();
            while let Some(event) = stream.next().await {
                let event = match event {
                    Ok(e) => { e }
                    Err(err) => {
                        eprintln!("err {}", err); continue
                    }
                };
                self.live_event_handler(event).await.expect("Worker loop shat itself")
            }
        });
        Ok(())
    }

    async fn live_event_handler(&mut self, le: LiveEvent) -> Result<()> {
        match le {
            LiveEvent::InsertLocal { entry } => {
                self.handle_ready_blob(entry).await?;
            }
            LiveEvent::InsertRemote { entry, content_status, .. } => {
                match content_status {
                    ContentStatus::Complete => {
                        self.handle_ready_blob(entry).await?;
                    }
                    _ => {
                        self.entry_hashes.insert(entry.content_hash(), entry);
                    }
                }
            }
            LiveEvent::ContentReady { hash } => {
                if let Some(entry) = self.entry_hashes.remove(&hash) {
                    self.handle_ready_blob(entry).await?
                }
            }
            LiveEvent::NeighborUp(pk) => {
                self.outbox.send(NeighborUp((*pk.as_bytes()).into())).await?
            }
            LiveEvent::NeighborDown(pk) => {
                self.outbox.send(NeighborDown((*pk.as_bytes()).into())).await?
            }
            LiveEvent::SyncFinished(_) => {}
        }
        Ok(())
    }

    async fn handle_ready_blob(&mut self, entry: Entry) -> Result<()> {
        self.outbox.send(LiveActorEvent::ContentReady(entry)).await?;
        Ok(())
    }
}