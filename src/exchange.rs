use std::cmp::PartialEq;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use iroh::base::node_addr::AddrInfoOptions;
use iroh::base::ticket::Ticket;
use iroh::bytes::Hash;
use iroh::client::{Entry, LiveEvent};
use iroh::net::NodeAddr;
use iroh::rpc_protocol::ShareMode::Write;
use iroh::sync::ContentStatus;
use iroh::sync::store::Query;
use iroh::ticket::DocTicket;
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, RwLock};
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::sync::broadcast::error::SendError;

use crate::data::{Doc, ExchangeId, load_from_doc_at_key, Node, PublicKey, save_on_doc_as_key, SerializingBlobsClient, WideId};
use crate::exchange::ContextEvents::Loaded;
use crate::exchange::Events::ExchangeListDidUpdate;
use crate::exchange::ExchangeManifest::MutualBinary;
use crate::identity::{Identification, Service as IdentityService};
use crate::settings::Service as SettingsService;

// lives on exchange doc
const MANIFEST_KEY: &str = "manifest.v1";
const IDENTIFICATION_KEY: &str = "identification";
const MESSAGE_KEY_PREFIX: &str = "messages";

// actually saved on the settings doc
const SETTINGS_EXCHANGES_LIST_KEY: &str = "exchange/tracked_exchanges";



// struct details of a context, which is ALIVE.  not a record for serialization

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct Message {
    pub pk: PublicKey,
    pub date: DateTime<Utc>,
    pub text: String
}

impl Message {
    pub fn new(pk: &PublicKey, text: &str) -> Message {
        Message {
            pk: pk.clone(),
            date: Utc::now(),
            text: String::from(text)
        }
    }
}

#[derive(Clone)]
pub struct ExchangeContext(Arc<ExchangeContextInner>);
pub struct ExchangeContextInner {
    id: ExchangeId,
    node: Node,
    pub doc: Doc,
    // service: Service,
    broadcast: Sender<ContextEvents>,
    participants: RwLock<Vec<Identification>>,
    messages: RwLock<Vec<Message>>,
    connected: RwLock<HashSet<iroh::net::key::PublicKey>>,
}

#[derive(Clone, Debug)]
pub enum ContextEvents {
    Loaded,
    Join(PublicKey),
    MessageReceived(Message),
    LiveConnectionsUpdated(usize)
}

fn make_message_key(message: &Message) -> String {
    format!("{MESSAGE_KEY_PREFIX}/{}",message.date.to_string())
}

