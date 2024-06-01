use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Mutex;
use crate::data::{BlobHash, Doc, Node, PublicKey, save_on_doc_as_key};
use crate::exchange::ExchangeService;
use crate::identity::Identification;
use crate::settings::Service as SettingsService;
use crate::identity::Service as IdentityService;
use anyhow::Result;
use futures_util::StreamExt;
use iroh::base::node_addr::AddrInfoOptions;
use iroh::base::ticket::Ticket;
use iroh::client::docs::{Entry, LiveEvent, ShareMode};

use tokio::runtime::Handle;
use tokio::select;
use tracing::{debug, error, info};
use tracing::field::debug;
use crate::dispatch::establish::Event::{ConnectionConfirmed, ConnectionEstablished, ConnectionFailed, IdentityLoaded, PicLoaded, Reset};
use crate::dispatch::establish::ExchangeLink::{Creator, Joiner};
use crate::exchange::context::{ContextEvents, ExchangeContext};
use crate::live_doc::ContentReadyProcessor;
use crate::live_doc::IdentificationReader;

#[derive(Clone)]
pub struct Context {
    pub settings: SettingsService,
    pub identity: IdentityService,
    pub exchange: ExchangeService,
    pub node: Node,
}

#[derive(uniffi::Enum, Debug)]
pub enum Event {
    Reset,
    JoinTicketGenerated(String),
    AttemptingJoin,
    IdentityLoaded(Identification),
    PicLoaded(BlobHash),
    ConnectionEstablished, // this means we found the doc write ticket
    ConnectionConfirmed,
    ConnectionFailed(String),
    ConnectionRejected
}
#[derive(Debug)]
#[derive(uniffi::Enum)]
pub enum Action {
    GenerateJoinTicket,
    ReceivedJoinTicket(String),
    AcceptConnection,
    RejectConnection
}

#[uniffi::export(with_foreign)]
pub trait EstablishConnectionDispatchResponder: Send + Sync{
    fn event(&self, state: Event);
}

#[derive(uniffi::Object)]
pub struct EstablishConnectionDispatcher {
    handle: Handle,
    context: Context,
    action_tx: Sender<Action>,
    action_rx: Arc<Mutex<Option<Receiver<Action>>>>
}

struct EstablishConnectionActor {
    context: Context,
    action_rx: Arc<Mutex<Option<Receiver<Action>>>>,
    responder: Arc<dyn EstablishConnectionDispatchResponder>,
    // could probably boil this into a state machine
    doc: Option<Doc>,
    other: Option<Identification>,
    exchange: Option<ExchangeLink>,
}

enum ExchangeLink {
    Creator(ExchangeContext),
    Joiner(String)
}

pub const IDENTIFICATION: &str = "id";
pub const PIC: &str = "id_pic";
pub const EXCHANGE_TICKET: &str = "exchange_ticket";

