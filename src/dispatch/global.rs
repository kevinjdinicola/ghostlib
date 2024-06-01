use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::mpsc::Sender;
use tracing::info;
use tracing::log::debug;
use crate::data::{BlobHash, ExchangeId, Node, WideId};
use crate::dispatch::{AsyncMessageDispatch, EventSender, EventWrapper};
use crate::dispatch::EventWrapper::Event;
use crate::dispatch::global::GlobalEvents::{ExchangeCreated, ExchangeListChanged, IdentityNeeded, IdentityPicUpdate, IdentitySelected};
use crate::exchange::{ExchangeService, ExchangeServiceEvent, ID_PIC};
use crate::identity::{Identification, ParticipantList, Service as IdentityService};
use crate::settings::Service as SettingsService;

pub struct Context {
    pub settings: SettingsService,
    pub identity: IdentityService,
    pub exchange: ExchangeService,
    pub node: Node,

}

#[derive(uniffi::Object)]
pub struct GlobalAppDispatcher {
    dispatch: AsyncMessageDispatch<GlobalEvents, GlobalActions, Arc<Context>>,
}

// #[derive(Debug)]
#[derive(uniffi::Enum)]
pub enum GlobalEvents {
    IdentityNeeded,
    IdentitySelected(Identification),
    ExchangeListChanged(Vec<ExchangeListItem>),
    ExchangeCreated(WideId),
    IdentityPicUpdate(BlobHash)
}

#[derive(Debug)]
#[derive(uniffi::Enum)]
pub enum GlobalActions {
    Start,
    CreateIdentity(String),
    CreateExchange,
    DeleteExchange(ExchangeId),
    WakeFromSleep,
    SetIdentityPic(Vec<u8>)
}

#[uniffi::export(with_foreign)]
pub trait GlobalDispatchResponder: Send + Sync{
    fn event(&self, state: GlobalEvents);
}


#[derive(uniffi::Record)]
pub struct ExchangeListItem {
    id: WideId,
    label: String,
    pic: Option<BlobHash>,
    connections: u32
}



async fn create_exchange(ctx: Arc<Context>, tx: &Sender<EventWrapper<GlobalEvents>>) -> anyhow::Result<()> {
    let e=  ctx.exchange.create_exchange(true).await?;
    info!("created exchange {}",e.id());
    // might not need to do this if it happens automatically by listening
    tx.send(Event(ExchangeCreated(e.id()))).await?;
    // exchange_list_changed(ctx, tx).await?;
    Ok(())
}

// async fn join_exchange(token: String, chat: ChatService, tx: &Sender<EventWrapper<GlobalEvents>>) -> anyhow::Result<()> {
//     let ectx = chat.join_exchange(&token).await?;
//     // might not need to do this if it happens automatically by listening
//     reload_exchanges(chat, tx).await?;
//     Ok(())
// }
//
//
// async fn create_identification(name: String, chat: ChatService, tx: &Sender<EventWrapper<GlobalEvents>>) -> anyhow::Result<()> {
//     chat.create_identification(&name).await?;
//     // might not need to do this if it happens automatically by listening
//     reload_identities(chat, tx).await?;
//     Ok(())
// }
//
async fn start(ctx: Arc<Context>, tx: &Sender<EventWrapper<GlobalEvents>>) -> anyhow::Result<()> {
    // good place for initialization logic
    ctx.identity.load_assumed_identity().await?;


    let iden = ctx.identity.assumed_identity().await;

    match iden {
        None => {
            tx.send(Event(IdentityNeeded)).await?;
        }
        Some(iden) => {
            ctx.exchange.reload_contexts(None, None).await?;
            // exchange_list_changed(ctx, tx).await?;
            if let Some(iden_pic) = ctx.identity.get_pic(iden.public_key()).await? {
                tx.send(Event(IdentityPicUpdate(iden_pic.content_hash().into()))).await?;
            }

            tx.send(Event(IdentitySelected(iden))).await?;
        }
    }
    Ok(())
}

