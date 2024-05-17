use std::sync::Arc;
use fallible_iterator::FallibleIterator;

use tokio::runtime::Handle;
use tokio::sync::mpsc::Sender;
use crate::data::{ExchangeId, WideId};


use crate::dispatch::{AsyncMessageDispatch, EventSender, EventWrapper};
use crate::dispatch::EventWrapper::Event;
use crate::dispatch::global::GlobalEvents::{ExchangeCreated, ExchangeListChanged, IdentityNeeded, IdentitySelected};
use crate::identity::{Identification, Service as IdentityService};
use crate::settings::Service as SettingsService;
use crate::exchange::{Events, Service as ExchangeService};

pub struct Context {
    pub settings: SettingsService,
    pub identity: IdentityService,
    pub exchange: ExchangeService,

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
    ExchangeCreated(WideId)
}

#[derive(Debug)]
#[derive(uniffi::Enum)]
pub enum GlobalActions {
    Start,
    CreateIdentity(String),
    CreateExchange,
    DeleteExchange(ExchangeId),
    WakeFromSleep
}

#[uniffi::export(with_foreign)]
pub trait GlobalDispatchResponder: Send + Sync{
    fn event(&self, state: GlobalEvents);
}


#[derive(uniffi::Record)]
pub struct ExchangeListItem {
    id: WideId,
    label: String,
    connections: u32
}



async fn create_exchange(ctx: Arc<Context>, tx: &Sender<EventWrapper<GlobalEvents>>) -> anyhow::Result<()> {
    let e=  ctx.exchange.create_exchange(true).await?;
    println!("created exchange {}",e.id());
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
            tx.send(Event(IdentitySelected(iden))).await?;
        }
    }
    Ok(())
}

async fn exchange_list_changed(ctx: &Arc<Context>, tx: &Sender<EventWrapper<GlobalEvents>>) -> anyhow::Result<()> {
    let me = ctx.identity.assumed_identity().await.unwrap();

    let mut disps: Vec<ExchangeListItem> = vec![];
    let ctxs = ctx.exchange.contexts().await;
    println!("updating exchanges in ui, found {} contexts", ctxs.len());

    for ex in ctxs {
        let parts = ex.participants().await;
        let connection_count = ex.live_connection_count().await?;
        let idens = parts.iter().as_slice();
        let label: String = if idens[0].public_key != me.public_key {
            idens[0].name.as_str().into()
        } else if idens.len()  == 2 {
            idens[1].name.as_str().into()
        } else {
            let id_prefix = ex.id().to_string();
            let id_prefix = &id_prefix.as_str()[..6];
            format!("loading... ({id_prefix}")
        };
        disps.push(ExchangeListItem { id: ex.id(), label, connections: connection_count as u32 });
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

async fn delete_exchange(ex_id: ExchangeId, ctx: Arc<Context>, tx: &Sender<EventWrapper<GlobalEvents>>) -> anyhow::Result<()> {
    ctx.exchange.delete_exchange(ex_id).await?;

    Ok(())
}
 fn spawn_exchange_service_watcher(ctx: Arc<Context>, tx: Sender<EventWrapper<GlobalEvents>>){
    Handle::current().spawn(async move {
        let mut subby = ctx.exchange.subscribe();
        while let Ok(e) = subby.recv().await {
            match e {
                Events::ExchangeListDidUpdate => {
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
            println!("app woke from sleep");
            ctx.exchange.sync_all().await?;
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
        println!("Automatically emitting start upon register to global dispatch");
        self.emit_action(GlobalActions::Start);
    }
}

impl GlobalAppDispatcher {
    pub fn new(context: Context) -> GlobalAppDispatcher {

        let message_adapter = AsyncMessageDispatch::new(Arc::new(context));
        message_adapter.register_action_handler(action);

        // context.exchange. // need to subscribe globally here

        println!("Global app dispatch created");
        GlobalAppDispatcher {
            dispatch: message_adapter,
        }
    }

}
