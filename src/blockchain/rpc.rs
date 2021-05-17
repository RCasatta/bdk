use crate::bitcoin::consensus::deserialize;
use crate::bitcoin::{Address, Network, OutPoint, Transaction, TxOut, Txid};
use crate::blockchain::{Blockchain, Capability, ConfigurableBlockchain, Progress};
use crate::database::BatchDatabase;
use crate::descriptor::{get_checksum, ExtendedDescriptor};
use crate::{Error, FeeRate, KeychainKind, LocalUtxo, TransactionDetails};
use bitcoincore_rpc::json::{
    GetAddressInfoResultLabel, ImportMultiOptions, ImportMultiRequest,
    ImportMultiRequestScriptPubkey, ImportMultiRescanSince,
};
use bitcoincore_rpc::jsonrpc::serde_json::Value;
use bitcoincore_rpc::{Auth, Client, RpcApi};
use log::debug;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

#[derive(Debug)]
pub struct RpcBlockchain {
    client: Client,
    network: Network,
    capabilities: HashSet<Capability>,
    rpc_version: usize,
    wallet_name: String,

    // This is a fixed Address used as a key to store information on the node
    satoshi_address: Address,
}

#[derive(Debug)]
pub struct RpcConfig {
    pub url: String,
    pub auth: Auth,
    pub network: Network,

    pub descriptor: ExtendedDescriptor,
    pub change_descriptor: Option<ExtendedDescriptor>,
}

impl RpcBlockchain {
    fn get_node_synced_height(&self) -> Result<u32, Error> {
        let info = self.client.get_address_info(&self.satoshi_address)?;
        if let Some(GetAddressInfoResultLabel::Simple(label)) = info.labels.first() {
            Ok(label.parse::<u32>().unwrap_or(0))
        } else {
            Ok(0)
        }
    }

    fn set_node_synced_height(&self, height: u32) -> Result<(), Error> {
        Ok(self
            .client
            .set_label(&self.satoshi_address, &height.to_string())?)
    }
}

impl Blockchain for RpcBlockchain {
    fn get_capabilities(&self) -> HashSet<Capability> {
        self.capabilities.clone()
    }

