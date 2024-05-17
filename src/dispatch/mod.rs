

use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;

use anyhow::Result;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Mutex;

pub mod global;
pub mod establish_connection;
pub mod exchange;

// pub mod exchange;

pub enum EventWrapper<E> {
    Event(E),
    Error,
    Unregister
}

type EventSender<E> = Sender<EventWrapper<E>>;

pub struct AsyncMessageDispatch<E, A, C>
{
    responder_tx: Sender<EventWrapper<E>>,
    responder_rx: Arc<Mutex<Receiver<EventWrapper<E>>>>,
    actor_tx: Sender<A>,
    actor_rx: Arc<Mutex<Receiver<A>>>,
    context: C,
    handle: Handle,

}

impl <E, A, C> AsyncMessageDispatch<E, A, C>
where
    C: Send + Sync + Clone + 'static,
    A: Send + Sync + 'static + Debug,
    E: Send + Sync + 'static,
{

    pub fn new(context: C) -> Self
    where
    {

        let (responder_tx, responder_rx)= tokio::sync::mpsc::channel(8);
        let (actor_tx, actor_rx)= tokio::sync::mpsc::channel(8);
        let instance = Self {
            responder_tx,
            responder_rx: Arc::new(Mutex::new(responder_rx)),
            context,
            actor_tx,
            actor_rx: Arc::new(Mutex::new(actor_rx)),
            handle: Handle::try_current().expect("Dispatch can only be created within tokio context")
        };

        instance
    }
    pub fn register_action_handler<F, Fut>(&self, action_receiver: F)
    where
        F: (Fn(C, A, Sender<EventWrapper<E>>) -> Fut) + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let ctx = self.context.clone();
        let actor_rx = self.actor_rx.clone();
        let responder_tx = self.responder_tx.clone();

        self.handle.spawn(async move {

            let mut actor_rx = actor_rx.lock().await;

            while let Some(action) = actor_rx.recv().await {

                let res = action_receiver(ctx.clone(), action, responder_tx.clone()).await;

                if let Err(e) = res {
                    eprintln!("Error handling action {}", e);
                    responder_tx.send(EventWrapper::Error).await.expect("Error reporting error");
                }
            }
        });
    }

    pub fn emit_action(&self, action: A) {
        self.actor_tx.try_send(action).unwrap();
    }
    pub fn unregister_responder(&self) {
        self.responder_tx.clone().try_send(EventWrapper::Unregister).unwrap()
    }

    pub fn register_responder<Fr>(&self, responder: Fr)
    where
        Fr: Fn(E) + Send + Sync + 'static,
    {
        let mine = self.responder_rx.clone();
        self.handle.spawn(async move {
            let mut rx = mine.lock().await;
            while let Some(event) = rx.recv().await {
                match event {
                    EventWrapper::Unregister => {
                        // breaking out of this loop will allow this
                        // async closure to end, which will release the
                        // rx mutex and release the reference to the responder
                        break;
                    },
                    EventWrapper::Event(event) => {
                        responder(event);
                    },
                    EventWrapper::Error => {
                        println!("I need to tell the responder something fucked up")
                    }
                }
            }
        });
    }
}
