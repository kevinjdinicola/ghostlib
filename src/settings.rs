use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use iroh::sync::NamespaceId;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::OnceCell;

use crate::data::{Doc, Node};

const NODE_SETTINGS_FILE: &str = "node_root_settings_doc.bin";

#[derive(Clone)]
pub struct Service(Arc<ServiceInner>);
pub struct ServiceInner {
    node: Node,
    path: PathBuf,
    root_settings: OnceCell<Doc> // don't think I need arc if Service always lives in an arc
}

impl Deref for Service {
    type Target = ServiceInner;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl Service {
    pub fn new(node: Node, data_path: impl AsRef<Path>) -> Service {
        let path = data_path.as_ref();
        let path = path.join(NODE_SETTINGS_FILE);

        let inner = Arc::new(ServiceInner {
            node,
            path,
            root_settings: OnceCell::new()
        });

        Service(inner)
    }
    pub async fn identity_doc(&self) -> &Doc {
        // maybe we just share the same doc for now, we prefix everything
        self.settings_doc().await
    }
    pub async fn settings_doc(&self) -> &Doc {
        self.0.root_settings.get_or_init(|| async {
            let Service(inner) = self;

            let open_doc: Option<Doc> = if inner.path.exists() {
                let mut file = File::open(&inner.path).await.expect("Couldn't open existing settings namespace");
                let mut contents: [u8; 32] = [0; 32];
                if let Ok(_) = file.read(&mut contents).await {
                    let read_namespace = NamespaceId::from(&contents);
                    println!("Existing settings namespace found, opening: {}", read_namespace);
                    inner.node.docs.open(read_namespace).await.unwrap_or(None)
                } else {
                    None
                }
            } else {
                println!("Failed to open existing root settings path, creating new namespace");
                let doc = inner.node.docs.create().await.expect("Failed to create new doc for settings");
                let mut file = File::create(&inner.path).await.expect("couldnt open");
                let created_ns = doc.id();
                file.write(created_ns.as_bytes()).await.expect("Failed to save settings doc");
                // println!("Created {}", created_ns);
                Some(doc)
            };
            open_doc.expect("Failed to create settings namespace")

        }).await
    }

    pub async fn namespace(&self) -> NamespaceId {
        let iden_doc = self.settings_doc().await;
        iden_doc.id()
    }
}
