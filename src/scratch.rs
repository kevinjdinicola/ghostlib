use std::fs;

use tokio::runtime::Runtime;

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
    let _rt: Runtime = Runtime::new().unwrap();

    

}

