// Magical Bitcoin Library
// Written in 2020 by
//     Alekos Filini <alekos.filini@gmail.com>
//
// Copyright (c) 2020 Magical Bitcoin
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use std::collections::{HashMap, HashSet};

#[allow(unused_imports)]
use log::{debug, error, info, trace};

use bitcoin::{BlockHeader, OutPoint, Script, Transaction, Txid};

use super::*;
use crate::database::{BatchDatabase, BatchOperations, DatabaseUtils};
use crate::error::Error;
use crate::types::{ScriptType, TransactionDetails, UTXO};
use crate::wallet::utils::ChunksIterator;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::iter::FromIterator;
use std::time::Instant;

#[derive(Debug)]
pub struct ELSGetHistoryRes {
    pub height: i32,
    pub tx_hash: Txid,
}

#[derive(Debug)]
pub struct ELSListUnspentRes {
    pub height: usize,
    pub tx_hash: Txid,
    pub tx_pos: usize,
}

/// Implements the synchronization logic for an Electrum-like client.
#[maybe_async]
pub trait ElectrumLikeSync {
    fn els_batch_script_get_history<'s, I: IntoIterator<Item = &'s Script>>(
        &self,
        scripts: I,
    ) -> Result<Vec<Vec<ELSGetHistoryRes>>, Error>;

    fn els_batch_script_list_unspent<'s, I: IntoIterator<Item = &'s Script>>(
        &self,
        scripts: I,
    ) -> Result<Vec<Vec<ELSListUnspentRes>>, Error>;

    fn els_batch_transaction_get<'s, I: IntoIterator<Item = &'s Txid>>(
        &self,
        txids: I,
    ) -> Result<Vec<Transaction>, Error>;

    fn els_batch_block_header<I: IntoIterator<Item = u32>>(
        &self,
        heights: I,
    ) -> Result<Vec<BlockHeader>, Error>;

    fn els_transaction_get(&self, txid: &Txid) -> Result<Transaction, Error>;

    // Provided methods down here...

