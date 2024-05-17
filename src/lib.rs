use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use fallible_iterator::FallibleIterator;

use tokio::runtime::{Handle, Runtime};
use tokio::sync::RwLock;

use crate::data::{ExchangeId, Node};
use crate::dispatch::establish_connection::EstablishConnectionDispatcher;
use crate::dispatch::exchange::ExchangeDispatcher;
use crate::dispatch::global::GlobalAppDispatcher;
use crate::exchange::Service as ExchangeService;
use crate::identity::Service as IdentityService;
use crate::settings::Service as SettingsService;

uniffi::setup_scaffolding!();

pub mod dispatch;
pub mod settings;
pub mod data;
pub mod identity;
pub mod exchange;

//
// some kind of root context to host the initialized rust app and
// interact with it


#[derive(uniffi::Record)]
pub struct AppConfig {
    pub data_path: String
}


#[derive(uniffi::Object)]
pub struct AppHost {
    // note: no functions exposed through FFI can be &mut
    // AppHost will always be accessed through an arc
    rt: Runtime,
    pub node: RwLock<Option<Node>>,
    pub settings: SettingsService,
    pub identity: IdentityService,
    pub exchange: ExchangeService,
    global_dispatch: Arc<GlobalAppDispatcher>,
    reset_flag: AtomicBool,
    config: AppConfig,
}

/*
    Non FFI exposed functions
 */
impl AppHost {

    pub fn handle(&self) -> Handle {
        self.rt.handle().clone()
    }

}



#[uniffi::export]
impl AppHost {

    #[uniffi::constructor]
    pub fn new(config: AppConfig) -> AppHost {
        let rt = Runtime::new().expect("Unable to start a tokio runtime");

        let _guard = rt.enter();
        let node = rt.block_on(async {
            Node::persistent(Path::new(&config.data_path.as_str())).await.unwrap().spawn().await.unwrap()
        });


        let settings = SettingsService::new(node.clone(), &config.data_path.as_str());
        let identity = IdentityService::new(node.clone(), settings.clone());
        let exchange = ExchangeService::new(node.clone(), settings.clone(), identity.clone());

        println!("Created ghostlib app host");

        let gd = GlobalAppDispatcher::new(dispatch::global::Context {
            settings: settings.clone(),
            identity: identity.clone(),
            exchange: exchange.clone()
        });

        AppHost {
            rt,
            node: RwLock::new(Some(node)),
            global_dispatch: Arc::new(gd),
            settings,
            identity,
            exchange,
            reset_flag: AtomicBool::new(false),
            config
        }
    }

    pub fn global_dispatch(&self) -> Arc<GlobalAppDispatcher> {
        self.global_dispatch.clone()
    }

    pub fn create_establish_connection_dispatch(&self) -> Arc<EstablishConnectionDispatcher> {
        let _guard = self.handle().enter();
        let dispatcher = EstablishConnectionDispatcher::new(dispatch::establish_connection::Context  {
            settings: self.settings.clone(),
            identity: self.identity.clone(),
            exchange: self.exchange.clone(),
            ex_ctx: RwLock::new(None),
        });
        Arc::new(dispatcher)
    }

    pub fn create_exchange_context_dispatch(&self, exchange_id: ExchangeId) -> Arc<ExchangeDispatcher> {
        let ectx = self.handle().block_on(async {
           self.exchange.context_by_id(&exchange_id).await.unwrap()
        });

        let _guard = self.handle().enter();
        let dispatcher = ExchangeDispatcher::new(dispatch::exchange::Context {
            identity: self.identity.clone(),
            ectx,
        });
        Arc::new(dispatcher)
    }

    pub fn set_reset_flag(&self) {
        self.reset_flag.store(true, Relaxed);
    }

