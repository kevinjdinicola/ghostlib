use std::fs;
use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;
use futures::channel::mpsc::Sender;
use ghostlib::AppConfig;
use ghostlib::dispatch::exchange::ExchangeDispatchResponder;
use ghostlib::dispatch::global::{GlobalActions, GlobalDispatchResponder, GlobalEvents};
use ghostlib::exchange::Message;


const TEST_DIR: &str = "./scratchdata";
fn wipe_test_dir(dir: Option<&str>) {
    let dir: &str = dir.unwrap_or_else(|| TEST_DIR);
    if let Ok(md) = fs::metadata(dir) {
        if md.is_dir() {
            fs::remove_dir_all(dir).unwrap();
        }
    }
}

fn main() {
    wipe_test_dir(None);
    fs::create_dir(TEST_DIR).unwrap();
    let ah = ghostlib::AppHost::new(AppConfig { data_path: "./scratch_data".into() });

    let r: anyhow::Result<()> = ah.handle().block_on(async {
        let iden = ah.identity.create_identification("scratch").await?;
        let ex = ah.exchange.create_exchange(true).await?;
        let ticket = ex.generate_join_ticket().await?;

        println!("join me at {ticket}");
        let mut counter = 0;
        loop {
            let mgs = Message::new(&iden.public_key,format!("its {counter}").as_str());
            ex.send_message(&mgs).await?;
            tokio::time::sleep(Duration::from_millis(5000)).await;
            counter += 1;
            if counter == 1000 { break; }
        };
        Ok(())
    });

}

