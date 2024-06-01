
// struct details of a context, which is ALIVE.  not a record for serialization

use std::collections::HashSet;
use std::ops::Deref;
use std::sync::Arc;

use anyhow::Result;
use futures_util::StreamExt;
use iroh::base::node_addr::AddrInfoOptions;
use iroh::base::ticket::Ticket;
use iroh::client::docs::{Entry, LiveEvent};
use iroh::client::docs::ShareMode::Write;
use iroh::net::NodeAddr;
use tokio::sync::{broadcast, RwLock};
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::sync::broadcast::error::SendError;
use tracing::{debug, error, info, warn};

use crate::data::{BlobHash, Doc, ExchangeId, Node, PublicKey, save_on_doc_as_key};
use crate::exchange::{FILE_KEY_PREFIX, IDENTIFICATION_KEY, Message, MESSAGE_KEY_PREFIX};
use crate::exchange::context::ContextEvents::{Loaded, SyncFinished};
use crate::identity::Identification;
use crate::live_doc::{ContentReadyProcessor, File, FileReadWriter, IdentificationReadWriter, MessagesReadWriter};

#[derive(Clone)]
pub struct ExchangeContext(Arc<ExchangeContextInner>);
pub struct ExchangeContextInner {
    id: ExchangeId,
    node: Node,
    pub doc: Doc,
    broadcast: Sender<ContextEvents>,
    participants: RwLock<Vec<Identification>>,
    messages: RwLock<Vec<Message>>,
    connected: RwLock<HashSet<PublicKey>>,
    files: RwLock<Vec<File>>,
    identification_rw: IdentificationReadWriter,
    messages_rw: MessagesReadWriter,
    file_rw: FileReadWriter,

}

#[derive(Clone, Debug)]
pub enum ContextEvents {
    Loaded,
    Join(PublicKey),
    MessageReceived(Message),
    LiveConnectionsUpdated(usize),
    FileUpdated(File),
    SyncFinished,
}

fn make_message_key(message: &Message) -> String {
    format!("{MESSAGE_KEY_PREFIX}/{}",message.date.to_string())
}

impl ExchangeContext {
    pub fn new(id: ExchangeId, doc: Doc, node: Node) -> ExchangeContext {

        let identification_rw: IdentificationReadWriter = IdentificationReadWriter {
            key_path: IDENTIFICATION_KEY,
            doc: doc.clone(),
            node: node.clone(),
        };
        let messages_rw: MessagesReadWriter = MessagesReadWriter {
            key_prefix: MESSAGE_KEY_PREFIX,
            doc: doc.clone(),
            node: node.clone()
        };
        let file_rw: FileReadWriter = FileReadWriter {
            key_prefix: FILE_KEY_PREFIX,
            doc: doc.clone(),
            node: node.clone(),
        };

        let (tx, _rx): (Sender<ContextEvents>, Receiver<ContextEvents>) = broadcast::channel(16);
        // start streaming and shit?
        let ectx = ExchangeContext(Arc::new(ExchangeContextInner {
            id,
            doc,
            node,
            // service,
            broadcast: tx,
            participants: RwLock::new(vec![]),
            messages: RwLock::new(vec![]),
            connected: RwLock::new(HashSet::new()),
            files: RwLock::new(vec![]),
            identification_rw,
            messages_rw,
            file_rw,
        }));

        ectx
    }
    pub async fn start(&self) -> Result<()> {
        let worker = self.clone();
        tokio::spawn(async move {
            match worker.worker_loop().await {
                Ok(_) => {
                    debug!("worker ended successfully")
                }
                Err(err) => {
                    error!("worker ended in error: {}", err)
                }
            }
        });
        Ok(())
    }

    // pub fn shutdown(&self) {
    //     Handle::current().block_on(async {
    //
    //     });
    // }

    pub fn id(&self) -> ExchangeId {
        self.0.id
    }

    pub fn subscribe(&self) -> Receiver<ContextEvents> {
        self.broadcast.subscribe()
    }

    pub async fn send_message(&self, message: &Message) -> Result<()>{
        let key = make_message_key(message);
        save_on_doc_as_key(&self.node, &self.doc, &message.pk, &key, &message).await
    }
    pub async fn generate_join_ticket(&self) -> Result<String> {
        let ticket = self.doc.share(Write, AddrInfoOptions::Addresses).await?;
        // let bytes = Ticket::to_bytes(&ticket);
        Ok(Ticket::serialize(&ticket))
    }

    pub async fn sleep(&self) -> Result<()> {
        self.doc.leave().await
    }

    pub async fn shutdown(&self) -> Result<()> {
        info!("shutting down context {}", self.id);
        self.doc.leave().await?;
        self.doc.close().await
    }

    async fn broadcast(&self, event: ContextEvents) -> Result<usize, SendError<ContextEvents>> {
        // todo will this ever race condition? should i just catch errors?
        if self.broadcast.receiver_count() > 0 {
            debug!("broadcasting {:?}", event);
            self.broadcast.send(event)
        } else {
            debug!("Tried to broadcast, but no one is listening {:?}", event);
            Ok(0)
        }
    }

    pub async fn live_connection_count(&self) -> Result<usize> {
        let read = self.connected.read().await;
        let count = read.len();
        Ok(count)
    }

    pub async fn sync(&self) -> Result<()> {
        if let Some(peers) = self.doc.get_sync_peers().await? {
            let peers: Vec<NodeAddr>= peers.iter().map(|peer_id_bytes| {
                NodeAddr::new(iroh::net::key::PublicKey::from_bytes(peer_id_bytes).unwrap())
            }).collect();
            debug!("starting sync with {:?}", peers);
            self.doc.start_sync(peers).await?;
        }

        Ok(())
    }

