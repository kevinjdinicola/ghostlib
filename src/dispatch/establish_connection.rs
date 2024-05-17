use std::ptr::write;
use std::sync::Arc;
use fallible_iterator::FallibleIterator;
use tokio::runtime::Handle;
use tokio::sync::RwLock;
use crate::data::WideId;
use crate::dispatch::{AsyncMessageDispatch, EventSender, EventWrapper};
use crate::dispatch::establish_connection::EstablishConnectionEvents::{AttemptingJoin, ConnectionConfirmed, ConnectionEstablished, ConnectionRejected, JoinTicketGenerated};
use crate::dispatch::EventWrapper::Event;
use crate::identity::{Identification, ParticipantList, Service as IdentityService};
use crate::settings::Service as SettingsService;
use crate::exchange::{ContextEvents, ExchangeContext, Service as ExchangeService};
#[derive(uniffi::Object)]
pub struct EstablishConnectionDispatcher {
    dispatch: AsyncMessageDispatch<EstablishConnectionEvents, EstablishConnectionActions, Arc<Context>>,
}
pub struct Context {
    pub settings: SettingsService,
    pub identity: IdentityService,
    pub exchange: ExchangeService,
    pub ex_ctx: RwLock<Option<ExchangeContext>>

}

// #[derive(Debug)]
#[derive(uniffi::Enum)]
pub enum EstablishConnectionEvents {
    JoinTicketGenerated(String),
    AttemptingJoin,
    ConnectionEstablished(Identification),
    ConnectionConfirmed,
    ConnectionRejected
}

#[derive(Debug)]
#[derive(uniffi::Enum)]
pub enum EstablishConnectionActions {
    Start,
    GenerateJoinTicket,
    ReceivedJoinTicket(String),
    AcceptConnection,
    RejectConnection
}

#[uniffi::export(with_foreign)]
pub trait EstablishConnectionDispatchResponder: Send + Sync{
    fn event(&self, state: EstablishConnectionEvents);
}

fn wait_for_other_participants(exctx: ExchangeContext, ctx: Arc<Context>, tx: EventSender<EstablishConnectionEvents>) {
    Handle::current().spawn(async move {
        let myself = ctx.identity.assumed_identity().await.unwrap();
        let mut subby = exctx.subscribe();
        while let Ok(e) = subby.recv().await {
            match e {
                ContextEvents::Join(i) => {
                    if i != myself.public_key {
                        let other = exctx.participants().await.get_other(myself.public_key()).unwrap();
                        tx.send(Event(ConnectionEstablished(other))).await.expect("couldnt send msg!");
                        break;
                    }
                },
                _ => { }
            }
        }
    });
}

async fn action(ctx: Arc<Context>, action: EstablishConnectionActions, tx: EventSender<EstablishConnectionEvents>) -> anyhow::Result<()> {
    match action {
        EstablishConnectionActions::Start => {
            println!("started establish connection context")
        }
        EstablishConnectionActions::GenerateJoinTicket => {

            //sender side only
            let exctx = ctx.exchange.create_exchange(false).await?;

            let join_ticket = exctx.generate_join_ticket().await?;

            let mut write_guard = ctx.ex_ctx.write().await;
            (*write_guard) = Some(exctx.clone());
            drop(write_guard);

            wait_for_other_participants(exctx, ctx, tx.clone());


            tx.send(Event(JoinTicketGenerated(join_ticket))).await?;
        }
        EstablishConnectionActions::ReceivedJoinTicket(join_ticket) => {
            let myself = ctx.identity.assumed_identity().await.unwrap();
            // receiver side only
            let exctx = ctx.exchange.join_exchange(join_ticket.as_str(), false).await?;


            let mut write_guard = ctx.ex_ctx.write().await;
            (*write_guard) = Some(exctx.clone());
            drop(write_guard);

            // anyone else there?
            let other_person = exctx.participants().await.get_other(&myself.public_key);
            match other_person {
                Some(iden) => {
                    tx.send(Event(ConnectionEstablished(iden))).await?;
                },
                None => {
                    tx.send(Event(AttemptingJoin)).await?;
                    wait_for_other_participants(exctx, ctx, tx.clone());
                }
            };
        }
        EstablishConnectionActions::AcceptConnection => {
            println!("some kind of accept connection logic? it's already added into the exchange service");
            let mut write_guard = ctx.ex_ctx.write().await;
            let exctx = (*write_guard).take().unwrap();
            ctx.exchange.add_context(exctx).await?;

            tx.send(Event(ConnectionConfirmed)).await?;
        }
        EstablishConnectionActions::RejectConnection => {
            let mut write_guard = ctx.ex_ctx.write().await;
            let exctx = (*write_guard).take().unwrap();
            ctx.exchange.shutdown().await?;
            tx.send(Event(ConnectionRejected)).await?;

        }
    };
    Ok(())
}



#[uniffi::export]
impl EstablishConnectionDispatcher {

    pub fn emit_action(&self, action: EstablishConnectionActions) {
        self.dispatch.emit_action(action);
    }

    pub fn unregister_responder(&self) {
        self.dispatch.responder_tx.try_send(EventWrapper::Unregister).expect("failed to unregister global dispatch")
    }
    pub fn register_responder(&self, responder: Arc<dyn EstablishConnectionDispatchResponder>) {
        self.dispatch.register_responder(move |e| {
            responder.event(e);
        });
        println!("Automatically emitting start upon register to global dispatch");
        self.emit_action(EstablishConnectionActions::Start);
    }
}

impl EstablishConnectionDispatcher {
    pub fn new(context: Context) -> EstablishConnectionDispatcher {
        let message_adapter = AsyncMessageDispatch::new(Arc::new(context));
        message_adapter.register_action_handler(action);

        println!("Establish connection dispatch created");
        EstablishConnectionDispatcher {
            dispatch: message_adapter,
        }
    }

}

impl Drop for EstablishConnectionDispatcher {
    fn drop(&mut self) {
        println!("EstablishConnectionDispatcher dropped!!!!!!");
    }
}