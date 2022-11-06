use std::any::Any;
use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use hashbrown::hash_map::RawEntryMut;
use polars_core::error::PolarsResult;
use polars_core::export::ahash::RandomState;
use polars_core::frame::hash_join::{ChunkId, _finish_join};
use polars_core::prelude::*;
use polars_core::series::IsSorted;
use polars_core::utils::{_set_partition_size, accumulate_dataframes_vertical_unchecked};
use polars_utils::hash_to_partition;
use polars_utils::slice::GetSaferUnchecked;
use polars_utils::unwrap::UnwrapUncheckedRelease;

use crate::executors::sinks::utils::{hash_series, load_vec};
use crate::executors::sinks::HASHMAP_INIT_SIZE;
use crate::expressions::PhysicalPipedExpr;
use crate::operators::{
    DataChunk, FinalizedSink, Operator, OperatorResult, PExecutionContext, Sink, SinkResult,
};

type ChunkIdx = IdxSize;
type DfIdx = IdxSize;
// This is the hash and the Index offset in the chunks and the index offset in the dataframe
#[derive(Copy, Clone, Debug)]
struct Key {
    hash: u64,
    chunk_idx: ChunkIdx,
    df_idx: DfIdx,
}

impl Key {
    #[inline]
    fn new(hash: u64, chunk_idx: ChunkIdx, df_idx: DfIdx) -> Self {
        Key {
            hash,
            chunk_idx,
            df_idx,
        }
    }
}

impl Hash for Key {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash)
    }
}

pub struct GenericBuild {
    chunks: Vec<DataChunk>,
    // the join columns are all tightly packed
    // the values of a join column(s) can be found
    // by:
    // first get the offset of the chunks and multiply that with the number of join
    // columns
    //      * chunk_offset = (idx * n_join_keys)
    //      * end = (offset + n_join_keys)
    materialized_join_cols: Vec<ArrayRef>,
    suffix: Arc<str>,
    hb: RandomState,
    // partitioned tables that will be used for probing
    // stores the key and the chunk_idx, df_idx of the left table
    hash_tables: Vec<PlIdHashMap<Key, Vec<ChunkId>>>,

    // the columns that will be joined on
    join_columns_left: Arc<Vec<Arc<dyn PhysicalPipedExpr>>>,
    join_columns_right: Arc<Vec<Arc<dyn PhysicalPipedExpr>>>,

    // amortize allocations
    join_series: Vec<Series>,
    hashes: Vec<u64>,
    join_type: JoinType,
    // the join order is swapped to ensure we hash the smaller table
    swapped: bool,
}

impl GenericBuild {
    pub(crate) fn new(
        suffix: Arc<str>,
        join_type: JoinType,
        swapped: bool,
        join_columns_left: Arc<Vec<Arc<dyn PhysicalPipedExpr>>>,
        join_columns_right: Arc<Vec<Arc<dyn PhysicalPipedExpr>>>,
    ) -> Self {
        let hb: RandomState = Default::default();
        let partitions = _set_partition_size();
        let hash_tables = load_vec(partitions, || PlIdHashMap::with_capacity(HASHMAP_INIT_SIZE));
        GenericBuild {
            chunks: vec![],
            join_type,
            suffix,
            hb,
            swapped,
            join_columns_left,
            join_columns_right,
            join_series: vec![],
            materialized_join_cols: vec![],
            hash_tables,
            hashes: vec![],
        }
    }
}

fn compare_fn(
    key: &Key,
    h: u64,
    join_columns_all_chunks: &[ArrayRef],
    current_row: &[AnyValue],
    n_join_cols: usize,
) -> bool {
    let key_hash = key.hash;

    let chunk_idx = key.chunk_idx as usize * n_join_cols;
    let df_idx = key.df_idx as usize;

    // get the right columns from the linearly packed buffer
    let join_cols = unsafe {
        join_columns_all_chunks.get_unchecked_release(chunk_idx..chunk_idx + n_join_cols)
    };

    // we check the hash and
    // we get the appropriate values from the join columns and compare it with the current row
    key_hash == h && {
        join_cols
            .iter()
            .zip(current_row)
            .all(|(column, value)| unsafe { &column.get_unchecked(df_idx) == value })
    }
}