impl ExchangeContext {
    pub fn new(id: ExchangeId, doc: Doc, node: Node) -> ExchangeContext {
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
            connected: RwLock::new(HashSet::new())
        }));

        ectx
    }

    pub fn start(&self) {
        let worker = self.clone();
        Handle::current().spawn(async move {
            match worker.worker_loop().await {
                Ok(_) => {
                    println!("worker ended successfully")
                }
                Err(err) => {
                    eprintln!("worker ended in error: {}", err)
                }
            }
        });
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
        Ok(Ticket::serialize(&ticket))
    }

    pub async fn sleep(&self) -> Result<()> {
        self.doc.leave().await
    }

    pub async fn shutdown(&self) -> Result<()> {
        println!("shutting down context {}", self.id);
        self.doc.leave().await?;
        self.doc.close().await
    }

    async fn broadcast(&self, event: ContextEvents) -> Result<usize, SendError<ContextEvents>> {
        // todo will this ever race condition? should i just catch errors?
        if self.broadcast.receiver_count() > 0 {
           self.broadcast.send(event)
        } else {
            println!("not sending cuz no ones listening");
            Ok(0)
        }
    }

    pub async fn live_connection_count(&self) -> Result<usize> {
        let read = self.connected.read().await;
        let count = read.len();
        Ok(count)
    }

    async fn sync(&self) -> Result<()> {
        if let Some(peers) = self.doc.get_sync_peers().await? {
            let peers: Vec<NodeAddr>= peers.iter().map(|peer_id_bytes| {
               NodeAddr::new(iroh::net::key::PublicKey::from_bytes(peer_id_bytes).unwrap())
            }).collect();
            println!("starting sync with {:?}", peers);
            self.doc.start_sync(peers).await?;
        }

        Ok(())
    }

    async fn worker_loop(self) -> Result<()> {
        println!("staring worker loop for {}", self.id);
        self.initial_data_load().await?;
        self.broadcast(Loaded).await?;
        println!("persistent data loaded, starting to subscribe");
        let mut stream = self.doc.subscribe().await?;

        let mut entry_hashes: HashMap<Hash, Entry> = HashMap::new();
        println!("listening for changes");
        while let Some(event) = stream.next().await {
            let event: Result<LiveEvent> = event;
            let event: LiveEvent = match event {
                Ok(e) => { e }
                Err(_) => { continue }
            };

            let handle_result: Result<()> = match event {
                LiveEvent::InsertLocal { entry } => {
                    println!("insert local {}", String::from_utf8_lossy(entry.key()));
                    self.handle_ready_blob(entry).await
                }
                LiveEvent::InsertRemote { entry, content_status, .. } => {
                    println!("insert remote {}", String::from_utf8_lossy(entry.key()));
                    match content_status {
                        ContentStatus::Complete => {
                            println!("remote was complete, lets handle");
                            self.handle_ready_blob(entry).await
                        },
                        _ => {
                            println!("remote incomplete for {}", entry.content_hash());
                            entry_hashes.insert(entry.content_hash(), entry);
                            Ok(())
                        }
                    }
                }
                LiveEvent::ContentReady { hash } => {
                    println!("content ready {hash}");
                    if let Some(entry) = entry_hashes.remove(&hash) {
                        self.handle_ready_blob(entry).await
                    } else {
                        println!("hash ready for something i wasnt tracking... {hash}");
                        Ok(())
                    }

                }
                LiveEvent::NeighborUp(pk) => {
                    println!("neighbor up {pk}");
                    let mut write_guard = self.connected.write().await;
                    (*write_guard).insert(pk);
                    let count = write_guard.len();
                    drop(write_guard);
                    self.broadcast(ContextEvents::LiveConnectionsUpdated(count)).await?;
                    Ok(())
                }
                LiveEvent::NeighborDown(pk) => {
                    println!("neighbor down {pk}");
                    let mut write_guard = self.connected.write().await;
                    (*write_guard).remove(&pk);
                    let count = write_guard.len();
                    drop(write_guard);
                    self.broadcast(ContextEvents::LiveConnectionsUpdated(count)).await?;
                    Ok(())
                }
                LiveEvent::SyncFinished(..) => {
                    println!("sync finished");
                    Ok(())
                }
            };
            if let Err(e) = handle_result {
                eprintln!("whats going on in my subscriber!! {}", e);
            }
        };
        println!("Worker loop ended for {}", self.id);
        Ok(())
    }

    async fn handle_ready_blob(&self, entry: Entry) -> Result<()> {
        let key = String::from_utf8_lossy(entry.key());
        if key.eq(IDENTIFICATION_KEY) {
            println!("we found an identification key!!");
            let iden: Identification = self.node.blobs.deserialize_read_blob(entry.content_hash()).await?;
            self.new_participant_joined(iden).await
        } else if key.starts_with(MESSAGE_KEY_PREFIX) {
            println!("we got a new message!");
            let msg: Message = self.node.blobs.deserialize_read_blob(entry.content_hash()).await?;
            self.new_message_received(msg).await
        } else {
            Ok(())
        }

    }

    async fn new_participant_joined(&self, iden: Identification) -> Result<()> {
        let pk = iden.public_key().clone();
        let mut parts_write_guard = self.participants.write().await;
        (*parts_write_guard).push(iden);
        drop(parts_write_guard);
        self.broadcast(ContextEvents::Join(pk)).await?;
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
        let mut parts_write_guard = self.participants.write().await;
        let mut saved_participants = self.doc.get_many(Query::key_exact(IDENTIFICATION_KEY)).await?;
        while let Some(Ok(entry)) = saved_participants.next().await {
            let entry: Entry = entry;
            let iden: Identification = self.node.blobs.deserialize_read_blob(entry.content_hash()).await?;
            (*parts_write_guard).push(iden);
        }
        drop(parts_write_guard);

        let mut msgs_write_guard = self.messages.write().await;
        let mut saved_msgs = self.doc.get_many(Query::key_prefix(MESSAGE_KEY_PREFIX)).await?;
        while let Some(Ok(entry)) = saved_msgs.next().await {
            let entry: Entry = entry;
            let msg: Message = self.node.blobs.deserialize_read_blob(entry.content_hash()).await?;
            (*msgs_write_guard).push(msg);
        };
        (*msgs_write_guard).sort_by_key(|m| m.date);
        drop(msgs_write_guard);

        // maybe do messages here
        Ok(())
    }
}