impl EstablishConnectionActor {
    async fn worker_loop(mut self) -> Result<()> {
        let (le_tx, mut le_rx) = tokio::sync::mpsc::channel(16);
        let mut guard = self.action_rx.lock().await;
        let mut action_rx = guard.take().unwrap();
        drop(guard);

        let mut crp = ContentReadyProcessor::new();
        let myself = self.context.identity.assumed_identity_pk().await.expect("no assumed identity");

        self.responder.event(Reset);
        let mut found_neighbor = false;

        loop {
            // this will quit when action_tx and le_tx end.
            // action_tx will end when the app drops the dispatcher (closing the view)
            // le_rx will end when the doc subscription closes
            select! {
                Some(a) = action_rx.recv() => {
                    match a {
                        Action::GenerateJoinTicket => {
                            if self.doc.is_none() {
                               debug!("Generating join ticket.. creating doc first");
                                let doc = self.context.node.docs.create().await?;
                                debug!("doc created {}, making ticket", doc.id());

                                self.doc = Some(doc);
                                self.identify_self_on_doc(&myself, false).await.unwrap();
                                debug!("beginning {}", le_tx.is_closed());
                                self.subscribe_doc(le_tx.clone()).await.unwrap();
                            }
                            let ticket = self.doc.as_ref().unwrap().share(ShareMode::Write,AddrInfoOptions::RelayAndAddresses).await?;

                            let tkt = Ticket::serialize(&ticket);
                            debug!("ticket generated, sending event back.. {}", tkt);
                            self.responder.event(Event::JoinTicketGenerated(tkt));
                        }

                        Action::ReceivedJoinTicket(ticket) => {
                            if self.doc.is_some() {
                                break;
                            }
                            debug!("received a join ticket! joining doc {}", ticket);
                            let doc = self.context.node.docs.import(Ticket::deserialize(&ticket)?).await?;
                            self.doc = Some(doc);
                            self.identify_self_on_doc(&myself, true).await.unwrap();
                            debug!("identified myself, now subscribing");
                            self.subscribe_doc(le_tx.clone()).await.unwrap();
                        }

                        Action::AcceptConnection => {
                            debug!("accepting connection");
                            self.accept_connection(&myself).await?;
                            self.drop_doc().await?;
                            self.responder.event(Event::Reset);
                        }

                        Action::RejectConnection => {
                            debug!("rejecting connection");
                            self.drop_doc().await?;
                            self.responder.event(Event::Reset);
                        }
                    }
                },
                Some(le) = le_rx.recv() => {
                    match le {
                        LiveEvent::NeighborUp(pk) => {
                            debug!("neighbor up {}", pk);
                            found_neighbor = true;
                        }
                        LiveEvent::NeighborDown(_) => {}
                        LiveEvent::SyncFinished(_) => {
                            debug!("sync finished");
                            if !found_neighbor {
                                self.responder.event(ConnectionFailed(String::from("Timed out waiting for connection")));
                            }
                        }
                        _ => {
                            if let Some(entry) = crp.process_event(le)? {
                                self.handle_doc_entry(entry, myself).await?;
                            }
                        }
                    }
                },
                else => {
                    debug!("both branches done, closing worker loop")
                },
            }
        };
        Ok(())
    }

    async fn drop_doc(&mut self) -> Result<()> {
        let doc = self.doc.take().unwrap();
        doc.leave().await?;
        self.context.node.docs.drop_doc(doc.id()).await?;
        Ok(())
    }

    async fn identify_self_on_doc(&mut self, pk: &PublicKey, create_exchange: bool) -> Result<()> {
        debug!("adding my details to the doc");
        if let Some(doc) = self.doc.as_ref() {
            debug!("doc exists, saving identification");
            let myself = self.context.identity.get_identification(pk).await?;
            save_on_doc_as_key(&self.context.node, doc, pk, IDENTIFICATION, &myself).await?;

            if let Some(pic) = self.context.identity.get_pic(pk).await? {
                debug("saving my pic");
                doc.set_hash(pk.into(), PIC, pic.content_hash(), pic.content_len()).await?;
            }
            if create_exchange {
                let exctx = self.context.exchange.create_exchange(false).await?;
                let tkt = exctx.generate_join_ticket().await?;
                debug!("saving exchange ticket {}", tkt);
                doc.set_bytes(pk.into(), EXCHANGE_TICKET, tkt).await?;
                self.exchange = Some(Creator(exctx));
            }
        } else {
            error!("Can't identify self on doc, no doc exists");
        }
        Ok(())
    }