async fn exchange_list_changed(ctx: &Arc<Context>, tx: &Sender<EventWrapper<GlobalEvents>>) -> anyhow::Result<()> {
    let me = ctx.identity.assumed_identity().await.unwrap();

    let mut disps: Vec<ExchangeListItem> = vec![];
    let ctxs = ctx.exchange.contexts().await;
    debug!("updating exchanges in ui, found {} contexts", ctxs.len());

    for ex in ctxs {
        let maybe_other = ex.participants().await.get_other(me.public_key());

        let (label, pic_hash): (String, Option<BlobHash>) = if let Some(other) = maybe_other {
            let pic = ex.get_file(other.public_key(), ID_PIC).await?;
            (other.name, pic)
        } else {
            let id_prefix = ex.id().to_string();
            let id_prefix = &id_prefix.as_str()[..6];
            (format!("loading... ({id_prefix}"), None)
        };
        let cnx = ex.live_connection_count().await?;
        disps.push(ExchangeListItem { id: ex.id(), label, pic: pic_hash, connections: cnx as u32 });
    }

    tx.send(Event(ExchangeListChanged(disps))).await?;

    Ok(())
}

async fn create_identity(name: String, ctx: Arc<Context>, tx: &Sender<EventWrapper<GlobalEvents>>) -> anyhow::Result<()> {
    // this automatically assumes
    ctx.identity.create_identification(&name).await?;
    // recalling start again should be fine
    start(ctx, tx).await
}

async fn delete_exchange(ex_id: ExchangeId, ctx: Arc<Context>, _tx: &Sender<EventWrapper<GlobalEvents>>) -> anyhow::Result<()> {
    ctx.exchange.delete_exchange(ex_id).await?;

    Ok(())
}
 fn spawn_exchange_service_watcher(ctx: Arc<Context>, tx: Sender<EventWrapper<GlobalEvents>>){
    Handle::current().spawn(async move {
        let mut subby = ctx.exchange.subscribe();
        while let Ok(e) = subby.recv().await {
            match e {
                ExchangeServiceEvent::ListDidUpdate => {
                    exchange_list_changed(&ctx, &tx).await.expect("couldnt send");
                }
            }
        }
    });
}

async fn action(ctx: Arc<Context>, action: GlobalActions, tx: EventSender<GlobalEvents>) -> anyhow::Result<()> {
    match action {
        GlobalActions::Start => {
            // importantly, this isnt being done in the start function because that gets called twice
            // maybe thatn eeds to be refactored
            spawn_exchange_service_watcher(ctx.clone(), tx.clone());
            start(ctx, &tx).await?
        },
        GlobalActions::CreateIdentity(name) => {
            create_identity(name, ctx, &tx).await?
        }
        GlobalActions::CreateExchange => {
            create_exchange(ctx, &tx).await?
        }
        GlobalActions::DeleteExchange(ex_id) => {
            delete_exchange(ex_id, ctx, &tx).await?
        }
        GlobalActions::WakeFromSleep => {
            info!("app woke from sleep");
            ctx.exchange.sync_all().await?;
        }
        GlobalActions::SetIdentityPic(path) => {
            let myself = ctx.identity.assumed_identity().await.unwrap();
            let hash = ctx.identity.set_pic(myself.public_key(), path).await?;
            debug!("identity pic set! hash {}", hash);

            // lol does this work
            ctx.exchange.reload_pic_for_all().await?;

            tx.send(Event(IdentityPicUpdate(hash))).await?;
        }
    };
    Ok(())
}

#[uniffi::export]
impl GlobalAppDispatcher {

    pub fn emit_action(&self, action: GlobalActions) {
        self.dispatch.emit_action(action);
    }

    pub fn unregister_responder(&self) {
        self.dispatch.responder_tx.try_send(EventWrapper::Unregister).expect("failed to unregister global dispatch")
    }
    pub fn register_responder(&self, responder: Arc<dyn GlobalDispatchResponder>) {
        self.dispatch.register_responder(move |e| {
            responder.event(e);
        });
        debug!("Automatically emitting start upon register to global dispatch");
        self.emit_action(GlobalActions::Start);
    }
}

impl GlobalAppDispatcher {
    pub fn new(context: Context) -> GlobalAppDispatcher {

        let message_adapter = AsyncMessageDispatch::new(Arc::new(context));
        message_adapter.register_action_handler(action);

        // context.exchange. // need to subscribe globally here

        debug!("Global app dispatch created");
        GlobalAppDispatcher {
            dispatch: message_adapter,
        }
    }

}