impl GenericBuild {
    #[inline]
    fn number_of_keys(&self) -> usize {
        self.join_columns_left.len()
    }

    fn set_join_series(
        &mut self,
        context: &PExecutionContext,
        chunk: &DataChunk,
    ) -> PolarsResult<&[Series]> {
        self.join_series.clear();
        for phys_e in self.join_columns_left.iter() {
            let s = phys_e.evaluate(chunk, context.execution_state.as_ref())?;
            let s = s.to_physical_repr();
            let s = s.rechunk();
            self.materialized_join_cols.push(s.array_ref(0).clone());
            self.join_series.push(s);
        }
        Ok(&self.join_series)
    }
    unsafe fn get_tuple<'a>(
        &'a self,
        chunk_idx: ChunkIdx,
        df_idx: DfIdx,
        buf: &mut Vec<AnyValue<'a>>,
    ) {
        buf.clear();
        // get the right columns from the linearly packed buffer
        let join_cols = self
            .materialized_join_cols
            .get_unchecked_release(chunk_idx as usize..chunk_idx as usize + self.number_of_keys());
        buf.extend(
            join_cols
                .iter()
                .map(|arr| arr.get_unchecked(df_idx as usize)),
        )
    }
}

impl Sink for GenericBuild {
    fn sink(&mut self, context: &PExecutionContext, chunk: DataChunk) -> PolarsResult<SinkResult> {
        let mut hashes = std::mem::take(&mut self.hashes);
        self.set_join_series(context, &chunk)?;
        hash_series(&self.join_series, &mut hashes, &self.hb);
        self.hashes = hashes;

        let current_chunk_offset = self.chunks.len() as ChunkIdx;

        // iterators over anyvalues
        let mut key_iters = self
            .join_series
            .iter()
            .map(|s| s.phys_iter())
            .collect::<Vec<_>>();

        // a small buffer that holds the current key values
        // if we join by 2 keys, this holds 2 anyvalues.
        let mut current_tuple_buf = Vec::with_capacity(self.number_of_keys());
        let n_join_cols = self.number_of_keys();

        // row offset in the chunk belonging to the hash
        let mut current_df_idx = 0 as IdxSize;
        for h in &self.hashes {
            // load the keys in the buffer
            // TODO! write an iterator for this
            current_tuple_buf.clear();
            for key_iter in key_iters.iter_mut() {
                unsafe { current_tuple_buf.push(key_iter.next().unwrap_unchecked_release()) }
            }

            // get the hashtable belonging by this hash partition
            let partition = hash_to_partition(*h, self.hash_tables.len());
            let current_table = unsafe { self.hash_tables.get_unchecked_release_mut(partition) };

            let entry = current_table.raw_entry_mut().from_hash(*h, |key| {
                compare_fn(
                    key,
                    *h,
                    &self.materialized_join_cols,
                    &current_tuple_buf,
                    n_join_cols,
                )
            });

            let payload = [current_chunk_offset, current_df_idx];
            match entry {
                RawEntryMut::Vacant(entry) => {
                    let key = Key::new(*h, current_chunk_offset, current_df_idx);
                    entry.insert(key, vec![payload]);
                }
                RawEntryMut::Occupied(mut entry) => {
                    entry.get_mut().push(payload);
                }
            };

            current_df_idx += 1;
        }
        self.chunks.push(chunk);
        Ok(SinkResult::CanHaveMoreInput)
    }