    async fn accept_connection(&mut self, myself: &PublicKey) -> Result<()> {
        if let Some(ex) = &self.exchange {
            let exctx = match ex {
                Creator(exctx) => { exctx.clone() }
                Joiner(ticket) => {
                    debug!("I only have the ticket.. joining the echange {}", ticket);
                    self.context.exchange.join_exchange(&ticket, false).await?
                }
            };
            // wait until the exchange has both ppl here..
            let mut subscriber = exctx.subscribe();
            let mut other_part: Option<PublicKey> = None;
            debug!("subscribing, waiting for the other person to show up");
            while let Ok(ce) = subscriber.recv().await {
                match ce {
                    ContextEvents::Join(part) => {
                        if &part != myself {
                            info!("Met participant on exchange, id {}", part);
                            other_part = Some(part);
                            break;
                        }
                    },
                    ContextEvents::SyncFinished => {
                        break;
                    }
                    _ => {}
                }
            }
            drop(subscriber);


            if other_part.is_some() {
                // we're done here!
                debug!("they showed up! it worked");
                self.context.exchange.add_context(exctx).await?;
                self.responder.event(ConnectionConfirmed);
            } else {
                self.responder.event(ConnectionFailed(String::from("Other participant failed to join")));
            }

        }
        Ok(())
    }

    async fn handle_doc_entry(&mut self, entry: Entry, myself: PublicKey) -> Result<()> {
        if entry.author() == myself.into() {
            return Ok(())
        }
        let key = String::from_utf8_lossy(entry.key());

        if key.eq(IDENTIFICATION)  {
            // not me joined!
            let id = self.context.node.read(entry.content_hash()).await?;
            self.responder.event(IdentityLoaded(id.clone()));
            self.other = Some(id);

        } else if key.eq(PIC) {
            self.responder.event(PicLoaded(entry.content_hash().into()))

        } else if key.eq(EXCHANGE_TICKET) {
            let ticket = self.context.node.blobs.read_to_bytes(entry.content_hash()).await?;
            let ticket = String::from_utf8_lossy(ticket.as_ref());
            self.exchange = Some(Joiner(ticket.to_string()));
        }

        // check after receving stuff.. do i have everything I need to proceed?
        if self.other.is_some() && self.exchange.is_some() {
            debug!("I have found the others details and have exchange access, we good");
            // i found the other person and know how to talk to them, cool!
            self.responder.event(ConnectionEstablished);
        }

        Ok(())
    }

    async fn subscribe_doc(&mut self, le_tx: Sender<LiveEvent>) -> Result<()> {
        debug!("trying to subscrbe, its this state {}", le_tx.is_closed());
        if let Some(doc) = &self.doc {
            let mut stream = doc.subscribe().await.unwrap();
            debug!("did a subscribe");
            tokio::spawn(async move {
                debug!("about to call next, its this state os {}", le_tx.is_closed());
                while let Some(Ok(le)) = stream.next().await {
                    debug!("AND FUCKING HERE?? {}", le_tx.is_closed());
                    if let Err(e) = le_tx.send(le).await {
                        error!("Failed to send doc message back to worker {}", e.to_string())
                    } else {
                        debug!("successfully sent doc message back to worker, le_tx is open")
                    }
                }
                debug!("Doc subscriber ended")
            });
        }
        Ok(())
    }

}



impl EstablishConnectionDispatcher {
    pub fn new(context: Context) -> Self {
        let (action_tx, action_rx) = tokio::sync::mpsc::channel(16);
        EstablishConnectionDispatcher {
            context,
            action_tx,
            action_rx: Arc::new(Mutex::new(Some(action_rx))),
            handle: Handle::current()
        }
    }


}

#[uniffi::export]
impl EstablishConnectionDispatcher {
    pub fn emit_action(&self, action: Action) {
        if let Err(err) = self.action_tx.try_send(action) {
            error!("{}", err.to_string());
        }
    }
    pub fn start(&self, responder: Arc<dyn EstablishConnectionDispatchResponder>) {
        let actor = EstablishConnectionActor {
            context: self.context.clone(),
            action_rx: self.action_rx.clone(),
            responder,
            doc: None,
            other: None,
            exchange: None,
        };
        self.handle.spawn(async move {
            match actor.worker_loop().await {
                Ok(_) => {
                    info!("Worker exited gracefully");
                }
                Err(err) => {
                    error!("Worker crashed! {}", err.to_string());
                }
            };
        });
    }

}

impl Drop for EstablishConnectionDispatcher {
    fn drop(&mut self) {
        info!("dropped")
    }
}