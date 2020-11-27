use bdk::blockchain::compact_filters::*;
use bdk::*;
use bitcoin::*;
use std::sync::Arc;
use std::path::PathBuf;
use blockchain::compact_filters::CompactFiltersBlockchain;
use blockchain::compact_filters::CompactFiltersError;
 use bdk::blockchain::noop_progress;
 use log::info;

/// This will return wallet balance using compact filters
/// NOTE: more than 5GB are downloaded and filters are not saved to disk
fn main() -> Result<(), CompactFiltersError> {
    env_logger::init();
    info!("start");

    let num_threads = 4;
    let mempool = Arc::new(Mempool::default());
    let peers = (0..num_threads)
        .map(|_| {
            Peer::connect(
                "btcd-mainnet.lightning.computer:8333", // Note: needed https://github.com/rust-bitcoin/rust-bitcoin/pull/529 to work with bitcoin core 0.21
                Arc::clone(&mempool),
                Network::Testnet,
            )
        })
        .collect::<Result<_, _>>()?;
    let blockchain = CompactFiltersBlockchain::new(peers, "./wallet-filters", Some(500_000))?;
    info!("done {:?}", blockchain);
    let descriptor = "wpkh(tpubD6NzVbkrYhZ4X2yy78HWrr1M9NT8dKeWfzNiQqDdMqqa9UmmGztGGz6TaLFGsLfdft5iu32gxq1T4eMNxExNNWzVCpf9Y6JZi5TnqoC9wJq/*)";

    let database = sled::open(prepare_home_dir().to_str().unwrap()).unwrap();
    let tree = database
        .open_tree("ciao")
        .unwrap();
    let wallet = Arc::new(
        Wallet::new(
            descriptor,
            None,
            Network::Testnet,
            tree,
            blockchain,
        )
        .unwrap(),
    );
    wallet.sync(noop_progress(), None).unwrap();
    Ok(())
}

fn prepare_home_dir() -> PathBuf {
    let mut dir = PathBuf::new();
    dir.push(&dirs_next::home_dir().unwrap());
    dir.push(".bdk-bitcoin");

    if !dir.exists() {
        info!("Creating home directory {}", dir.as_path().display());
        std::fs::create_dir(&dir).unwrap();
    }

    dir.push("database.sled");
    dir
}