    fn combine(&mut self, mut other: Box<dyn Sink>) {
        let other = other.as_any().downcast_ref::<Self>().unwrap();
        let mut tuple_buf = Vec::with_capacity(self.number_of_keys());

        let chunks_offset = self.chunks.len() as IdxSize;
        self.chunks.extend_from_slice(&other.chunks);
        self.materialized_join_cols
            .extend_from_slice(&other.materialized_join_cols);

        // we combine the other hashtable with ours, but we must offset the chunk_idx
        // values by the number of chunks we already got.
        for (ht, other_ht) in self.hash_tables.iter_mut().zip(&other.hash_tables) {
            for (k, val) in other_ht.iter() {
                // use the indexes to materialize the row
                for [chunk_idx, df_idx] in val {
                    unsafe { other.get_tuple(*chunk_idx, *df_idx, &mut tuple_buf) };
                }

                let h = k.hash;
                let entry = ht.raw_entry_mut().from_hash(h, |key| {
                    compare_fn(
                        key,
                        h,
                        &self.materialized_join_cols,
                        &tuple_buf,
                        tuple_buf.len(),
                    )
                });

                match entry {
                    RawEntryMut::Vacant(entry) => {
                        let [chunk_idx, df_idx] = unsafe { val.get_unchecked_release(0) };
                        let new_chunk_idx = chunk_idx + chunks_offset;
                        let key = Key::new(h, new_chunk_idx, *df_idx);
                        let mut payload = vec![[new_chunk_idx, *df_idx]];
                        if val.len() > 1 {
                            let iter = val[1..]
                                .iter()
                                .map(|[chunk_idx, val_idx]| [*chunk_idx + chunks_offset, *val_idx]);
                            payload.extend(iter);
                        }
                        entry.insert(key, payload);
                    }
                    RawEntryMut::Occupied(mut entry) => {
                        let iter = val
                            .iter()
                            .map(|[chunk_idx, val_idx]| [*chunk_idx + chunks_offset, *val_idx]);
                        entry.get_mut().extend(iter);
                    }
                }
            }
        }
    }

    fn split(&self, _thread_no: usize) -> Box<dyn Sink> {
        let mut new = Self::new(
            self.suffix.clone(),
            self.join_type.clone(),
            self.swapped,
            self.join_columns_left.clone(),
            self.join_columns_right.clone(),
        );
        new.hb = self.hb.clone();
        Box::new(new)
    }

    fn finalize(&mut self) -> PolarsResult<FinalizedSink> {
        match self.join_type {
            JoinType::Inner => {
                let left_df = Arc::new(accumulate_dataframes_vertical_unchecked(
                    std::mem::take(&mut self.chunks)
                        .into_iter()
                        .map(|chunk| chunk.data),
                ));
                let materialized_join_cols =
                    Arc::new(std::mem::take(&mut self.materialized_join_cols));
                let suffix = self.suffix.clone();
                let hb = self.hb.clone();
                let hash_tables = Arc::new(std::mem::take(&mut self.hash_tables));
                let join_columns_right = self.join_columns_right.clone();

                // take the buffers, this saves one allocation
                let mut join_series = std::mem::take(&mut self.join_series);
                join_series.clear();
                let mut hashes = std::mem::take(&mut self.hashes);
                hashes.clear();

                let probe_operator = InnerJoinProbe {
                    left_df,
                    materialized_join_cols,
                    suffix,
                    hb,
                    hash_tables,
                    join_columns_right,
                    join_series,
                    join_tuples_left: vec![],
                    join_tuples_right: vec![],
                    hashes,
                    swapped: self.swapped,
                    join_column_idx: None,
                };
                Ok(FinalizedSink::Operator(Box::new(probe_operator)))
            }
            _ => unimplemented!(),
        }
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Clone)]
pub struct InnerJoinProbe {
    // all chunks are stacked into a single dataframe
    // the dataframe is not rechunked.
    left_df: Arc<DataFrame>,
    // the join columns are all tightly packed
    // the values of a join column(s) can be found
    // by:
    // first get the offset of the chunks and multiply that with the number of join
    // columns
    //      * chunk_offset = (idx * n_join_keys)
    //      * end = (offset + n_join_keys)
    materialized_join_cols: Arc<Vec<ArrayRef>>,
    suffix: Arc<str>,
    hb: RandomState,
    // partitioned tables that will be used for probing
    // stores the key and the chunk_idx, df_idx of the left table
    hash_tables: Arc<Vec<PlIdHashMap<Key, Vec<ChunkId>>>>,

    // the columns that will be joined on
    join_columns_right: Arc<Vec<Arc<dyn PhysicalPipedExpr>>>,