    /// MR description
    ///
    /// improvement and future improvement: faster, consider more than 100 addresses, tx timestamp
    /// future improvement:
    ///
    fn electrum_like_setup<D: BatchDatabase, P: Progress>(
        &self,
        stop_gap: Option<usize>,
        database: &mut D,
        _progress_update: P,
    ) -> Result<(), Error> {
        // TODO: progress
        let start = Instant::now();
        info!("start setup at {:?}", start);

        let stop_gap = stop_gap.unwrap_or(20);
        let chunk_size = stop_gap;

        let mut history_txs_id = HashSet::new();
        let mut txid_height = HashMap::new();
        let mut max_index = HashMap::new();

        let mut wallet_chains = vec![ScriptType::Internal, ScriptType::External];
        // shuffling improve privacy, the server doesn't know my first request is from my internal or external addresses
        wallet_chains.shuffle(&mut thread_rng());
        // download history of our internal and external script_pubkeys
        for script_type in wallet_chains.iter() {
            let script_iter = database.iter_script_pubkeys(Some(*script_type))?.into_iter();
            for (i, chunk) in ChunksIterator::new(script_iter, stop_gap).enumerate() {
                // TODO if i == last, should create another chunk of addresses in db
                let call_result: Vec<Vec<ELSGetHistoryRes>> =
                    maybe_await!(self.els_batch_script_get_history(chunk.iter()))?;
                if let Some(max) = find_max_index(&call_result) {
                    max_index.insert(script_type, max);
                }
                let flattened: Vec<ELSGetHistoryRes> = call_result.into_iter().flatten().collect();
                info!("#{} of {:?} results:{}", i, script_type, flattened.len());
                if flattened.is_empty() {
                    // Didn't find anything in the last `stop_gap` script_pubkeys, breaking
                    break;
                }

                for el in flattened {
                    // el.height = -1 means unconfirmed with unconfirmed parents
                    // el.height =  0 means unconfirmed with confirmed parents
                    // but we threat those tx the same
                    let height = el.height.max(0);
                    if height == 0 {
                        txid_height.insert(el.tx_hash, None);
                    } else {
                        txid_height.insert(el.tx_hash, Some(height as u32));
                    }
                    history_txs_id.insert(el.tx_hash);
                }
            }
        }

        // saving max indexes
        for script_type in wallet_chains.iter() {
            if let Some(index) = max_index.get(script_type) {
                database.set_last_index(*script_type, *index)?;
            }
        }

        // get db status
        let tx_details_in_db = database.iter_txs(false)?;
        let txids_details_in_db = HashSet::from_iter(tx_details_in_db.iter().map(|tx| tx.txid));
        let tx_raw_in_db = database.iter_raw_txs()?;
        let txids_raw_in_db = HashSet::from_iter(tx_raw_in_db.iter().map(|tx| tx.txid()));

        // download new txs and headers
        let new_txs =
            self.download_needed_raw_txs(&history_txs_id, &txids_raw_in_db, chunk_size)?;
        let new_timestamps =
            self.download_needed_headers(&txid_height, &txids_details_in_db, chunk_size)?;

        // save any raw tx not in db, it's required they are in db for the next step
        if !new_txs.is_empty() {
            // TODO what if something breaks in the middle of the sync, may be better to save raw tx at every chunk during download
            let mut batch = database.begin_batch();
            for new_tx in new_txs.iter() {
                batch.set_raw_tx(new_tx)?;
            }
            database.commit_batch(batch)?;
        }

        // save any tx details not in db but in history_txs_id
        let mut batch = database.begin_batch();
        for txid in history_txs_id.difference(&txids_details_in_db) {
            let timestamp = *new_timestamps.get(txid).unwrap(); // TODO should be ok to unwrap
            let height = txid_height.get(txid).unwrap().clone();
            save_transaction_details_and_utxos(txid, database, timestamp, height, &mut batch)?;
        }
        database.commit_batch(batch)?;

        // remove any tx details in db but not in history_txs_id
        let mut batch = database.begin_batch();
        for tx_details in database.iter_txs(false)? {
            if !history_txs_id.contains(&tx_details.txid) {
                batch.del_tx(&tx_details.txid, false)?;
            }
        }
        database.commit_batch(batch)?;

        // remove any spent utxo
        let mut batch = database.begin_batch();
        for new_tx in new_txs.iter() {
            for input in new_tx.input.iter() {
                batch.del_utxo(&input.previous_output)?;
            }
        }
        database.commit_batch(batch)?;

        info!("finish setup, elapsed {:?}ms", start.elapsed().as_millis());

        Ok(())
    }

    /// download txs identified by `history_txs_id` and theirs previous outputs if not already present in db
    fn download_needed_raw_txs(
        &self,
        history_txs_id: &HashSet<Txid>,
        txids_in_db: &HashSet<Txid>,
        chunk_size: usize,
    ) -> Result<Vec<Transaction>, Error> {
        let mut txs_downloaded = vec![];
        let txids_to_download: Vec<&Txid> = history_txs_id.difference(&txids_in_db).collect();
        if !txids_to_download.is_empty() {
            info!("got {} txs to download", txids_to_download.len());
            txs_downloaded.extend(self.download_in_chunks(txids_to_download, chunk_size)?);
            let mut previous_txids = HashSet::new();
            let mut txids_downloaded = HashSet::new();
            for tx in txs_downloaded.iter() {
                txids_downloaded.insert(tx.txid());
                for input in tx.input.iter() {
                    previous_txids.insert(input.previous_output.txid);
                }
            }
            let already_present: HashSet<Txid> =
                txids_downloaded.union(&txids_in_db).cloned().collect();
            let previous_txs_to_download: Vec<&Txid> =
                previous_txids.difference(&already_present).collect();
            txs_downloaded.extend(self.download_in_chunks(previous_txs_to_download, chunk_size)?);
        }
        Ok(txs_downloaded)
    }