    pub fn shutdown(&self) {
        // shutdown all the things
        // let node = std::mem::take(&mut self.node);
        let lock_ref = &self.node;

        self.rt.block_on(async move {
            println!("shutting down exchange service..");
            self.exchange.shutdown().await.expect("failed to shutdown contexts");

            println!("Waiting for node lock...");
            let mut lock = lock_ref.write().await;
            let owned_node = lock.take().expect("Failed to take node from option lock");

            println!("Shutting down node");
            // call on all other services to close their docs
            owned_node.shutdown().await.expect("Failed at shutting down iroh node... will crash");
            println!("Node shutdown complete");
        });

        if self.reset_flag.load(Relaxed) {
            println!("Deleting all data");
            fs::remove_dir_all(self.config.data_path.as_str()).unwrap();
        }

    }

}


#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;
    use fallible_iterator::FallibleIterator;

    use iroh::base::node_addr::AddrInfoOptions;
    use iroh::base::ticket::Ticket;
    use iroh::net::dns::node_info::NodeInfo;

    use iroh::rpc_protocol::ShareMode::Write;

    use crate::data::{ExchangeId, PublicKey, SerializingBlobsClient, WideId};
    use crate::exchange::{ContextEvents as ExchangeEvents, ContextEvents, Message};
    use crate::identity::Identification;

    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    const TEST_DIR: &str = "./testtmp";

    fn wipe_test_dir(dir: Option<&str>) {
        let dir: &str = dir.unwrap_or_else(|| TEST_DIR);
        if let Ok(md) = fs::metadata(dir) {
            if md.is_dir() {
                fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn will_delete_all_data() {
        wipe_test_dir(None);

        fs::create_dir(TEST_DIR).unwrap();
        let ah = AppHost::new(AppConfig { data_path: TEST_DIR.into() } );
        ah.set_reset_flag();
        ah.shutdown();

        assert!(matches!(fs::metadata(TEST_DIR), Err(_)))

    }


    use futures_util::StreamExt;
    use iroh::net::magic_endpoint::ConnectionInfo;
    use iroh::net::NodeAddr;
    use iroh::sync::PeerIdBytes;

    #[test]
    fn plz() {
        // wipe_test_dir(None);
        // fs::create_dir(TEST_DIR).unwrap();
        let ah = AppHost::new(AppConfig { data_path: TEST_DIR.into() } );
        let r: anyhow::Result<()> = ah.handle().block_on(async {
            ah.identity.load_assumed_identity().await?;
            ah.exchange.reload_contexts(None, None).await?; //autmate
            let ctxs = ah.exchange.contexts().await;
            // println!("i have {:?}", ctxs.len());
            //
            // tokio::time::sleep(Duration::from_millis(2000)).await;
            //
            // let iden = ah.identity.create_identification("TESTTTTT").await?;
            // let ticket = "docaaacaobf3xdntefygxnxix5kucvci6texjprv4qorwokv3stgvk2trhtagmhbffn4safwbqohcbjhy4k3wqbc632c6bzzq6dyg4hnatsspcdoaadaafaaam2yrlqacqaaib4ivyaf6lyho6ek4";
            // let ex = ah.exchange.join_exchange(ticket, true).await?;


            // let ctxs = ah.exchange.contexts().await;
            // println!("i have {:?}", ctxs.len());


            // let ex = ctxs.get(0).unwrap();

            // let doc = &ex.doc;
            // let fuck = ah.node.read().await;
            // let node = (&*fuck).as_ref().unwrap().clone();
            // let mut stream = node.node.connections().await?;
            //
            // println!("NODE CONN INFO");
            // while let Some(Ok(e)) = stream.next().await {
            //     let e: ConnectionInfo = e;
            //     println!("node id {}, dr addrs {:?}", e.node_id, e.addrs)
            // }
            //
            // if let Some(peers) = doc.get_sync_peers().await? {
            //     println!("DOC SYNC PEERS");
            //     for p in peers {
            //         let thing: [u8; 32] = p;
            //         println!("peer id in bytes {:?}", p);
            //         println!("peer id wide: {}", WideId::from(thing));
            //     }
            //
            // }



            // let peer: [u8; 32] = [152, 112, 148, 173, 228, 128, 91, 6, 14, 56, 130, 147, 227, 138, 221, 160, 17, 123, 122, 23, 131, 156, 195, 195, 193, 184, 118, 130, 114, 147, 196, 55];
            // let pk = iroh::net::key::PublicKey::from_bytes(&peer)?;
            // let nodeaddr = NodeAddr::new(pk);
            // ex.doc.start_sync(vec![nodeaddr]).await?;

            tokio::time::sleep(Duration::from_millis(5000)).await;
            // let mut subby = ex.subscribe();
            // while let Ok(e) = subby.recv().await {
            //     match e {
            //         ContextEvents::Loaded => {
            //             println!("loaded?")
            //         }
            //         ContextEvents::Join(joiner) => {
            //             println!("joined: {joiner}");
            //             break;
            //         }
            //         ContextEvents::MessageReceived(msg) => {
            //             println!("msg: {}", msg.text);
            //         }
            //     }
            // };

            Ok(())
        });
        ah.shutdown();
    }


    #[test]
    fn can_create_and_reload_data() {
        wipe_test_dir(None);

        fs::create_dir(TEST_DIR).unwrap();
        let ah = AppHost::new(AppConfig { data_path: TEST_DIR.into() } );

        let first = ah.handle().block_on(async {
            ah.identity.create_identification("kevin").await.unwrap();
            ah.identity.load_assumed_identity().await.unwrap();
            ah.identity.assumed_identity().await.unwrap().name
        });
        assert_eq!("kevin", first);

        ah.shutdown();

        let ah2 = AppHost::new(AppConfig { data_path: TEST_DIR.into() } );
        let created_name = ah2.handle().block_on(async {
            ah2.identity.load_assumed_identity().await.unwrap();
            ah2.identity.assumed_identity().await.unwrap().name
        });

        assert_eq!("kevin", created_name);
        ah2.shutdown();
    }

    #[test]
    fn create_exchange() {
        wipe_test_dir(None);
        let ah = AppHost::new(AppConfig { data_path: TEST_DIR.into() } );
        ah.handle().block_on(async {
            let id = ah.identity.create_identification("kevin").await.unwrap();
            let ex = ah.exchange.create_exchange(true).await.unwrap();
            println!("made id {}, and ex {}", id.public_key(), ex.id());
            let zz = ah.exchange.contexts().await;
            assert_eq!(zz[0].id(), ex.id());

            ah.exchange.create_exchange(true).await.unwrap();
            let killme = ah.exchange.create_exchange(true).await.unwrap();
            let more_ctxs = ah.exchange.contexts().await;
            assert_eq!(3, more_ctxs.len());

            ah.exchange.delete_exchange(killme.id()).await.unwrap();

            assert_eq!(2, ah.exchange.contexts().await.len());

        });
        ah.shutdown();
    }

    #[test]
    fn exchange_persists() {
        wipe_test_dir(None);
        let ah = AppHost::new(AppConfig { data_path: TEST_DIR.into() } );
        let first_exchange: ExchangeId = ah.handle().block_on(async {
            ah.identity.create_identification("kevin").await.unwrap();
            ah.exchange.create_exchange(true).await.unwrap();
            ah.exchange.contexts().await[0].id()
        });
        ah.shutdown();
        let ah = AppHost::new(AppConfig { data_path: TEST_DIR.into() } );
        let second_exchange: ExchangeId = ah.handle().block_on(async {
            ah.exchange.reload_contexts(None, None).await.expect("couldn't load contexts");
            ah.exchange.contexts().await[0].id()
        });
        ah.shutdown();
        assert_eq!(first_exchange, second_exchange);
    }

    #[test]
    fn creator() {
        wipe_test_dir(None);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let abort_tx = tx.clone();
        // first
        let ah = AppHost::new(AppConfig { data_path: TEST_DIR.into() } );

        // setup aborter
        ah.handle().spawn(async move {
            tokio::time::sleep(Duration::from_millis(10000)).await;
            abort_tx.send("timeout".into()).await
        });

        let token = ah.handle().block_on(async {
            ah.identity.create_identification("kevin").await.unwrap();
            let ex = ah.exchange.create_exchange(true ).await.unwrap();


            let tkn = ex.generate_join_ticket().await.expect("failed to generate join ticket");

            let mut listener = ex.subscribe();
            let ex_clone = ex.clone();
            ah.handle().spawn(async move {
                let ex = ex_clone;
                match listener.recv().await {
                    Ok(ExchangeEvents::Join(pk)) => {
                        let found = ex.participants().await.into_iter().find(|p| {
                            p.public_key() == &pk
                        });
                        match found {
                            None => { }
                            Some(i) => {
                                tx.send(String::from(i.name())).await.unwrap();
                            }
                        };
                    },
                    Ok(_) => { },
                    Err(..) => { }
                }
            });
            tkn
        });

        let jh = ah.handle().spawn(async move {
           match rx.recv().await {
               Some(str) => {
                   str
               }
               None => {
                   String::from("err")
               }
           }
        });

        println!("alright lets interact with it");
        use iroh::ticket::DocTicket;

        ah.handle().block_on(async {
            let node: iroh::node::MemNode = iroh::node::Node::memory().spawn().await.unwrap();
            let dt: DocTicket = iroh::base::ticket::Ticket::deserialize(&token).unwrap();
            let doc = node.docs.import(dt).await.unwrap();
            let author = node.authors.create().await?;
            let pk: PublicKey = author.into();
            let iden = Identification {
                public_key: pk,
                name: String::from("hurshal")
            };
            let blob_add = node.blobs.serialize_write_blob(iden).await?;
            doc.set_hash(author, String::from("identification"), blob_add.hash, blob_add.size).await?;
            Ok::<(), anyhow::Error>(())
        }).unwrap();


        let str = ah.handle().block_on(jh).unwrap();
        ah.shutdown();
        assert_eq!(str, "hurshal");

    }

    #[test]
    fn joiner() {
        wipe_test_dir(None);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let abort_tx = tx.clone();
        // first
        let ah = AppHost::new(AppConfig { data_path: TEST_DIR.into() } );

        // setup aborter
        ah.handle().spawn(async move {
            tokio::time::sleep(Duration::from_millis(10000)).await;
            abort_tx.send("timeout".into()).await
        });

        let ticket = ah.handle().block_on(async {
            let node: iroh::node::MemNode = iroh::node::Node::memory().spawn().await.unwrap();
            let doc = node.docs.create().await.unwrap();
            println!("a doc exists at {}", doc.id());
            let author = node.authors.create().await?;
            let pk: PublicKey = author.into();
            let iden = Identification {
                public_key: pk,
                name: String::from("hurshal")
            };
            let blob_add = node.blobs.serialize_write_blob(iden).await?;
            doc.set_hash(author, String::from("identification"), blob_add.hash, blob_add.size).await?;
            let ticket = doc.share(Write, AddrInfoOptions::Addresses).await?.serialize();
            Ok::<String, anyhow::Error>(ticket)
        }).unwrap();

        let jh = ah.handle().spawn(async move {
            match rx.recv().await {
                Some(str) => { str }
                None => { String::from("err") }
            }
        });

        ah.handle().block_on(async {
            ah.identity.create_identification("kevin").await?;
            let ex = ah.exchange.join_exchange(&ticket, true).await?;

            let mut listener = ex.subscribe();
            let ex_clone = ex.clone();
            ah.handle().spawn(async move {
                let ex = ex_clone;
                match listener.recv().await {
                    Ok(ExchangeEvents::Join(pk)) => {
                        let found = ex.participants().await.into_iter().find(|p| {
                            p.public_key() == &pk
                        });
                        match found {
                            None => { }
                            Some(i) => {
                                tx.send(String::from(i.name())).await.unwrap();
                            }
                        };
                    },
                    Ok(_) => { },
                    Err(..) => { }
                }
            });
            Ok::<(), anyhow::Error>(())
        }).unwrap();

        let str = ah.handle().block_on(jh).unwrap();
        ah.shutdown();
        assert_eq!(str, "hurshal");
        println!("Success!");
        drop(ah);

        // re-open with persistence helping!
        let ah = AppHost::new(AppConfig { data_path: TEST_DIR.into() } );
        let names = ah.handle().block_on(async {
            ah.exchange.reload_contexts(None, None).await.expect("go");
            let ex = ah.exchange.contexts().await.remove(0);
            println!("subscribing to worker events");
            let mut sub = ex.subscribe();
            println!("subscribed");

            if let Ok(ContextEvents::Loaded) = sub.recv().await {
                let mut names: Vec<String> = ex.participants().await.into_iter().map(|p|p.name).collect();
                names.sort();
                println!("names {:?}", names);
                names
            } else {
                vec![]
            }
        });
        assert_eq!(names[0], "hurshal");
        assert_eq!(names[1], "kevin");

    }

    #[test]
    fn sends_messages() {
        wipe_test_dir(None);
        let ah = AppHost::new(AppConfig { data_path: TEST_DIR.into() } );

        let (tx, mut rx) = tokio::sync::mpsc::channel(3);

        let (id, ex_id) = ah.handle().block_on(async {
            let id = ah.identity.create_identification("kevin").await.unwrap();
            let ex = ah.exchange.create_exchange(true).await.unwrap();
            let ex_id = ex.id();

            let mut subby = ex.subscribe();
            Handle::current().spawn(async move {
                let tx = tx;
                while let Ok(e) = subby.recv().await {
                    println!("got event {:?}", e);
                    match e {
                       ContextEvents::MessageReceived(m)=> {
                           tx.send(m.text).await.unwrap();
                       },
                       _ => {

                       }
                    }
                }
            });

            ex.send_message(&Message::new(id.public_key(), "first")).await.unwrap();
            ex.send_message(&Message::new(id.public_key(), "second")).await.unwrap();
            (id, ex_id)
        });

        assert_eq!("first",  rx.blocking_recv().unwrap());
        assert_eq!("second",  rx.blocking_recv().unwrap());
        ah.shutdown();
        drop(ah);

        // reload from persistence!
        let ah = AppHost::new(AppConfig { data_path: TEST_DIR.into() } );
        let (tx, mut rx) = tokio::sync::mpsc::channel(3);
        let ex = ah.handle().block_on(async {
            ah.exchange.reload_contexts(None, None).await.unwrap();
            let ex = ah.exchange.context_by_id(&ex_id).await.unwrap();
            let mut subby = ex.subscribe();
            let ex_inside = ex.clone();
            Handle::current().spawn(async move {
               while let Ok(e) = subby.recv().await {
                   match e {
                       ContextEvents::Loaded => {
                           println!("my loaded messages are!! {:?}", ex_inside.messages().await)
                       }
                       ContextEvents::Join(_) => {}
                       ContextEvents::MessageReceived(m) => {
                           tx.send(m.text).await.unwrap();
                       }
                   }
               }
            });
            ex.send_message(&Message::new(id.public_key(), "third")).await.unwrap();
            ex
        });
        assert_eq!("third",  rx.blocking_recv().unwrap());
        let z: Vec<String> = ah.handle().block_on(async { ex.messages().await }).into_iter().map(|m|m.text).collect();
        assert_eq!("firstsecondthird", z.join(""));
        ah.shutdown();
        drop(ah);
    }

}