impl Deref for ExchangeContext {
    type Target = ExchangeContextInner;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub enum ExchangeManifest {
    MutualBinary // no addl details here
}

#[derive(Clone, Debug)]
pub enum Events {
    ExchangeListDidUpdate
}

pub struct ServiceInner {
    node: Node,
    settings: SettingsService,
    identity: IdentityService,
    // these contexts represent
    // the datastructre of an active exchange
    // that is potentially listening for new messages
    exchanges: RwLock<Vec<ExchangeContext>>,
    broadcast: Sender<Events>,
}

#[derive(Clone)]
pub struct Service(Arc<ServiceInner>);
impl Deref for Service {
    type Target = ServiceInner;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}


impl Service {
    pub fn new(node: Node, settings: SettingsService, identity: IdentityService) -> Service {
        let (tx, _rx): (Sender<Events>, Receiver<Events>) = broadcast::channel(16);

        let inner = ServiceInner {
            node,
            settings,
            identity,
            exchanges: RwLock::new(vec![]),
            broadcast: tx,
        };

        Service(Arc::new(inner))
    }
    async fn broadcast(&self, event: Events) -> Result<usize, SendError<Events>> {
        // todo will this ever race condition? should i just catch errors?
        if self.broadcast.receiver_count() > 0 {
            self.broadcast.send(event)
        } else {
            println!("not sending cuz no ones listening");
            Ok(0)
        }
    }

    pub fn subscribe(&self) -> Receiver<Events> {
        self.broadcast.subscribe()
    }

    pub async fn create_exchange(&self, register: bool) -> Result<ExchangeContext> {
        let doc = self.node.docs.create().await?;
        let myself = self.identity.assumed_identity().await
            .ok_or_else(|| anyhow!("cant create exchange with no assumed identity"))?;
        let pk = myself.public_key();
        let manifest = MutualBinary; // only kind for now

        save_on_doc_as_key(&self.node, &doc, pk, MANIFEST_KEY, manifest).await?;
        save_on_doc_as_key(&self.node, &doc, pk, IDENTIFICATION_KEY, &myself).await?;
        let ex_id: ExchangeId = doc.id().into();
        println!("created doc for exchange {}, {:?}", doc.id(), doc.status().await?);

        let ex = ExchangeContext::new(ex_id, doc, self.node.clone());
        ex.start();

        if register {
            // save namespace id to list of exchange namespace ids
            self.save_exchange_id(ex_id.clone()).await?;
            self.reload_contexts(Some(ex.clone()), None).await?;
        }
        // reload contexts from id list, will create whats missing

        Ok(ex)
    }
    pub async fn join_exchange(&self, join_ticket: &str, register: bool) -> Result<ExchangeContext> {
        let join_ticket: DocTicket = Ticket::deserialize(join_ticket)?;
        let doc = self.node.docs.import(join_ticket).await?;
        let myself = self.identity.assumed_identity().await
            .ok_or_else(|| anyhow!("cant create exchange with no assumed identity"))?;
        // todo - implement some logic where i wait here to see
        // if i actually joined and if its really valid?
        let ex_id: ExchangeId = doc.id().into();
        save_on_doc_as_key(&self.node, &doc, myself.public_key(), IDENTIFICATION_KEY, &myself).await?;

        let ex = ExchangeContext::new(ex_id, doc, self.node.clone());
        ex.start();

        if register {
            self.save_exchange_id(ex_id.clone()).await?;
            self.reload_contexts(Some(ex.clone()), None).await?;
        }

        Ok(ex)
    }