    /// download headers at heights in `txid_height` if tx details not already present, returns a map Txid -> timestamp
    fn download_needed_headers(
        &self,
        txid_height: &HashMap<Txid, Option<u32>>,
        txid_details_in_db: &HashSet<Txid>,
        chunk_size: usize,
    ) -> Result<HashMap<Txid, u64>, Error> {
        let needed_txid_height: HashMap<&Txid, &Option<u32>> = txid_height
            .iter()
            .filter(|(txid, _)| !txid_details_in_db.contains(*txid))
            .collect();
        let needed_heights: Vec<u32> = needed_txid_height.iter().filter_map(|(_, b)| **b).collect();

        let mut height_timestamp: HashMap<u32, u64> = HashMap::new();
        for chunk in ChunksIterator::new(needed_heights.into_iter(), chunk_size) {
            let call_result: Vec<BlockHeader> =
                maybe_await!(self.els_batch_block_header(chunk.clone()))?;
            let vec: Vec<(u32, u64)> = chunk
                .into_iter()
                .zip(call_result.iter().map(|h| h.time as u64))
                .collect();
            height_timestamp.extend(vec);
        }

        let mut txid_timestamp = HashMap::new();
        for (txid, height_opt) in needed_txid_height {
            if let Some(height) = height_opt {
                txid_timestamp.insert(txid.clone(), *height_timestamp.get(height).unwrap());
                // TODO check unwrap
            }
        }

        Ok(txid_timestamp)
    }

    fn download_in_chunks(
        &self,
        to_download: Vec<&Txid>,
        chunk_size: usize,
    ) -> Result<Vec<Transaction>, Error> {
        let mut txs_downloaded = vec![];
        for chunk in ChunksIterator::new(to_download.into_iter(), chunk_size) {
            let call_result: Vec<Transaction> =
                maybe_await!(self.els_batch_transaction_get(chunk))?;
            txs_downloaded.extend(call_result);
        }
        Ok(txs_downloaded)
    }
}

fn save_transaction_details_and_utxos<D: BatchDatabase>(
    txid: &Txid,
    database: &mut D,
    timestamp: u64,
    height: Option<u32>,
    updates: &mut dyn BatchOperations,
) -> Result<(), Error> {
    let tx = database.get_raw_tx(txid).unwrap().unwrap(); // TODO everything is in db, but handle errors

    let mut incoming: u64 = 0;
    let mut outgoing: u64 = 0;

    let mut inputs_sum: u64 = 0;
    let mut outputs_sum: u64 = 0;

    // look for our own inputs
    for input in tx.input.iter() {
        // skip coinbase inputs
        if input.previous_output.is_null() {
            continue;
        }

        // We already downloaded all previous output txs in the previous step
        if let Some(previous_output) = database.get_previous_output(&input.previous_output)? {
            inputs_sum += previous_output.value;

            if database.is_mine(&previous_output.script_pubkey)? {
                outgoing += previous_output.value;
            }
        } else {
            // The input is not ours, but we still need to count it for the fees
            let tx = database.get_raw_tx(&input.previous_output.txid)?.unwrap(); // TODO safe
            inputs_sum += tx.output[input.previous_output.vout as usize].value;
        }
    }

    for (i, output) in tx.output.iter().enumerate() {
        // to compute the fees later
        outputs_sum += output.value;

        // this output is ours, we have a path to derive it
        if let Some((script_type, _child)) =
            database.get_path_from_script_pubkey(&output.script_pubkey)?
        {
            debug!("{} output #{} is mine, adding utxo", txid, i);
            updates.set_utxo(&UTXO {
                outpoint: OutPoint::new(tx.txid(), i as u32),
                txout: output.clone(),
                is_internal: script_type.is_internal(),
            })?;
            incoming += output.value;
        }
    }

    let tx_details = TransactionDetails {
        txid: tx.txid(),
        transaction: Some(tx),
        received: incoming,
        sent: outgoing,
        height,
        timestamp,
        fees: inputs_sum.saturating_sub(outputs_sum), // if the tx is a coinbase, fees would be negative
    };
    updates.set_tx(&tx_details)?;

    Ok(())
}

fn find_max_index(vec: &Vec<Vec<ELSGetHistoryRes>>) -> Option<u32> {
    vec.iter()
        .enumerate()
        .filter(|(_, v)| !v.is_empty())
        .map(|(i, _)| i as u32)
        .max()
}
