use std::collections::HashSet;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::sync::{broadcast, RwLock};
use tokio::sync::broadcast::error::SendError;
use crate::data::{ExchangeId, load_from_doc_at_key, Node, save_on_doc_as_key};
use crate::exchange::context::{ContextEvents, ExchangeContext};
use crate::settings::Service as SettingsService;
use crate::identity::Service as IdentityService;
use anyhow::{anyhow, Result};
use iroh::base::ticket::Ticket;
use iroh::docs::DocTicket;
use crate::exchange::{ID_PIC, IDENTIFICATION_KEY, SETTINGS_EXCHANGES_LIST_KEY};
use crate::exchange::service::Events::ListDidUpdate;

#[derive(Clone, Debug)]
pub enum Events {
    ListDidUpdate
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
            println!("ExchangeService tried to broadcast but no ones listening {:?}", event);
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
        // let manifest = MutualBinary; // only kind for now

        // save_on_doc_as_key(&self.node, &doc, pk, MANIFEST_KEY, manifest).await?;
        save_on_doc_as_key(&self.node, &doc, pk, IDENTIFICATION_KEY, &myself).await?;

        // set our pic too
        if let Some(pic) = self.identity.get_pic(myself.public_key()).await? {
            doc.set_hash(myself.public_key().into(), ID_PIC, pic.content_hash(), pic.content_len()).await?;
        }

        let ex_id: ExchangeId = doc.id().into();
        println!("created doc for exchange {}, {:?}", doc.id(), doc.status().await?);

        let ex = ExchangeContext::new(ex_id, doc, self.node.clone());
        ex.start().await?;

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

        // set our pic too
        if let Some(pic) = self.identity.get_pic(myself.public_key()).await? {
            doc.set_hash(myself.public_key().into(), ID_PIC, pic.content_hash(), pic.content_len()).await?;
        }

        let ex = ExchangeContext::new(ex_id, doc, self.node.clone());
        ex.start().await?;

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
            if &e.id() == id {
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
        ex_ctx.start().await?;

        Ok(ex_ctx)
    }

    async fn watch_context_changes(&self, ectx: ExchangeContext) -> Result<()> {
        let self_clone = self.clone();
        tokio::spawn(async move {
            let mut subby = ectx.subscribe();
            while let Ok(e) = subby.recv().await {
                match e {
                    ContextEvents::Loaded => {}
                    ContextEvents::Join(_) => {
                        // someone joined, that means the name might change, refresh the list
                        self_clone.broadcast(ListDidUpdate).await.expect("gdamnitshit");
                    }
                    ContextEvents::MessageReceived(_) => {}
                    ContextEvents::LiveConnectionsUpdated(..) => {
                        // if any little guys change, update whole exchange list
                        self_clone.broadcast(ListDidUpdate).await.expect("gdamnitshit");
                    },
                    ContextEvents::FileUpdated(file) => {
                        if file.name.eq(ID_PIC) {
                            self_clone.broadcast(ListDidUpdate).await.expect("gdamnitshit");
                        }
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

    pub async fn reload_pic_for_all(&self) -> Result<()> {
        let myself = self.identity.assumed_identity_pk().await.unwrap();
        let pic = self.identity.get_pic(&myself).await?;
        if let Some(pic) = pic {
            let read_guard = self.exchanges.read().await;
            for x in &*read_guard {
                x.add_file(myself.clone(), "id_pic", &pic).await?;
            }
        }

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
                if item.id() == id {
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
        self.broadcast(ListDidUpdate).await?;
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