    fn setup<D: BatchDatabase, P: 'static + Progress>(
        &self,
        stop_gap: Option<usize>,
        database: &mut D,
        progress_update: P,
    ) -> Result<(), Error> {
        let mut scripts_pubkeys = database.iter_script_pubkeys(Some(KeychainKind::External))?;
        scripts_pubkeys.extend(database.iter_script_pubkeys(Some(KeychainKind::Internal))?);
        debug!(
            "importing {} script_pubkeys (some maybe already imported)",
            scripts_pubkeys.len()
        );
        let requests: Vec<_> = scripts_pubkeys
            .iter()
            .map(|s| ImportMultiRequest {
                timestamp: ImportMultiRescanSince::Timestamp(0),
                script_pubkey: Some(ImportMultiRequestScriptPubkey::Script(&s)),
                watchonly: Some(true),
                ..Default::default()
            })
            .collect();
        let options = ImportMultiOptions {
            rescan: Some(false),
        };
        // Note we use import_multi because as of bitcoin core 0.21.0 many descriptors are not supported
        // https://bitcoindevkit.org/descriptors/#compatibility-matrix
        //TODO maybe convenient using import_descriptor for compatible descriptor and import_multi as fallback
        self.client.import_multi(&requests, Some(&options))?;
        self.sync(stop_gap, database, progress_update)
    }

    fn sync<D: BatchDatabase, P: 'static + Progress>(
        &self,
        _stop_gap: Option<usize>,
        db: &mut D,
        progress_update: P,
    ) -> Result<(), Error> {
        debug!("sync");
        let current_height = self.get_height()?;
        let node_synced = self.get_node_synced_height()?;

        //TODO if current_height == node_synced should check only the mempool

        //TODO split the interval in chunk so that we can give progress update
        self.client
            .rescan_blockchain(Some(node_synced as usize), Some(current_height as usize))?;
        progress_update.update(1.0, None)?;

        let known_txs: HashMap<_, _> = db
            .iter_raw_txs()?
            .into_iter()
            .map(|tx| (tx.txid(), tx))
            .collect();
        let known_utxos: HashSet<_> = db.iter_utxos()?.into_iter().collect();

        //TODO list_since_blocks would be more efficient
        let current_utxo = self.client.list_unspent(Some(0), None, None, None, None)?;
        debug!("current_utxo len {}", current_utxo.len());
        let current_txs = self
            .client
            .list_transactions(None, None, None, Some(true))?;
        debug!("current_txs len {}", current_txs.len());

        let mut indexes = HashMap::new();
        for keykind in vec![KeychainKind::External, KeychainKind::Internal] {
            indexes.insert(keykind, db.get_last_index(keykind)?.unwrap_or(0));
        }

        for tx_result in current_txs {
            if !known_txs.contains_key(&tx_result.info.txid) {
                let tx_result = self
                    .client
                    .get_transaction(&tx_result.info.txid, Some(true))?;
                let tx: Transaction = deserialize(&tx_result.hex)?;

                for output in tx.output.iter() {
                    if let Ok(Some((kind, index))) =
                        db.get_path_from_script_pubkey(&output.script_pubkey)
                    {
                        if index > *indexes.get(&kind).unwrap() {
                            indexes.insert(kind, index);
                        }
                    }
                }

                let td = TransactionDetails {
                    transaction: Some(tx),
                    txid: tx_result.info.txid,
                    timestamp: tx_result.info.time,
                    received: 0,                                                 //TODO
                    sent: 0,                                                     //TODO
                    fees: tx_result.fee.map(|f| f.as_sat() as u64).unwrap_or(0), //TODO
                    height: tx_result.info.blockheight,
                };
                debug!("saving tx: {}", tx_result.info.txid);
                db.set_tx(&td)?; //TODO batching
            }
        }

        let current_utxos: HashSet<LocalUtxo> = current_utxo
            .into_iter()
            .map(|u| LocalUtxo {
                outpoint: OutPoint::new(u.txid, u.vout),
                txout: TxOut {
                    value: u.amount.as_sat(),
                    script_pubkey: u.script_pub_key,
                },
                keychain: KeychainKind::External,
            })
            .collect();

        let spent: HashSet<_> = known_utxos.difference(&current_utxos).collect();
        for s in spent {
            debug!("removing utxo: {:?}", s);
            db.del_utxo(&s.outpoint)?; //TODO batching
        }
        let received: HashSet<_> = current_utxos.difference(&known_utxos).collect();
        for s in received {
            debug!("adding utxo: {:?}", s);
            db.set_utxo(s)?; //TODO batching
        }

        for (keykind, index) in indexes {
            debug!("{:?} max {}", keykind, index);
            db.set_last_index(keykind, index)?;
        }

        self.set_node_synced_height(current_height)?;
        Ok(())
    }

    fn get_tx(&self, txid: &Txid) -> Result<Option<Transaction>, Error> {
        if self.capabilities.contains(&Capability::FullHistory) {
            Ok(Some(self.client.get_raw_transaction(txid, None)?))
        } else {
            Ok(None)
        }
    }

    fn broadcast(&self, tx: &Transaction) -> Result<(), Error> {
        Ok(self.client.send_raw_transaction(tx).map(|_| ())?)
    }

    fn get_height(&self) -> Result<u32, Error> {
        Ok(self.client.get_blockchain_info().map(|i| i.blocks as u32)?)
    }

    fn estimate_fee(&self, target: usize) -> Result<FeeRate, Error> {
        let sat_per_kb = self
            .client
            .estimate_smart_fee(target as u16, None)?
            .fee_rate
            .ok_or(Error::FeeRateUnavailable)?
            .as_sat() as f64;
        dbg!(&sat_per_kb);

        Ok(FeeRate::from_sat_per_vb((sat_per_kb / 1000f64) as f32))
    }
}

