use std::collections::HashMap;
use std::rc::Rc;

use rand::{self, Rng};

use crate::prelude::*;
use crate::state::keyed_state::VERSIONED_ROW_HEADER_SIZE;
use crate::state::single_state::SingleState;
use common::SizeOf;

#[derive(Default)]
pub struct MemoryState {
    state: Vec<SingleState>,
    by_tag: HashMap<Tag, usize>,
    mem_size: u64,
}

impl SizeOf for MemoryState {
    fn size_of(&self) -> u64 {
        use std::mem::size_of;

        size_of::<Self>() as u64
    }

    fn deep_size_of(&self) -> u64 {
        self.mem_size
    }
}

impl State for MemoryState {
    fn add_key(&mut self, columns: &[usize], partial: Option<Vec<Tag>>) {
        let (i, exists) = if let Some(i) = self.state_for(columns) {
            // already keyed by this key; just adding tags
            (i, true)
        } else {
            // will eventually be assigned
            (self.state.len(), false)
        };

        if let Some(ref p) = partial {
            for &tag in p {
                self.by_tag.insert(tag, i);
            }
        }

        if exists {
            return;
        }

        self.state
            .push(SingleState::new(columns, partial.is_some()));

        if !self.state.is_empty() && partial.is_none() {
            // we need to *construct* the index!
            let (new, old) = self.state.split_last_mut().unwrap();

            if !old.is_empty() {
                assert!(!old[0].partial());
                for (headers, rs) in old[0].versioned_values() {
                    debug_assert_eq!(headers.len(), rs.len());
                    for i in 1..headers.len() {
                        new.insert_row_with_header(Row::from(rs[i].0.clone()), headers[i].clone());
                    }
                }
            }
        }
    }

    fn is_useful(&self) -> bool {
        !self.state.is_empty()
    }

    fn is_partial(&self) -> bool {
        self.state.iter().any(SingleState::partial)
    }

    fn process_records(&mut self, records: &mut Records, ts: Timestamp, partial_tag: Option<Tag>) {
        if self.is_partial() {
            records.retain(|r| {
                // we need to check that we're not erroneously filling any holes
                // there are two cases here:
                //
                //  - if the incoming record is a partial replay (i.e., partial.is_some()), then we
                //    *know* that we are the target of the replay, and therefore we *know* that the
                //    materialization must already have marked the given key as "not a hole".
                //  - if the incoming record is a normal message (i.e., partial.is_none()), then we
                //    need to be careful. since this materialization is partial, it may be that we
                //    haven't yet replayed this `r`'s key, in which case we shouldn't forward that
                //    record! if all of our indices have holes for this record, there's no need for us
                //    to forward it. it would just be wasted work.
                //
                //    XXX: we could potentially save come computation here in joins by not forcing
                //    `right` to backfill the lookup key only to then throw the record away
                match *r {
                    Record::Positive(ref r) => self.insert(r.clone(), partial_tag, ts),
                    Record::Negative(ref r) => self.remove(r, ts),
                }
            });
        } else {
            for r in records.iter() {
                match *r {
                    Record::Positive(ref r) => {
                        let hit = self.insert(r.clone(), None, ts);
                        debug_assert!(hit);
                    }
                    Record::Negative(ref r) => {
                        let hit = self.remove(r, ts);
                        debug_assert!(hit);
                    }
                }
            }
        }
    }

    fn rows(&self) -> usize {
        self.state.iter().map(SingleState::rows).sum()
    }

    fn mark_filled(&mut self, key: Vec<DataType>, tag: Tag) {
        debug_assert!(!self.state.is_empty(), "filling uninitialized index");
        let index = self.by_tag[&tag];
        self.state[index].mark_filled(key);
    }

    fn mark_hole(&mut self, key: &[DataType], tag: Tag) {
        debug_assert!(!self.state.is_empty(), "filling uninitialized index");
        let index = self.by_tag[&tag];
        let freed_bytes = self.state[index].mark_hole(key);
        self.mem_size = self.mem_size.checked_sub(freed_bytes).unwrap();
    }