    // amortize allocations
    join_series: Vec<Series>,
    join_tuples_left: Vec<ChunkId>,
    join_tuples_right: Vec<DfIdx>,
    hashes: Vec<u64>,
    // the join order is swapped to ensure we hash the smaller table
    swapped: bool,
    // location of join columns.
    // these column locations need to be dropped from the rhs
    join_column_idx: Option<Vec<usize>>,
}

impl InnerJoinProbe {
    #[inline]
    fn number_of_keys(&self) -> usize {
        self.join_columns_right.len()
    }

    fn set_join_series(
        &mut self,
        context: &PExecutionContext,
        chunk: &DataChunk,
    ) -> PolarsResult<&[Series]> {
        self.join_series.clear();
        for phys_e in self.join_columns_right.iter() {
            let s = phys_e.evaluate(chunk, context.execution_state.as_ref())?;
            let s = s.to_physical_repr();
            self.join_series.push(s.rechunk());
        }

        if self.join_column_idx.is_none() {
            let mut idx = self
                .join_series
                .iter()
                .filter_map(|s| chunk.data.find_idx_by_name(s.name()))
                .collect::<Vec<_>>();
            // ensure that it is sorted so that we can later remove columns in
            // a predictable order
            idx.sort_unstable();
            self.join_column_idx = Some(idx);
        }

        Ok(&self.join_series)
    }
}

impl Operator for InnerJoinProbe {
    fn execute(
        &mut self,
        context: &PExecutionContext,
        chunk: &DataChunk,
    ) -> PolarsResult<OperatorResult> {
        self.join_tuples_left.clear();
        self.join_tuples_right.clear();
        let mut hashes = std::mem::take(&mut self.hashes);
        self.set_join_series(context, chunk)?;
        hash_series(&self.join_series, &mut hashes, &self.hb);
        self.hashes = hashes;

        // iterators over anyvalues
        let mut key_iters = self
            .join_series
            .iter()
            .map(|s| s.phys_iter())
            .collect::<Vec<_>>();

        // a small buffer that holds the current key values
        // if we join by 2 keys, this holds 2 anyvalues.
        let mut current_tuple_buf = Vec::with_capacity(self.number_of_keys());

        for (i, h) in self.hashes.iter().enumerate() {
            let df_idx_right = i as IdxSize;

            // load the keys in the buffer
            current_tuple_buf.clear();
            for key_iter in key_iters.iter_mut() {
                unsafe { current_tuple_buf.push(key_iter.next().unwrap_unchecked_release()) }
            }
            // get the hashtable belonging by this hash partition
            let partition = hash_to_partition(*h, self.hash_tables.len());
            let current_table = unsafe { self.hash_tables.get_unchecked_release(partition) };

            let entry = current_table
                .raw_entry()
                .from_hash(*h, |key| {
                    compare_fn(
                        key,
                        *h,
                        &self.materialized_join_cols,
                        &current_tuple_buf,
                        current_tuple_buf.len(),
                    )
                })
                .map(|key_val| key_val.1);

            if let Some(indexes_left) = entry {
                self.join_tuples_left.extend_from_slice(indexes_left);
                self.join_tuples_right
                    .extend(std::iter::repeat(df_idx_right).take(indexes_left.len()));
            }
        }

        let left_df = unsafe {
            self.left_df
                ._take_chunked_unchecked_seq(&self.join_tuples_left, IsSorted::Not)
        };
        let right_df = unsafe {
            let mut df = Cow::Borrowed(&chunk.data);
            if let Some(ids) = &self.join_column_idx {
                let mut tmp = df.into_owned();
                let cols = tmp.get_columns_mut();
                // we go from higher idx to lower so that lower indices remain untouched
                // by our mutation
                for idx in ids.iter().rev() {
                    let _ = cols.remove(*idx);
                }
                df = Cow::Owned(tmp);
            }
            df._take_unchecked_slice(&self.join_tuples_right, false)
        };

        let (a, b) = if self.swapped {
            (right_df, left_df)
        } else {
            (left_df, right_df)
        };
        let out = _finish_join(a, b, Some(self.suffix.as_ref()))?;

        Ok(OperatorResult::Finished(chunk.with_data(out)))
    }

    fn split(&self, _thread_no: usize) -> Box<dyn Operator> {
        let new = self.clone();
        Box::new(new)
    }
}
