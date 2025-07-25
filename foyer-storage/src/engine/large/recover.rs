// Copyright 2025 foyer Project Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{atomic::Ordering, Arc},
    time::Instant,
};

use foyer_common::metrics::Metrics;
use futures_util::future::try_join_all;
use itertools::Itertools;
use tokio::sync::Semaphore;

use super::indexer::{EntryAddress, Indexer};
use crate::{
    engine::{
        large::{
            indexer::HashedEntryAddress,
            region::{Region, RegionId, RegionManager},
            scanner::{EntryInfo, RegionScanner},
            serde::{AtomicSequence, Sequence},
            tombstone::Tombstone,
        },
        RecoverMode,
    },
    error::{Error, Result},
    runtime::Runtime,
};

#[derive(Debug)]
pub struct RecoverRunner;

impl RecoverRunner {
    #[expect(clippy::too_many_arguments)]
    pub async fn run(
        recover_concurrency: usize,
        recover_mode: RecoverMode,
        blob_index_size: usize,
        regions: &[RegionId],
        sequence: &AtomicSequence,
        indexer: &Indexer,
        region_manager: &RegionManager,
        tombstones: &[Tombstone],
        runtime: Runtime,
        metrics: Arc<Metrics>,
    ) -> Result<()> {
        let now = Instant::now();

        // Recover regions concurrently.
        let semaphore = Arc::new(Semaphore::new(recover_concurrency));
        let mode = recover_mode;
        let handles = regions.iter().map(|id| {
            let semaphore = semaphore.clone();
            let region = region_manager.region(*id).clone();
            runtime.user().spawn(async move {
                let permit = semaphore.acquire().await;
                let res = RegionRecoverRunner::run(mode, region, blob_index_size).await;
                drop(permit);
                res
            })
        });
        let total = try_join_all(handles).await.unwrap();

        // Return error is there is.
        let (total, errs): (Vec<_>, Vec<_>) = total.into_iter().partition(|res| res.is_ok());
        if !errs.is_empty() {
            let errs = errs.into_iter().map(|r| r.unwrap_err()).collect_vec();
            return Err(Error::multiple(errs));
        }

        #[derive(Debug)]
        enum EntryAddressOrTombstone {
            EntryAddress(EntryAddress),
            Tombstone,
        }

        // Dedup entries.
        let mut latest_sequence = 0;
        let mut indices: HashMap<u64, Vec<(Sequence, EntryAddressOrTombstone)>> = HashMap::new();
        let mut clean_regions = vec![];
        let mut evictable_regions = vec![];
        for (region, infos) in total.into_iter().map(|r| r.unwrap()).enumerate() {
            let region = region as RegionId;

            if infos.is_empty() {
                clean_regions.push(region);
            } else {
                evictable_regions.push(region);
            }

            for EntryInfo { hash, addr } in infos {
                latest_sequence = latest_sequence.max(addr.sequence);
                indices
                    .entry(hash)
                    .or_default()
                    .push((addr.sequence, EntryAddressOrTombstone::EntryAddress(addr)));
            }
        }
        tombstones.iter().for_each(|tombstone| {
            latest_sequence = latest_sequence.max(tombstone.sequence);
            indices
                .entry(tombstone.hash)
                .or_default()
                .push((tombstone.sequence, EntryAddressOrTombstone::Tombstone))
        });
        let indices = indices
            .into_iter()
            .filter_map(|(hash, mut versions)| {
                versions.sort_by_key(|(sequence, _)| *sequence);
                tracing::trace!("[recover runner]: hash {hash} has versions: {versions:?}");
                match versions.pop() {
                    None => None,
                    Some((_, EntryAddressOrTombstone::Tombstone)) => None,
                    Some((_, EntryAddressOrTombstone::EntryAddress(address))) => {
                        Some(HashedEntryAddress { hash, address })
                    }
                }
            })
            .collect_vec();

        // Log recovery.
        tracing::info!(
            "Recovers {e} regions with data, {c} clean regions, {t} total entries with max sequence as {s}..",
            e = evictable_regions.len(),
            c = clean_regions.len(),
            t = indices.len(),
            s = latest_sequence,
        );

        // Update components.
        indexer.insert_batch(indices);
        sequence.store(latest_sequence + 1, Ordering::Release);
        region_manager.init(&clean_regions);

        let elapsed = now.elapsed();
        tracing::info!("[recover] finish in {:?}", elapsed);

        metrics.storage_lodc_recover_duration.record(elapsed.as_secs_f64());

        Ok(())
    }
}

#[derive(Debug)]
struct RegionRecoverRunner;

impl RegionRecoverRunner {
    async fn run(mode: RecoverMode, region: Region, blob_index_size: usize) -> Result<Vec<EntryInfo>> {
        if mode == RecoverMode::None {
            return Ok(vec![]);
        }

        let mut recovered = vec![];

        let id = region.id();
        let mut iter = RegionScanner::new(region, blob_index_size);
        'recover: loop {
            let r = iter.next().await;
            let infos = match r {
                Ok(Some(infos)) => infos,
                Ok(None) => break,
                Err(e) => {
                    if mode == RecoverMode::Strict {
                        return Err(e);
                    } else {
                        tracing::warn!("error raised when recovering region {id}, skip further recovery for {id}.");
                        break;
                    }
                }
            };

            for info in infos {
                if info.addr.sequence < recovered.last().map(|last: &EntryInfo| last.addr.sequence).unwrap_or(0) {
                    break 'recover;
                }
                recovered.push(info);
            }
        }

        Ok(recovered)
    }
}