impl ConfigurableBlockchain for RpcBlockchain {
    type Config = RpcConfig;

    /// Returns RpcBlockchain backend creating an RPC client to a specific wallet named as the descriptor's checksum
    /// if it's the first time it creates the wallet in the node and upon return is granted the wallet is loaded
    fn from_config(config: &Self::Config) -> Result<Self, Error> {
        //TODO check descriptors contains only public keys
        let mut wallet_name = get_checksum(config.descriptor.to_string().as_str())?;
        if let Some(change_descriptor) = config.change_descriptor.as_ref() {
            wallet_name.push_str(get_checksum(change_descriptor.to_string().as_str())?.as_str());
        }
        let wallet_url = format!("{}/wallet/{}", config.url, wallet_name);
        debug!("connecting to {} auth:{:?}", wallet_url, config.auth);

        let client = Client::new(wallet_url, config.auth.clone())?;
        let loaded_wallets = client.list_wallets()?;
        if loaded_wallets.contains(&wallet_name) {
            debug!("wallet already loaded {:?}", wallet_name);
        } else {
            let existing_wallets = list_wallet_dir(&client)?;
            if existing_wallets.contains(&wallet_name) {
                client.load_wallet(&wallet_name)?;
                debug!("wallet loaded {:?}", wallet_name);
            } else {
                client.create_wallet(&wallet_name, Some(true), None, None, None)?;
                debug!("wallet created {:?}", wallet_name);
            }
        }

        let blockchain_info = client.get_blockchain_info()?;
        let network = match blockchain_info.chain.as_str() {
            "main" => Network::Bitcoin,
            "test" => Network::Testnet,
            "regtest" => Network::Regtest,
            _ => return Err(Error::Generic("Invalid network".to_string())),
        };
        if network != config.network {
            return Err(Error::InvalidNetwork {
                requested: config.network,
                found: network,
            });
        }

        let mut capabilities: HashSet<_> = vec![Capability::FullHistory].into_iter().collect();
        let rpc_version = client.version()?;
        if rpc_version >= 210_000 {
            let info: HashMap<String, Value> = client.call("getindexinfo", &[]).unwrap();
            if info.contains_key("txindex") {
                capabilities.insert(Capability::GetAnyTx);
                capabilities.insert(Capability::AccurateFees);
            }
        }

        // this is just a fixed address used only to store a label containing the synced height in the node
        let mut satoshi_address = Address::from_str("1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa").unwrap();
        satoshi_address.network = network;

        Ok(RpcBlockchain {
            client,
            network,
            capabilities,
            rpc_version,
            wallet_name,
            satoshi_address,
        })
    }
}

/// return the wallets available in default wallet directory
//TODO PR to create method in bitcoincore_rpc
fn list_wallet_dir(client: &Client) -> Result<Vec<String>, Error> {
    #[derive(Deserialize)]
    struct Name {
        name: String,
    }
    #[derive(Deserialize)]
    struct Result {
        wallets: Vec<Name>,
    }

    let result: Result = client.call("listwalletdir", &[])?;
    Ok(result.wallets.into_iter().map(|n| n.name).collect())
}

#[cfg(test)]
mod test {
    use super::{RpcBlockchain, RpcConfig};
    use crate::bitcoin::consensus::deserialize;
    use crate::bitcoin::{Address, Amount, Network, Transaction};
    use crate::blockchain::{noop_progress, Blockchain, Capability, ConfigurableBlockchain};
    use crate::database::MemoryDatabase;
    use crate::descriptor::IntoWalletDescriptor;
    use crate::wallet::AddressIndex;
    use crate::Wallet;
    use bitcoin::secp256k1::Secp256k1;
    use bitcoin::Txid;
    use bitcoincore_rpc::json::CreateRawTransactionInput;
    use bitcoincore_rpc::RawTx;
    use bitcoincore_rpc::{Auth, RpcApi};
    use bitcoind::BitcoinD;
    use log::{LevelFilter, Metadata, Record};
    use std::collections::HashMap;
    use std::sync::Once;