    pub async fn delete_exchange(&self, exchange_id: ExchangeId) -> Result<ExchangeId> {
        let ctx_to_delete = self.context_by_id(&exchange_id).await
            .ok_or_else(|| anyhow!("exchange not found"))?;
        ctx_to_delete.shutdown().await?;
        self.node.docs.drop_doc(exchange_id.into()).await?;
        self.delete_exchange_id(exchange_id).await?;
        self.reload_contexts(None, Some(exchange_id)).await?;

        Ok(exchange_id)
    }

    pub async fn sync_all(&self) -> Result<()> {
        let ctxs = self.contexts().await;
        let futures: Vec<_> = ctxs.iter().map(|ex| {
            ex.sync()
        }).collect();
        futures::future::join_all(futures).await;
        println!("did resync on all loaded exchanges");
        Ok(())
    }
    
    pub async fn shutdown(&self) -> Result<()> {
        let ctxs = self.contexts().await;
        let futures: Vec<_> = ctxs.iter().map(|ex| ex.shutdown()).collect();
        futures::future::join_all(futures).await;
        println!("shut down all the contexts safely!");
        Ok(())
    }

    pub async fn contexts(&self) -> Vec<ExchangeContext> {
        let read_lock = self.exchanges.read().await;
        // all my contexts are just are wrappers which are cheap clones
        let copy = read_lock.clone();
        copy
    }

    pub async fn context_by_id(&self, id: &ExchangeId) -> Option<ExchangeContext>{
        let read_lock = self.exchanges.read().await;
        (&*read_lock).into_iter().find_map(|e| {
            if &e.id == id {
                Some(e.clone())
            } else {
                None
            }
        })
    }
    async fn load_exchange_context(&self, exchange_id: ExchangeId) -> Result<ExchangeContext> {
        let doc = self.node.docs.open(exchange_id.into()).await?
            .ok_or_else(|| anyhow!("No exchange document to load"))?;

        let ex_ctx = ExchangeContext::new(exchange_id, doc, self.node.clone());
        ex_ctx.start();

        Ok(ex_ctx)
    }

    async fn watch_context_changes(&self, ectx: ExchangeContext) -> Result<()> {
        let self_clone = self.clone();
        Handle::current().spawn(async move {
            let mut subby = ectx.subscribe();
            while let Ok(e) = subby.recv().await {
                match e {
                    Loaded => {}
                    ContextEvents::Join(_) => {
                        // someone joined, that means the name might change, refresh the list
                        self_clone.broadcast(ExchangeListDidUpdate).await.expect("gdamnitshit");
                    }
                    ContextEvents::MessageReceived(_) => {}
                    ContextEvents::LiveConnectionsUpdated(count) => {
                        // if any little guys change, update whole exchange list
                        self_clone.broadcast(ExchangeListDidUpdate).await.expect("gdamnitshit");
                    }
                }
            }
        });
        Ok(())
    }

