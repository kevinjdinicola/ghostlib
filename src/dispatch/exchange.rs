use std::sync::Arc;

use anyhow::Result;
use tokio::runtime::Handle;

use crate::dispatch::{AsyncMessageDispatch, EventSender, EventWrapper};
use crate::dispatch::EventWrapper::Event;
use crate::dispatch::exchange::ExchangeEvents::{NameChanged, OthersPicUpdated};
use crate::exchange::context::{ContextEvents, ExchangeContext};
use crate::exchange::{ID_PIC, Message};
use crate::identity::{ParticipantList, Service as IdentityService};
use crate::live_doc::File;

#[derive(uniffi::Object)]
pub struct ExchangeDispatcher {
    message_adapter: AsyncMessageDispatch<ExchangeEvents, ExchangeActions, Arc<Context>>
}
pub struct Context {
    pub identity: IdentityService,
    pub ectx: ExchangeContext,

}

#[derive(Debug)]
#[derive(uniffi::Enum)]
pub enum ExchangeEvents {
    NameChanged(String),
    OthersPicUpdated(File),
    MessagesReloaded{ messages: Vec<DisplayMessage>},
    MessageReceived { message: DisplayMessage },
    LiveConnectionsUpdated { count: u32 }
}

#[derive(Debug)]
#[derive(uniffi::Enum)]
pub enum ExchangeActions {
    Start,
    SendMessage { text: String },
}

#[derive(Debug)]
#[derive(uniffi::Record)]
pub struct DisplayMessage {
    pub id: u32,
    pub text: String,
    pub is_self: bool
}

impl Drop for ExchangeDispatcher {
    fn drop(&mut self) {
        println!("Exchange controller dropped")
    }
}

#[uniffi::export(with_foreign)]
pub trait ExchangeDispatchResponder: Send + Sync {
    fn event(&self, state: ExchangeEvents);
}

async fn reload_messages(ctx: &Arc<Context>, tx: &EventSender<ExchangeEvents>) -> Result<()> {
    let myself = ctx.identity.assumed_identity().await.unwrap();

    let msgs: Vec<DisplayMessage> =  ctx.ectx.messages().await.into_iter().enumerate().map(|(idx, msg)| {
        DisplayMessage {
            id: idx as u32,
            text: msg.text,
            is_self: myself.public_key == msg.pk
        }
    }).collect();

    tx.send(Event(ExchangeEvents::MessagesReloaded { messages: msgs })).await?;
    Ok(())
}

// async fn update_one_message(message: Message, ctx: Arc<Context>, tx: EventSender<ExchangeEvents>) -> Result<()> {
//     let myself = ctx.identity.assumed_identity().await.unwrap();
//
//     let msgs: Vec<DisplayMessage> =  ctx.ectx.messages().await.into_iter().enumerate().map(|(idx, msg)| {
//         DisplayMessage {
//             id: idx as u32,
//             text: msg.text,
//             is_self: myself.public_key == msg.pk
//         }
//     }).collect();
//
//     tx.send(EventWrapper::Event(ExchangeEvents::MessagesReloaded { messages: msgs })).await?;
//     Ok(())
// }

async fn send_message(text: String, ctx: Arc<Context>, _tx: EventSender<ExchangeEvents>) -> Result<()> {
    let myself = ctx.identity.assumed_identity().await.unwrap();
    let msg = Message::new(myself.public_key(), text.as_str());
    ctx.ectx.send_message(&msg).await?;

    Ok(())
}

async fn update_name(ctx: &Arc<Context>, tx: &EventSender<ExchangeEvents>) -> Result<()> {
    let myself = ctx.identity.assumed_identity().await.unwrap();
    let other = ctx.ectx.participants().await.get_other(myself.public_key()).map(|i|i.name).unwrap_or_else(|| "loading".into());
    tx.send(Event(NameChanged(other))).await?;
    Ok(())
}

async fn file_updated(file: File, ctx: &Arc<Context>, tx: &EventSender<ExchangeEvents>) -> Result<()> {
    // i only really care about id pics of the other person in this particular context
    let myself = ctx.identity.assumed_identity().await.unwrap();
    if file.owner != myself.public_key && file.name.eq(ID_PIC) {
        tx.send(Event(OthersPicUpdated(file))).await?;
    }
    Ok(())
}

fn watch_exchange_context(ctx: Arc<Context>, tx: EventSender<ExchangeEvents>) {
    Handle::current().spawn(async move {
        let mut subby = ctx.ectx.subscribe();
        while let Ok(e) = subby.recv().await {
            match e {
                ContextEvents::Loaded => {}
                ContextEvents::Join(_) => {}
                ContextEvents::MessageReceived(_) => {
                    reload_messages(&ctx, &tx).await.unwrap();
                }
                ContextEvents::LiveConnectionsUpdated(count) => {
                    tx.send(Event(ExchangeEvents::LiveConnectionsUpdated { count: count as u32 })).await.unwrap()
                }
                ContextEvents::FileUpdated(file) => {
                    file_updated(file, &ctx, &tx).await.unwrap();
                }
            }
        }
    });
}

async fn action(ctx: Arc<Context>, action: ExchangeActions, tx: EventSender<ExchangeEvents>) -> Result<()> {
    match action {
        ExchangeActions::SendMessage { text } => {
            send_message(text, ctx, tx).await?
        }
        ExchangeActions::Start => {
            // plz only do once...
            watch_exchange_context(ctx.clone(), tx.clone());
            update_name(&ctx, &tx).await?;
            reload_messages(&ctx, &tx).await?;
            // maybe should group together relatively non-changing pieces into a single struct
            let cnt = ctx.ectx.live_connection_count().await?;
            tx.send(Event(ExchangeEvents::LiveConnectionsUpdated { count: cnt as u32 })).await.unwrap()

        }
    }
    Ok(())
}

#[uniffi::export]
impl ExchangeDispatcher {
    pub fn emit_action(&self, action: ExchangeActions) {
        println!("emitting action {:?}", action);
        self.message_adapter.emit_action(action);
    }

    pub fn unregister_responder(&self) {
        self.message_adapter.responder_tx.try_send(EventWrapper::Unregister).expect("work bitch");
    }

    pub fn register_responder(&self, responder: Arc<dyn ExchangeDispatchResponder>) {
        self.message_adapter.register_responder(move |e| {
            println!("responding with event {:?}", e);
            responder.event(e);
        });
    }
}

impl ExchangeDispatcher {

    pub fn new(ctx: Context) -> ExchangeDispatcher {
        let message_adapter = AsyncMessageDispatch::new(Arc::new(ctx));
        message_adapter.register_action_handler(action);
        println!("Created an exchange controller");
        ExchangeDispatcher {
            message_adapter,
        }
    }


}