    fn lookup<'a>(&'a self, columns: &[usize], key: &KeyType) -> LookupResult<'a> {
        self.lookup_at(columns, key, None)
    }

    fn lookup_at<'a>(
        &'a self,
        columns: &[usize],
        key: &KeyType,
        ts: Option<Timestamp>,
    ) -> LookupResult<'a> {
        debug_assert!(!self.state.is_empty(), "lookup on uninitialized index");
        let index = self
            .state_for(columns)
            .expect("lookup on non-indexed column set");
        self.state[index].lookup(key, ts)
    }

    fn keys(&self) -> Vec<Vec<usize>> {
        self.state.iter().map(|s| s.key().to_vec()).collect()
    }

    fn cloned_records(&self) -> Vec<Vec<DataType>> {
        #[allow(clippy::ptr_arg)]
        fn fix<'a>(rs: &'a Vec<Row>) -> impl Iterator<Item = Vec<DataType>> + 'a {
            rs.iter().map(|r| Vec::clone(&**r))
        }

        assert!(!self.state[0].partial());
        self.state[0].values().flat_map(fix).collect()
    }

    fn evict_random_keys(&mut self, count: usize) -> (&[usize], Vec<Vec<DataType>>, u64) {
        let mut rng = rand::thread_rng();
        let index = rng.gen_range(0, self.state.len());
        let (bytes_freed, keys) = self.state[index].evict_random_keys(count, &mut rng);
        self.mem_size = self.mem_size.saturating_sub(bytes_freed);
        (self.state[index].key(), keys, bytes_freed)
    }

    fn evict_keys(&mut self, tag: Tag, keys: &[Vec<DataType>]) -> Option<(&[usize], u64)> {
        // we may be told to evict from a tag that add_key hasn't been called for yet
        // this can happen if an upstream domain issues an eviction for a replay path that we have
        // been told about, but that has not yet been finalized.
        self.by_tag.get(&tag).cloned().map(move |index| {
            let bytes = self.state[index].evict_keys(keys);
            self.mem_size = self.mem_size.saturating_sub(bytes);
            (self.state[index].key(), bytes)
        })
    }

    fn clear(&mut self) {
        for state in &mut self.state {
            state.clear();
        }
        self.mem_size = 0;
    }
}

impl MemoryState {
    /// Returns the index in `self.state` of the index keyed on `cols`, or None if no such index
    /// exists.
    fn state_for(&self, cols: &[usize]) -> Option<usize> {
        self.state.iter().position(|s| s.key() == cols)
    }

    fn insert(&mut self, r: Vec<DataType>, partial_tag: Option<Tag>, ts: Timestamp) -> bool {
        let r = Rc::new(r);

        if let Some(tag) = partial_tag {
            let i = match self.by_tag.get(&tag) {
                Some(i) => *i,
                None => {
                    // got tagged insert for unknown tag. this will happen if a node on an old
                    // replay path is now materialized. must return true to avoid any records
                    // (which are destined for a downstream materialization) from being pruned.
                    return true;
                }
            };
            self.mem_size += r.deep_size_of() + VERSIONED_ROW_HEADER_SIZE;
            self.state[i].insert_row(Row::from(r), ts)
        } else {
            let mut hit_any = false;
            for i in 0..self.state.len() {
                hit_any |= self.state[i].insert_row(Row::from(r.clone()), ts);
            }
            if hit_any {
                self.mem_size += r.deep_size_of();
            }
            hit_any
        }
    }

    fn remove(&mut self, r: &[DataType], ts: Timestamp) -> bool {
        let mut hit = false;
        for s in &mut self.state {
            s.remove_row(r, &mut hit, ts);
            // TODO: Vaccum the unused records
            // if let Some(row) = s.remove_row(r, &mut hit, ts) {
            //     if Rc::strong_count(&row.0) == 1 {
            //         self.mem_size = self.mem_size.checked_sub(row.deep_size_of()).unwrap();
            //     }
            // }
        }

        hit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert<S: State>(state: &mut S, row: Vec<DataType>) {
        let record: Record = row.into();
        state.process_records(&mut record.into(), state.current_ts, None);
    }

    #[test]
    fn memory_state_process_records() {
        let mut state = MemoryState::default();
        let mut records: Records = vec![
            (vec![1.into(), "A".into()], true),
            (vec![2.into(), "B".into()], true),
            (vec![3.into(), "C".into()], true),
            (vec![1.into(), "A".into()], false),
        ]
        .into();

        state.add_key(&[0], None);
        state.process_records(
            &mut Vec::from(&records[..3]).into(),
            state.current_ts,
            None,
        );
        state.process_records(&mut records[3].clone().into(), state.current_ts, None);

        // Make sure the first record has been deleted:
        match state.lookup(&[0], &KeyType::Single(&records[0][0])) {
            LookupResult::Some(RecordResult::Borrowed(rows)) => assert_eq!(rows.len(), 0),
            _ => unreachable!(),
        };

        // Then check that the rest exist:
        for record in &records[1..3] {
            match state.lookup(&[0], &KeyType::Single(&record[0])) {
                LookupResult::Some(RecordResult::Borrowed(rows)) => {
                    assert_eq!(&*rows[0], &**record)
                }
                _ => unreachable!(),
            };
        }
    }

    #[test]
    fn memory_state_old_records_new_index() {
        let mut state = MemoryState::default();
        let row: Vec<DataType> = vec![10.into(), "Cat".into()];
        state.add_key(&[0], None);
        insert(&mut state, row.clone());
        state.add_key(&[1], None);

        match state.lookup(&[1], &KeyType::Single(&row[1])) {
            LookupResult::Some(RecordResult::Borrowed(rows)) => assert_eq!(&*rows[0], &row),
            _ => unreachable!(),
        };
    }
}