    fn create_rpc(
        bitcoind: &BitcoinD,
        desc: &str,
        network: Network,
    ) -> Result<RpcBlockchain, crate::Error> {
        let secp = Secp256k1::new();
        let (desc, _) =
            IntoWalletDescriptor::into_wallet_descriptor(desc, &secp, Network::Regtest).unwrap();
        let config = RpcConfig {
            url: bitcoind.url.clone(),
            auth: Auth::CookieFile(bitcoind.cookie_file.clone()),
            network,
            descriptor: desc,
            change_descriptor: None,
        };
        RpcBlockchain::from_config(&config)
    }
    fn create_bitcoind(args: Vec<String>) -> BitcoinD {
        let exe = std::env::var("BITCOIND_EXE").unwrap();
        bitcoind::BitcoinD::with_args(exe, args, false).unwrap()
    }

    const EXAMPLE_DESCRIPTOR: &'static str = "wpkh(tpubD6NzVbkrYhZ4X2yy78HWrr1M9NT8dKeWfzNiQqDdMqqa9UmmGztGGz6TaLFGsLfdft5iu32gxq1T4eMNxExNNWzVCpf9Y6JZi5TnqoC9wJq/*)";

    #[test]
    fn test_rpc_wallet_setup() {
        init_logger();
        let bitcoind = create_bitcoind(vec![]);
        let blockchain = create_rpc(&bitcoind, EXAMPLE_DESCRIPTOR, Network::Regtest).unwrap();
        let db = MemoryDatabase::new();
        let wallet =
            Wallet::new(EXAMPLE_DESCRIPTOR, None, Network::Regtest, db, blockchain).unwrap();

        wallet.sync(noop_progress(), None).unwrap();
        generate(&bitcoind, 101);
        wallet.sync(noop_progress(), None).unwrap();
        let address = wallet.get_address(AddressIndex::New).unwrap();
        send_to_address(&bitcoind, &address, 100_000);
        wallet.sync(noop_progress(), None).unwrap();
        assert_eq!(wallet.get_balance().unwrap(), 100_000);
    }

    #[test]
    fn test_rpc_from_config() {
        let bitcoind = create_bitcoind(vec![]);
        let blockchain = create_rpc(&bitcoind, EXAMPLE_DESCRIPTOR, Network::Regtest);
        assert!(blockchain.is_ok());
        let blockchain = create_rpc(&bitcoind, EXAMPLE_DESCRIPTOR, Network::Testnet);
        assert!(blockchain.is_err(), "wrong network doesn't error");
    }

    #[test]
    fn test_rpc_capabilities_get_tx() {
        let bitcoind = create_bitcoind(vec![]);
        let rpc = create_rpc(&bitcoind, EXAMPLE_DESCRIPTOR, Network::Regtest).unwrap();
        let capabilities = rpc.get_capabilities();
        assert!(capabilities.contains(&Capability::FullHistory) && capabilities.len() == 1);
        let bitcoind_indexed = create_bitcoind(vec!["-txindex".to_string()]);
        let rpc_indexed =
            create_rpc(&bitcoind_indexed, EXAMPLE_DESCRIPTOR, Network::Regtest).unwrap();
        assert_eq!(rpc_indexed.get_capabilities().len(), 3);
        let address = generate(&bitcoind_indexed, 101);
        let txid = send_to_address(&bitcoind_indexed, &address, 100_000);
        assert!(rpc_indexed.get_tx(&txid).unwrap().is_some());
        assert!(rpc.get_tx(&txid).is_err());
    }

