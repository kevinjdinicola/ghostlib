use std::collections::HashMap;

use anyhow::Result;

use futures_util::StreamExt;
use iroh::blobs::Hash;
use iroh::client::blobs::BlobStatus;
use iroh::client::docs::{Entry, LiveEvent};
use iroh::docs::{ContentStatus};
use iroh::docs::store::{Query, SortBy, SortDirection};


use crate::data::{BlobHash, BlobsSerializer, Doc, Node, PublicKey};
use crate::exchange::Message;
use crate::identity::Identification;


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
pub struct FileReadWriter {
    pub key_prefix: &'static str,
    pub doc: Doc,
    pub node: Node,
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct File {
    pub owner: PublicKey,
    pub blob: BlobHash,
    pub name: String,
    pub size: u64,
    pub downloaded: bool,
}


impl FileReadWriter {

    pub async fn get_all(&self) -> Result<Vec<File>> {
        let mut stream = self.doc.get_many(Query::key_prefix(self.key_prefix)).await?;
        let mut output: Vec<File> = vec![];
        while let Some(Ok(entry)) = stream.next().await {
            let bs = self.node.blobs.status(entry.content_hash()).await?;
            output.push(File {
                owner: entry.author().into(),
                blob: entry.content_hash().into(),
                name: String::from_utf8_lossy(entry.key()).into_owned(),
                size: entry.content_len(),
                downloaded: matches!(bs, BlobStatus::Complete{ .. })
            });
        }
        Ok(output)
    }
    pub async fn content_ready(&self, key: &str, entry: &Entry) -> Result<Option<File>> {
        if !key.starts_with(self.key_prefix) {
            return Ok(None)
        }
        Ok(Some(File {
            owner: entry.author().into(),
            blob: entry.content_hash().into(),
            name: key.into(),
            size: entry.content_len(),
            downloaded: true
        }))
    }
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

#[allow(async_fn_in_trait)]
pub trait IdentificationReader {
    async fn read(&self, hash: Hash) -> Result<Identification>;
}

impl IdentificationReader for Node {
    async fn read(&self, hash: Hash) -> Result<Identification> {
        self.deserialize_read_blob(hash).await
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

    pub fn register_incomplete(&mut self, entry: Entry) {
        self.entry_hashes.insert(entry.content_hash(), entry);
    }

    pub fn process_event(&mut self, le: LiveEvent) -> Result<Option<Entry>> {
        match le {
            LiveEvent::InsertLocal { entry } => {
                Ok(Some(entry))
            }
            LiveEvent::InsertRemote { entry, content_status, .. } => {
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
                    Ok(Some(entry))
                } else {
                    Ok(None)
                }
            }
            _ => { Ok(None) }
        }
    }

}