    async fn worker_loop(self) -> Result<()> {
        info!("staring worker loop for {}", self.id);
        self.initial_data_load().await?;
        self.broadcast(Loaded).await?;
        debug!("persistent data loaded, starting to subscribe");
        let mut stream = self.doc.subscribe().await?;

        // let mut entry_hashes: HashMap<Hash, Entry> = HashMap::new();
        let mut crp = ContentReadyProcessor::new();

        while let Some(Ok(event)) = stream.next().await {
            let handle_result: Result<()> = match event {
                LiveEvent::NeighborUp(pk) => {
                    self.neighbor_changed((*pk.as_bytes()).into(), true).await?;
                    Ok(())
                }
                LiveEvent::NeighborDown(pk) => {
                    self.neighbor_changed((*pk.as_bytes()).into(), false).await?;
                    Ok(())
                }
                LiveEvent::SyncFinished(e) => {
                    debug!("sync finished with {}", e.peer);
                    self.broadcast(SyncFinished).await?;
                    Ok(())
                },
                _ => {
                    if let Some(entry) = crp.process_event(event)? {
                        let key = std::str::from_utf8(entry.key())?;

                        if let Some(iden) = self.identification_rw.content_ready(key, &entry).await? {
                            self.new_participant_joined(iden).await?;
                        } else if let Some(msg) = self.messages_rw.content_ready(key, &entry).await? {
                            self.new_message_received(msg).await?
                        } else if let Some(file) = self.file_rw.content_ready(key, &entry).await? {
                            self.file_updated(file).await?;
                        } else {
                            warn!("Downloaded blob but cant handle: {}", key);
                        }
                        // self.handle_ready_blob(entry).await?;
                    }
                    Ok(())
                }
            };
            if let Err(e) = handle_result {
                error!("ExchangeContext worker got error {}", e);
            }
        };
        info!("Worker loop ended for {}", self.id);
        Ok(())
    }

    async fn neighbor_changed(&self, pk: PublicKey, joined: bool) -> Result<()> {
        debug!("neighbor down {pk}");
        let mut write_guard = self.connected.write().await;
        if joined {
            (*write_guard).insert(pk);
        } else {
            (*write_guard).remove(&pk);
        }
        let count = write_guard.len();
        drop(write_guard);
        self.broadcast(ContextEvents::LiveConnectionsUpdated(count)).await?;
        Ok(())
    }
    async fn new_participant_joined(&self, iden: Identification) -> Result<()> {
        let pk = iden.public_key().clone();
        let pname = iden.name.clone();
        let mut parts_write_guard = self.participants.write().await;
        (*parts_write_guard).push(iden);
        drop(parts_write_guard);
        debug!("new participant joined! {}, broadcasting", pname);
        self.broadcast(ContextEvents::Join(pk)).await?;
        Ok(())
    }

    pub async fn get_file(&self, owner: &PublicKey, key: &str) -> Result<Option<BlobHash>> {
        let res = self.doc.get_exact(owner.into(), key, false).await?
            .map(|e| e.content_hash().into() );
        Ok(res)
    }
    pub async fn add_file(&self, owner: PublicKey, key: &str, entry: &Entry) -> Result<()> {
        let key = format!("{FILE_KEY_PREFIX}/{key}");
        self.doc.set_hash(owner.into(), key.clone(), entry.content_hash(), entry.content_len()).await?;

        self.file_updated(File {
            owner,
            blob: entry.content_hash().into(),
            name: key,
            size: entry.content_len(),
            downloaded: true,
        }).await?;

        Ok(())
    }

    async fn new_message_received(&self, msg: Message) -> Result<()> {
        let mut msg_write_guard = self.messages.write().await;
        let send_me = msg.clone();
        (*msg_write_guard).push(msg);
        drop(msg_write_guard);
        self.broadcast(ContextEvents::MessageReceived(send_me)).await?;
        Ok(())
    }

    async fn file_updated(&self, ob: File) -> Result<()> {
        {
            let mut file_lock = self.files.write().await;
            let mut replaced = false;
            for item in &mut *file_lock {
                if ob.name.eq(&item.name) && ob.owner == item.owner {
                    *item = ob.clone();
                    replaced = true;
                }
            }
            if !replaced {
                file_lock.push(ob.clone());
            }
        }

        self.broadcast(ContextEvents::FileUpdated(ob)).await?;
        Ok(())
    }

    pub async fn participants(&self) -> Vec<Identification> {
        let read_guard = self.participants.read().await;
        read_guard.clone()
    }

    pub async fn messages(&self) -> Vec<Message> {
        let read_guard = self.messages.read().await;
        read_guard.clone()
    }

    async fn initial_data_load(&self) -> Result<()> {
        // load participants
        let mut participants = self.identification_rw.get_all().await?;
        {
            let mut parts_write_guard = self.participants.write().await;
            parts_write_guard.append(&mut participants);
        }

        // messages
        let mut saved_msgs = self.messages_rw.get_all().await?;
        {
            let mut msgs_write_guard = self.messages.write().await;
            msgs_write_guard.append(&mut saved_msgs);
            msgs_write_guard.sort_by_key(|m| m.date);
        }

        // files
        let mut loaded_files = self.file_rw.get_all().await?;
        {
            let mut files_write_guard = self.files.write().await;
            files_write_guard.append(&mut loaded_files);
        }

        Ok(())
    }
}

impl Deref for ExchangeContext {
    type Target = ExchangeContextInner;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}