    #[test]
    fn test_rpc_estimate_fee_get_height() {
        let bitcoind = create_bitcoind(vec![]);
        let rpc = create_rpc(&bitcoind, EXAMPLE_DESCRIPTOR, Network::Regtest).unwrap();
        let result = rpc.estimate_fee(2);
        assert!(result.is_err());
        let address = generate(&bitcoind, 100);
        // create enough tx so that core give some fee estimation
        for _ in 0..15 {
            let _ = bitcoind.client.generate_to_address(1, &address).unwrap();
            for _ in 0..2 {
                send_to_address(&bitcoind, &address, 100_000);
            }
        }
        let result = rpc.estimate_fee(2);
        assert!(result.is_ok());
        assert_eq!(rpc.get_height().unwrap(), 115);
    }

    #[test]
    fn test_rpc_node_synced_height() {
        init_logger();
        let bitcoind = create_bitcoind(vec![]);
        let rpc = create_rpc(&bitcoind, EXAMPLE_DESCRIPTOR, Network::Regtest).unwrap();
        let synced_height = rpc.get_node_synced_height().unwrap();

        assert_eq!(synced_height, 0);
        rpc.set_node_synced_height(1).unwrap();

        let synced_height = rpc.get_node_synced_height().unwrap();
        assert_eq!(synced_height, 1);
    }

    #[test]
    fn test_rpc_broadcast() {
        let bitcoind = create_bitcoind(vec![]);
        let rpc = create_rpc(&bitcoind, EXAMPLE_DESCRIPTOR, Network::Regtest).unwrap();
        let address = generate(&bitcoind, 101);
        let utxo = bitcoind
            .client
            .list_unspent(None, None, None, None, None)
            .unwrap();
        let input = CreateRawTransactionInput {
            txid: utxo[0].txid,
            vout: utxo[0].vout,
            sequence: None,
        };

        let out: HashMap<_, _> = vec![(
            address.to_string(),
            utxo[0].amount - Amount::from_sat(100_000),
        )]
        .into_iter()
        .collect();
        let tx = bitcoind
            .client
            .create_raw_transaction(&[input], &out, None, None)
            .unwrap();
        let signed_tx = bitcoind
            .client
            .sign_raw_transaction_with_wallet(tx.raw_hex(), None, None)
            .unwrap();
        let parsed_tx: Transaction = deserialize(&signed_tx.hex).unwrap();
        rpc.broadcast(&parsed_tx).unwrap();
        assert!(bitcoind
            .client
            .get_raw_mempool()
            .unwrap()
            .contains(&tx.txid()));
    }

    fn generate(bitcoind: &BitcoinD, blocks: u64) -> Address {
        let address = bitcoind.client.get_new_address(None, None).unwrap();
        bitcoind
            .client
            .generate_to_address(blocks, &address)
            .unwrap();
        address
    }

    fn send_to_address(bitcoind: &BitcoinD, address: &Address, amount: u64) -> Txid {
        bitcoind
            .client
            .send_to_address(
                &address,
                Amount::from_sat(amount),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap()
    }

    static LOGGER: SimpleLogger = SimpleLogger;

    pub struct SimpleLogger;

    impl log::Log for SimpleLogger {
        fn enabled(&self, metadata: &Metadata) -> bool {
            metadata.level() <= log::max_level()
        }

        fn log(&self, record: &Record) {
            if let Some(path) = record.module_path() {
                if self.enabled(record.metadata()) && path.contains("bdk") {
                    print!("{} - {}\n", record.level(), record.args());
                }
            }
        }

        fn flush(&self) {}
    }
    static INIT: Once = Once::new();

    pub fn init_logger() {
        INIT.call_once(|| {
            let level = if cfg!(debug_assertions) {
                LevelFilter::Debug
            } else {
                LevelFilter::Off
            };
            log::set_logger(&LOGGER)
                .map(|()| log::set_max_level(level))
                .expect("cannot initialize logging");
        });
    }
}
