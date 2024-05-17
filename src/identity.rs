use std::ops::Deref;
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::{OnceCell, RwLock};

use crate::data::{Doc, load_from_doc_at_key, Node, PublicKey, save_on_doc_as_key};
use crate::settings::Service as SettingsService;

const IDENTIFICATION_PREFIX: &str = "identity/identification/";
const ASSUMED_IDENTITY_KEY: &str = "identity/assumed_pk";

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
#[derive(uniffi::Record)]
pub struct Identification {
    pub public_key: PublicKey,
    pub name: String
}

impl Identification {
    pub fn public_key(&self) -> &PublicKey { &self.public_key }
    pub fn name(&self) -> &str { &self.name }
}

pub trait ParticipantList {
    fn get_other(self, myself: &PublicKey) -> Option<Identification>;
}

impl ParticipantList for Vec<Identification> {
    fn get_other(self, myself: &PublicKey) -> Option<Identification> {
        let maybe_found: Option<Identification> = self.into_iter().find(|iden| {
            iden.public_key != *myself
        });
        maybe_found
    }
}


#[derive(Clone)]
pub struct Service(Arc<ServiceInner>);
pub struct ServiceInner {
    node: Node,
    settings: SettingsService,
    doc: OnceCell<Doc>,
    assumed_identity_pk: RwLock<Option<PublicKey>>
}

impl Deref for Service {
    type Target = ServiceInner;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}
impl Service {
    pub fn new(node: Node, settings: SettingsService) -> Service {

        let inner = ServiceInner {
            node,
            settings,
            doc: OnceCell::new(),
            assumed_identity_pk: RwLock::new(None)
        };

        Service(Arc::new(inner))
    }
    async fn doc(&self) -> &Doc {
        self.0.doc.get_or_init(|| async {
            self.0.settings.identity_doc().await.clone()
        }).await
    }

    pub async fn assumed_identity_pk(&self) -> Option<PublicKey> {
        let lock = self.assumed_identity_pk.read().await;
        if let Some(pk) = lock.as_ref() {
            Some(pk.clone())
        } else {
            None
        }
    }

    pub async fn load_assumed_identity(&self) -> Result<()> {
        let doc = self.doc().await;

        let maybe_iden: Result<PublicKey> = load_from_doc_at_key(&self.node, doc, None, ASSUMED_IDENTITY_KEY).await;
        match maybe_iden {
            Ok(iden) => {
                let mut iden_lock = self.assumed_identity_pk.write().await;
                *iden_lock = Some(iden);
            }
            Err(e) => {
                eprintln!("couldn't load an assumed identity: {e}")
            }
        }
        Ok(())
    }

    pub async fn assumed_identity(&self) -> Option<Identification> {
        let pk = self.assumed_identity_pk().await?;
        self.get_identification(&pk).await.ok()
    }

    pub async fn set_assumed_identity_pk(&self, pk: PublicKey) -> Result<()> {
        let mut iden_lock = self.assumed_identity_pk.write().await;
        save_on_doc_as_key(&self.node, self.doc().await, &pk, ASSUMED_IDENTITY_KEY, &pk).await?;
        *iden_lock = Some(pk);
        Ok(())
    }

    pub async fn get_identification(&self, pk: &PublicKey) -> Result<Identification> {
        let key = format!("{IDENTIFICATION_PREFIX}/{}", &pk);
        load_from_doc_at_key(&self.node, self.doc().await, None, &key).await
    }

    pub async fn create_identification(&self, name: &str) -> Result<Identification>
    {
        let author_id = self.node.authors.create().await?;
        let id = Identification {
            public_key: author_id.clone().into(),
            name: String::from(name)
            // signing shit
        };

        if self.assumed_identity_pk().await == None {
            self.set_assumed_identity_pk(id.public_key).await?;
        }
        let key = format!("{IDENTIFICATION_PREFIX}/{}", &id.public_key);
        save_on_doc_as_key(&self.node, self.doc().await, id.public_key(), &key, &id).await?;

        Ok(id)
    }

}