    pub async fn add_context(&self, new_context: ExchangeContext) -> Result<()>
    {
        self.save_exchange_id(new_context.id()).await?;
        self.reload_contexts(Some(new_context), None).await?;
        Ok(())
    }
    pub async fn reload_contexts(&self, new_context: Option<ExchangeContext>, del_context: Option<ExchangeId>) -> Result<()> {
        println!("reloading exchange contexts!");

        if let Some(new) = &new_context {
            self.watch_context_changes(new.clone()).await?;
        }

        // tokio::time::sleep(Duration::from_millis(1000)).await;
        // all that should exist
        let total_exchanges = self.list_exchange_ids().await;

        let read_guard = self.exchanges.read().await;
        // all that do exist
        let running_exchanges: HashSet<ExchangeId> = read_guard.iter()
            .map(|e| e.id() )
            .collect();

        let mut new_exchanges: Vec<ExchangeContext> = vec![];
        let maybe_id_of_new = (&new_context).as_ref().map(|e| e.id());
        let movable_ctx = Mutex::new(new_context);

        // loop through what SHOULD exist, and make (or adopt from new_context) what doesnt
        for ex_id in &total_exchanges {
            if running_exchanges.contains(ex_id) {
                continue;
            }
            // we have an exchange id that's not in the list.  was it the new one we just passed in?
            // or should we create it
            let e_ctx = match maybe_id_of_new {
                Some(ref id) if id == ex_id => {
                    // this should only ever execute once
                    let lock = movable_ctx.lock();
                    lock.unwrap().take().unwrap()
                },
                _ => {
                    let newly_made_ctx = self.load_exchange_context(ex_id.clone()).await?;
                    newly_made_ctx.sync().await?; // if im making it here, it might not be syncing...
                    self.watch_context_changes(newly_made_ctx.clone()).await?;
                    newly_made_ctx
                }
            };
            // todo fix this shit
            new_exchanges.push(e_ctx);
            // let e_ctx = Ok(e_ctx);
            // match e_ctx {
            //     Ok(e) => {
            //         new_exchanges.push(e);
            //     },
            //     Err(e) => {
            //         println!("Error loading exchange {}", e);
            //     }
            // };
        }

        // if im deleting anything, find its index
        let mut idx_to_delete: Option<usize> = None;
        if let Some(id) = del_context {
            for (i, item) in read_guard.iter().enumerate() {
                if item.id == id {
                    idx_to_delete = Some(i);
                    break;
                }
            }
        }

        drop(read_guard);

        let mut write_lock = self.exchanges.write().await;
        // add the new stuff (append goes to end so we shouldnt affect the deletion index... eep
        (*write_lock).append(&mut new_exchanges);

        // delete the trash
        if let Some(del_idx) = idx_to_delete {
            println!("delete ctx at index {del_idx}");
            (*write_lock).remove(del_idx);
        }
        self.broadcast(ExchangeListDidUpdate).await?;
        Ok(())
    }

    async fn save_exchange_id(&self, exchange_id: ExchangeId) -> Result<()> {
        let doc = self.settings.settings_doc().await;
        let myself = self.identity.assumed_identity_pk().await
            .ok_or_else(|| anyhow!("no assumed identity, cant save exchange"))?;
        let mut ids: Vec<ExchangeId> = self.list_exchange_ids().await;

        ids.push(exchange_id);

        save_on_doc_as_key(&self.node, doc, &myself, SETTINGS_EXCHANGES_LIST_KEY, ids).await?;

        Ok(())
    }

    async fn list_exchange_ids(&self) -> Vec<ExchangeId> {
        let doc = self.settings.settings_doc().await;
        let exs: Vec<ExchangeId> = load_from_doc_at_key(&self.node, doc, None, SETTINGS_EXCHANGES_LIST_KEY).await
            .ok().unwrap_or_else(|| vec![]);
        exs
    }

    async fn delete_exchange_id(&self, exchange_id: ExchangeId) -> Result<()> {
        let doc = self.settings.settings_doc().await;
        let myself = self.identity.assumed_identity_pk().await
            .ok_or_else(|| anyhow!("no assumed identity"))?;
        let ids: Vec<ExchangeId> = self.list_exchange_ids().await;

        let new_list: Vec<ExchangeId> = ids.into_iter().filter(|e| e != &exchange_id).collect();

        save_on_doc_as_key(&self.node, doc, &myself, SETTINGS_EXCHANGES_LIST_KEY, new_list).await?;

        Ok(())
    }
}