use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use crate::data::PublicKey;

pub const MANIFEST_KEY: &str = "manifest.v1";
pub const IDENTIFICATION_KEY: &str = "identification";
pub const FILE_KEY_PREFIX: &str = "files";
pub const ID_PIC: &str = "files/id_pic";
pub const MESSAGE_KEY_PREFIX: &str = "messages";

// actually saved on the settings doc
pub const SETTINGS_EXCHANGES_LIST_KEY: &str = "exchange/tracked_exchanges";

pub mod context;
mod service;

pub use service::Service as ExchangeService;
pub use service::Events as ExchangeServiceEvent;


#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct Message {
    pub pk: PublicKey,
    pub date: DateTime<Utc>,
    pub text: String
}

impl Message {
    pub fn new(pk: &PublicKey, text: &str) -> Message {
        Message {
            pk: pk.clone(),
            date: Utc::now(),
            text: String::from(text)
        }
    }
}
