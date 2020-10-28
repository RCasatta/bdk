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

use bitcoin::{Address, Network, OutPoint, Script, Transaction, Txid};

use super::*;
use crate::database::{BatchDatabase, BatchOperations, DatabaseUtils};
use crate::error::Error;
use crate::types::{ScriptType, TransactionDetails, UTXO};
use crate::wallet::utils::ChunksIterator;
use electrum_client::GetHistoryRes;
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

    fn els_transaction_get(&self, txid: &Txid) -> Result<Transaction, Error>;

    // Provided methods down here...

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

        let mut wallet_chains = vec![ScriptType::External, ScriptType::External];
        // shuffling improve privacy, the server doesn't know my first request is from my internal or external addresses
        wallet_chains.shuffle(&mut thread_rng());
        // download history of our internal and external script_pubkeys
        for script_type in wallet_chains {
            let script_iter = database.iter_script_pubkeys(Some(script_type))?.into_iter();
            for (i, chunk) in ChunksIterator::new(script_iter, stop_gap).enumerate() {
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

        // get db status
        let tx_details_in_db = database.iter_txs(false)?;
        let tx_raw_in_db = database.iter_raw_txs()?;
        let txids_raw_in_db = HashSet::from_iter(tx_raw_in_db.iter().map(|tx| tx.txid()));

        // download new txs and headers
        let new_txs =
            self.download_needed_raw_txs(&history_txs_id, &txids_raw_in_db, chunk_size)?;
        let new_timestamps =
            self.download_needed_headers(&txid_height, &tx_details_in_db, chunk_size)?;

        // save any raw tx not in db
        if !new_txs.is_empty() {
            let mut batch = database.begin_batch();
            for new_tx in new_txs {
                batch.set_raw_tx(&new_tx);
            }
            database.commit_batch(batch);
        }

        // save any tx details not in db but in history_txs_id
        // remove any tx details in db but not in history_txs_id

        for tx_details in tx_details_in_db {}

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

    /// download headers at heights in `heights_set` if tx details not already present, returns a map heights -> timestamp
    fn download_needed_headers(
        &self,
        _txid_height: &HashMap<Txid, Option<u32>>,
        _tx_details_in_db: &Vec<TransactionDetails>,
        _chunk_size: usize,
    ) -> Result<HashMap<Txid, u64>, Error> {
        // TODO
        Ok(HashMap::new())
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

    /*
    fn electrum_like_setup<D: BatchDatabase, P: Progress>(
        &self,
        stop_gap: Option<usize>,
        database: &mut D,
        _progress_update: P,
    ) -> Result<(), Error> {
        // TODO: progress

        let stop_gap = stop_gap.unwrap_or(20);
        let batch_query_size = 20;

        // check unconfirmed tx, delete so they are retrieved later
        let mut del_batch = database.begin_batch();
        for tx in database.iter_txs(false)? {
            if tx.height.is_none() {
                del_batch.del_tx(&tx.txid, false)?;
            }
        }
        database.commit_batch(del_batch)?;

        // maximum derivation index for a change address that we've seen during sync
        let mut change_max_deriv = None;

        let mut already_checked: HashSet<Script> = HashSet::new();
        let mut to_check_later = VecDeque::with_capacity(batch_query_size);

        // insert the first chunk
        let mut iter_scriptpubkeys = database
            .iter_script_pubkeys(Some(ScriptType::External))?
            .into_iter();
        let chunk: Vec<Script> = iter_scriptpubkeys.by_ref().take(batch_query_size).collect();
        for item in chunk.into_iter().rev() {
            to_check_later.push_front(item);
        }

        let mut iterating_external = true;
        let mut index = 0;
        let mut last_found = None;
        while !to_check_later.is_empty() {
            trace!("to_check_later size {}", to_check_later.len());

            let until = cmp::min(to_check_later.len(), batch_query_size);
            let chunk: Vec<Script> = to_check_later.drain(..until).collect();
            let call_result = maybe_await!(self.els_batch_script_get_history(chunk.iter()))?;

            for (script, history) in chunk.into_iter().zip(call_result.into_iter()) {
                trace!("received history for {:?}, size {}", script, history.len());

                if !history.is_empty() {
                    last_found = Some(index);

                    let mut check_later_scripts = maybe_await!(self.check_history(
                        database,
                        script,
                        history,
                        &mut change_max_deriv
                    ))?
                    .into_iter()
                    .filter(|x| already_checked.insert(x.clone()))
                    .collect();
                    to_check_later.append(&mut check_later_scripts);
                }

                index += 1;
            }

            match iterating_external {
                true if index - last_found.unwrap_or(0) >= stop_gap => iterating_external = false,
                true => {
                    trace!("pushing one more batch from `iter_scriptpubkeys`. index = {}, last_found = {:?}, stop_gap = {}", index, last_found, stop_gap);

                    let chunk: Vec<Script> =
                        iter_scriptpubkeys.by_ref().take(batch_query_size).collect();
                    for item in chunk.into_iter().rev() {
                        to_check_later.push_front(item);
                    }
                }
                _ => {}
            }
        }

        // check utxo
        // TODO: try to minimize network requests and re-use scripts if possible
        let mut batch = database.begin_batch();
        for chunk in ChunksIterator::new(database.iter_utxos()?.into_iter(), batch_query_size) {
            let scripts: Vec<_> = chunk.iter().map(|u| &u.txout.script_pubkey).collect();
            let call_result = maybe_await!(self.els_batch_script_list_unspent(scripts))?;

            // check which utxos are actually still unspent
            for (utxo, list_unspent) in chunk.into_iter().zip(call_result.iter()) {
                debug!(
                    "outpoint {:?} is unspent for me, list unspent is {:?}",
                    utxo.outpoint, list_unspent
                );

                let mut spent = true;
                for unspent in list_unspent {
                    let res_outpoint = OutPoint::new(unspent.tx_hash, unspent.tx_pos as u32);
                    if utxo.outpoint == res_outpoint {
                        spent = false;
                        break;
                    }
                }
                if spent {
                    info!("{} not anymore unspent, removing", utxo.outpoint);
                    batch.del_utxo(&utxo.outpoint)?;
                }
            }
        }

        let current_ext = database.get_last_index(ScriptType::External)?.unwrap_or(0);
        let first_ext_new = last_found.map(|x| x + 1).unwrap_or(0) as u32;
        if first_ext_new > current_ext {
            info!("Setting external index to {}", first_ext_new);
            database.set_last_index(ScriptType::External, first_ext_new)?;
        }

        let current_int = database.get_last_index(ScriptType::Internal)?.unwrap_or(0);
        let first_int_new = change_max_deriv.map(|x| x + 1).unwrap_or(0);
        if first_int_new > current_int {
            info!("Setting internal index to {}", first_int_new);
            database.set_last_index(ScriptType::Internal, first_int_new)?;
        }

        database.commit_batch(batch)?;

        Ok(())
    }

    fn check_tx_and_descendant<D: BatchDatabase>(
        &self,
        database: &mut D,
        txid: &Txid,
        height: Option<u32>,
        cur_script: &Script,
        change_max_deriv: &mut Option<u32>,
    ) -> Result<Vec<Script>, Error> {
        debug!(
            "check_tx_and_descendant of {}, height: {:?}, script: {}",
            txid, height, cur_script
        );
        let mut updates = database.begin_batch();
        let tx = match database.get_tx(&txid, true)? {
            Some(mut saved_tx) => {
                // update the height if it's different (in case of reorg)
                if saved_tx.height != height {
                    info!(
                        "updating height from {:?} to {:?} for tx {}",
                        saved_tx.height, height, txid
                    );
                    saved_tx.height = height;
                    updates.set_tx(&saved_tx)?;
                }

                debug!("already have {} in db, returning the cached version", txid);

                // unwrap since we explicitly ask for the raw_tx, if it's not present something
                // went wrong
                saved_tx.transaction.unwrap()
            }
            None => {
                let fetched_tx = maybe_await!(self.els_transaction_get(&txid))?;
                database.set_raw_tx(&fetched_tx)?;

                fetched_tx
            }
        };

        let mut incoming: u64 = 0;
        let mut outgoing: u64 = 0;

        let mut inputs_sum: u64 = 0;
        let mut outputs_sum: u64 = 0;

        // look for our own inputs
        for (i, input) in tx.input.iter().enumerate() {
            // skip coinbase inputs
            if input.previous_output.is_null() {
                continue;
            }

            // the fact that we visit addresses in a BFS fashion starting from the external addresses
            // should ensure that this query is always consistent (i.e. when we get to call this all
            // the transactions at a lower depth have already been indexed, so if an outpoint is ours
            // we are guaranteed to have it in the db).
            if let Some(previous_output) = database.get_previous_output(&input.previous_output)? {
                inputs_sum += previous_output.value;

                if database.is_mine(&previous_output.script_pubkey)? {
                    outgoing += previous_output.value;

                    debug!("{} input #{} is mine, removing from utxo", txid, i);
                    updates.del_utxo(&input.previous_output)?;
                }
            } else {
                // The input is not ours, but we still need to count it for the fees. so fetch the
                // tx (from the database or from network) and check it
                let tx = match database.get_tx(&input.previous_output.txid, true)? {
                    Some(saved_tx) => saved_tx.transaction.unwrap(),
                    None => {
                        let fetched_tx =
                            maybe_await!(self.els_transaction_get(&input.previous_output.txid))?;
                        database.set_raw_tx(&fetched_tx)?;

                        fetched_tx
                    }
                };

                inputs_sum += tx.output[input.previous_output.vout as usize].value;
            }
        }

        let mut to_check_later = vec![];
        for (i, output) in tx.output.iter().enumerate() {
            // to compute the fees later
            outputs_sum += output.value;

            // this output is ours, we have a path to derive it
            if let Some((script_type, child)) =
                database.get_path_from_script_pubkey(&output.script_pubkey)?
            {
                debug!("{} output #{} is mine, adding utxo", txid, i);
                updates.set_utxo(&UTXO {
                    outpoint: OutPoint::new(tx.txid(), i as u32),
                    txout: output.clone(),
                    is_internal: script_type.is_internal(),
                })?;
                incoming += output.value;

                if output.script_pubkey != *cur_script {
                    debug!("{} output #{} script {} was not current script, adding script to be checked later", txid, i, output.script_pubkey);
                    to_check_later.push(output.script_pubkey.clone())
                }

                // derive as many change addrs as external addresses that we've seen
                if script_type == ScriptType::Internal
                    && (change_max_deriv.is_none() || child > change_max_deriv.unwrap_or(0))
                {
                    *change_max_deriv = Some(child);
                }
            }
        }

        let tx = TransactionDetails {
            txid: tx.txid(),
            transaction: Some(tx),
            received: incoming,
            sent: outgoing,
            height,
            timestamp: 0,
            fees: inputs_sum.saturating_sub(outputs_sum), // if the tx is a coinbase, fees would be negative
        };
        info!("Saving tx {}", txid);
        updates.set_tx(&tx)?;

        database.commit_batch(updates)?;

        Ok(to_check_later)
    }

    fn check_history<D: BatchDatabase>(
        &self,
        database: &mut D,
        script_pubkey: Script,
        txs: Vec<ELSGetHistoryRes>,
        change_max_deriv: &mut Option<u32>,
    ) -> Result<Vec<Script>, Error> {
        let mut to_check_later = Vec::new();

        debug!(
            "history of {} script {} has {} tx",
            Address::from_script(&script_pubkey, Network::Testnet).unwrap(),
            script_pubkey,
            txs.len()
        );

        for tx in txs {
            let height: Option<u32> = match tx.height {
                0 | -1 => None,
                x => u32::try_from(x).ok(),
            };

            to_check_later.extend_from_slice(&maybe_await!(self.check_tx_and_descendant(
                database,
                &tx.tx_hash,
                height,
                &script_pubkey,
                change_max_deriv,
            ))?);
        }

        Ok(to_check_later)
    }
    */
}

fn find_max_index(vec: &Vec<Vec<ELSGetHistoryRes>>) -> Option<u32> {
    vec.iter()
        .enumerate()
        .filter(|(_, v)| !v.is_empty())
        .map(|(i, _)| i as u32)
        .max()
}
