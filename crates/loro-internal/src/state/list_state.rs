use std::{io::Write, ops::RangeBounds, sync::Weak};

use super::{ApplyLocalOpReturn, ContainerState, DiffApplyContext, FastStateSnapshot};
use crate::{
    configure::Configure,
    container::{idx::ContainerIdx, list::list_op::ListOp, ContainerID},
    encoding::{EncodeMode, StateSnapshotDecodeContext, StateSnapshotEncoder},
    event::{Diff, Index, InternalDiff, ListDiff},
    handler::ValueOrHandler,
    op::{ListSlice, Op, RawOp, RawOpContent},
    LoroDocInner, LoroValue,
};

use fxhash::FxHashMap;
use generic_btree::{
    iter,
    rle::{CanRemove, HasLength, Mergeable, Sliceable, TryInsert},
    BTree, BTreeTrait, Cursor, LeafIndex, LengthFinder, UseLengthFinder,
};
use loro_common::{IdFull, IdLpSpan, LoroResult, ID};
use loro_delta::array_vec::ArrayVec;

#[derive(Debug)]
pub struct ListState {
    idx: ContainerIdx,
    list: BTree<ListImpl>,
    child_container_to_leaf: FxHashMap<ContainerID, LeafIndex>,
}

impl Clone for ListState {
    fn clone(&self) -> Self {
        Self {
            idx: self.idx,
            list: self.list.clone(),
            child_container_to_leaf: self.child_container_to_leaf.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Elem {
    pub v: LoroValue,
    pub id: IdFull,
}

impl HasLength for Elem {
    fn rle_len(&self) -> usize {
        1
    }
}

impl Sliceable for Elem {
    fn _slice(&self, range: std::ops::Range<usize>) -> Self {
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 1);
        self.clone()
    }

    fn split(&mut self, _pos: usize) -> Self {
        unreachable!()
    }
}

impl Mergeable for Elem {
    fn can_merge(&self, _rhs: &Self) -> bool {
        false
    }

    fn merge_right(&mut self, _rhs: &Self) {
        unreachable!()
    }

    fn merge_left(&mut self, _left: &Self) {
        unreachable!()
    }
}

impl TryInsert for Elem {
    fn try_insert(&mut self, _pos: usize, _elem: Self) -> Result<(), Self> {
        Err(_elem)
    }
}

impl CanRemove for Elem {
    fn can_remove(&self) -> bool {
        false
    }
}

struct ListImpl;
impl BTreeTrait for ListImpl {
    type Elem = Elem;
    type Cache = isize;
    type CacheDiff = isize;
    const USE_DIFF: bool = true;

    #[inline(always)]
    fn calc_cache_internal(
        cache: &mut Self::Cache,
        caches: &[generic_btree::Child<Self>],
    ) -> Self::CacheDiff {
        let mut new_cache = 0;
        for child in caches {
            new_cache += child.cache;
        }

        let diff = new_cache - *cache;
        *cache = new_cache;
        diff
    }

    #[inline(always)]
    fn apply_cache_diff(cache: &mut Self::Cache, diff: &Self::CacheDiff) {
        *cache += diff;
    }

    #[inline(always)]
    fn merge_cache_diff(diff1: &mut Self::CacheDiff, diff2: &Self::CacheDiff) {
        *diff1 += diff2
    }

    #[inline(always)]
    fn get_elem_cache(_elem: &Self::Elem) -> Self::Cache {
        1
    }

    #[inline(always)]
    fn new_cache_to_diff(cache: &Self::Cache) -> Self::CacheDiff {
        *cache
    }

    fn sub_cache(cache_lhs: &Self::Cache, cache_rhs: &Self::Cache) -> Self::CacheDiff {
        cache_lhs - cache_rhs
    }
}

impl UseLengthFinder<Self> for ListImpl {
    fn get_len(cache: &isize) -> usize {
        *cache as usize
    }
}

impl ListState {
    pub fn new(idx: ContainerIdx) -> Self {
        let tree = BTree::new();
        Self {
            idx,
            list: tree,
            child_container_to_leaf: Default::default(),
        }
    }

    pub fn contains_child_container(&self, id: &ContainerID) -> bool {
        let Some(&leaf) = self.child_container_to_leaf.get(id) else {
            return false;
        };

        self.list.get_elem(leaf).is_some()
    }

    pub fn get_child_container_index(&self, id: &ContainerID) -> Option<usize> {
        let leaf = *self.child_container_to_leaf.get(id)?;
        self.list.get_elem(leaf)?;
        let mut index = 0;
        self.list
            .visit_previous_caches(Cursor { leaf, offset: 0 }, |cache| match cache {
                generic_btree::PreviousCache::NodeCache(cache) => {
                    index += *cache;
                }
                generic_btree::PreviousCache::PrevSiblingElem(..) => {
                    index += 1;
                }
                generic_btree::PreviousCache::ThisElemAndOffset { .. } => {}
            });

        Some(index as usize)
    }

    pub fn insert(&mut self, index: usize, value: LoroValue, id: IdFull) {
        if index > self.len() {
            panic!("Index {index} out of range. The length is {}", self.len());
        }

        if self.list.is_empty() {
            let idx = self.list.push(Elem {
                v: value.clone(),
                id,
            });

            if value.is_container() {
                self.child_container_to_leaf
                    .insert(value.into_container().unwrap(), idx.leaf);
            }
            return;
        }

        let (leaf, data) = self.list.insert::<LengthFinder>(
            &index,
            Elem {
                v: value.clone(),
                id,
            },
        );

        if value.is_container() {
            self.child_container_to_leaf
                .insert(value.into_container().unwrap(), leaf.leaf);
        }

        assert!(data.arr.is_empty());
    }

    pub fn push(&mut self, value: LoroValue, id: IdFull) {
        if self.list.is_empty() {
            let idx = self.list.push(Elem {
                v: value.clone(),
                id,
            });

            if value.is_container() {
                self.child_container_to_leaf
                    .insert(value.into_container().unwrap(), idx.leaf);
            }
            return;
        }

        let leaf = self.list.push(Elem {
            v: value.clone(),
            id,
        });

        if value.is_container() {
            self.child_container_to_leaf
                .insert(value.into_container().unwrap(), leaf.leaf);
        }
    }

    pub fn delete(&mut self, index: usize) -> LoroValue {
        let leaf = self.list.query::<LengthFinder>(&index);
        let leaf = self.list.remove_leaf(leaf.unwrap().cursor).unwrap();
        if leaf.v.is_container() {
            self.child_container_to_leaf
                .remove(leaf.v.as_container().unwrap());
        }
        leaf.v
    }

    pub fn delete_range(
        &mut self,
        range: impl RangeBounds<usize>,
        mut notify_deletion: Option<&mut Vec<ContainerID>>,
    ) {
        let start: usize = match range.start_bound() {
            std::ops::Bound::Included(x) => *x,
            std::ops::Bound::Excluded(x) => *x + 1,
            std::ops::Bound::Unbounded => 0,
        };
        let end: usize = match range.end_bound() {
            std::ops::Bound::Included(x) => *x + 1,
            std::ops::Bound::Excluded(x) => *x,
            std::ops::Bound::Unbounded => self.len(),
        };
        if end - start == 1 {
            if let LoroValue::Container(c) = self.delete(start) {
                if let Some(notify_deletion) = &mut notify_deletion {
                    notify_deletion.push(c);
                }
            }
            return;
        }

        let list = &mut self.list;
        let q = start..end;
        let start1 = list.query::<LengthFinder>(&q.start);
        let end1 = list.query::<LengthFinder>(&q.end);
        for v in iter::Drain::new(list, start1, end1) {
            if v.v.is_container() {
                self.child_container_to_leaf
                    .remove(v.v.as_container().unwrap());
                if let Some(notify_deletion) = &mut notify_deletion {
                    notify_deletion.push(v.v.into_container().unwrap());
                }
            }
        }
    }

    // PERF: use &[LoroValue]
    // PERF: batch
    pub fn insert_batch(&mut self, index: usize, values: Vec<LoroValue>, start_id: IdFull) {
        let mut id = start_id;
        for (i, value) in values.into_iter().enumerate() {
            self.insert(index + i, value, id);
            id = id.inc(1);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &LoroValue> {
        self.list.iter().map(|x| &x.v)
    }

    #[allow(unused)]
    pub(crate) fn iter_with_id(&self) -> impl Iterator<Item = &Elem> {
        self.list.iter()
    }

    pub fn len(&self) -> usize {
        *self.list.root_cache() as usize
    }

    fn to_vec(&self) -> Vec<LoroValue> {
        let mut ans = Vec::with_capacity(self.len());
        for value in self.list.iter() {
            ans.push(value.v.clone());
        }
        ans
    }

    pub fn get(&self, index: usize) -> Option<&LoroValue> {
        let result = self.list.query::<LengthFinder>(&index)?;
        if result.found {
            Some(&result.elem(&self.list).unwrap().v)
        } else {
            None
        }
    }

    pub fn get_id_at(&self, index: usize) -> Option<IdFull> {
        let result = self.list.query::<LengthFinder>(&index)?;
        if result.found {
            Some(result.elem(&self.list).unwrap().id)
        } else {
            None
        }
    }

    #[allow(unused)]
    pub(crate) fn check(&self) {
        for value in self.iter() {
            if let LoroValue::Container(c) = value {
                self.get_child_index(c).unwrap();
            }
        }
    }

    pub fn get_index_of_id(&self, id: ID) -> Option<usize> {
        for (i, elem) in self.iter_with_id().enumerate() {
            if elem.id.id() == id {
                return Some(i);
            }
        }
        None
    }
}

impl ContainerState for ListState {
    fn container_idx(&self) -> ContainerIdx {
        self.idx
    }

    fn estimate_size(&self) -> usize {
        // TODO: this is inaccurate
        self.list.node_len() * std::mem::size_of::<isize>()
            + self.len() * std::mem::size_of::<Elem>()
            + self.child_container_to_leaf.len() * std::mem::size_of::<(ContainerID, LeafIndex)>()
    }

    fn is_state_empty(&self) -> bool {
        self.list.is_empty()
    }

    fn apply_diff_and_convert(
        &mut self,
        diff: InternalDiff,
        DiffApplyContext { doc, .. }: DiffApplyContext,
    ) -> Diff {
        let InternalDiff::ListRaw(delta) = diff else {
            unreachable!()
        };
        let mut ans: ListDiff = ListDiff::default();
        let mut index = 0;
        let doc = &doc.upgrade().unwrap();
        for span in delta.iter() {
            match span {
                crate::delta::DeltaItem::Retain { retain: len, .. } => {
                    index += len;
                    ans.push_retain(*len, Default::default());
                }
                crate::delta::DeltaItem::Insert { insert: value, .. } => {
                    let mut arr = Vec::new();
                    match &value.values {
                        either::Either::Left(range) => {
                            for i in range.to_range() {
                                let value = doc.arena.get_value(i).unwrap();
                                arr.push(value);
                            }
                        }
                        either::Either::Right(v) => arr.push(v.clone()),
                    }
                    for arr in ArrayVec::from_many(
                        arr.iter()
                            .map(|v| ValueOrHandler::from_value(v.clone(), doc)),
                    ) {
                        ans.push_insert(arr, Default::default());
                    }
                    let len = arr.len();
                    self.insert_batch(index, arr, value.id);
                    index += len;
                }
                crate::delta::DeltaItem::Delete { delete: len, .. } => {
                    self.delete_range(index..index + len, None);
                    ans.push_delete(*len);
                }
            }
        }

        Diff::List(ans)
    }

    fn apply_diff(&mut self, diff: InternalDiff, DiffApplyContext { doc, .. }: DiffApplyContext) {
        let doc = &doc.upgrade().unwrap();
        match diff {
            InternalDiff::ListRaw(delta) => {
                let mut index = 0;
                for span in delta.iter() {
                    match span {
                        crate::delta::DeltaItem::Retain { retain: len, .. } => {
                            index += len;
                        }
                        crate::delta::DeltaItem::Insert { insert: value, .. } => {
                            let mut arr = Vec::new();
                            match &value.values {
                                either::Either::Left(range) => {
                                    for i in range.to_range() {
                                        let value = doc.arena.get_value(i).unwrap();
                                        arr.push(value);
                                    }
                                }
                                either::Either::Right(v) => arr.push(v.clone()),
                            }

                            let len = arr.len();
                            self.insert_batch(index, arr, value.id);
                            index += len;
                        }
                        crate::delta::DeltaItem::Delete { delete: len, .. } => {
                            self.delete_range(index..index + len, None);
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    fn apply_local_op(&mut self, op: &RawOp, _: &Op) -> LoroResult<ApplyLocalOpReturn> {
        let mut ans: ApplyLocalOpReturn = Default::default();
        match &op.content {
            RawOpContent::List(list) => match list {
                ListOp::Insert { slice, pos } => match slice {
                    ListSlice::RawData(list) => match list {
                        std::borrow::Cow::Borrowed(list) => {
                            self.insert_batch(*pos, list.to_vec(), op.id_full());
                        }
                        std::borrow::Cow::Owned(list) => {
                            self.insert_batch(*pos, list.clone(), op.id_full());
                        }
                    },
                    _ => unreachable!(),
                },
                ListOp::Delete(del) => {
                    self.delete_range(del.span.to_urange(), Some(&mut ans.deleted_containers));
                }
                ListOp::Move { .. } => {
                    unreachable!()
                }
                ListOp::StyleStart { .. } => unreachable!(),
                ListOp::StyleEnd { .. } => unreachable!(),
                ListOp::Set { .. } => {
                    unreachable!()
                }
            },
            _ => unreachable!(),
        }
        Ok(ans)
    }

    #[doc = " Convert a state to a diff that when apply this diff on a empty state,"]
    #[doc = " the state will be the same as this state."]
    fn to_diff(&mut self, doc: &Weak<LoroDocInner>) -> Diff {
        let doc = &doc.upgrade().unwrap();
        Diff::List(ListDiff::from_many(
            self.to_vec()
                .into_iter()
                .map(|v| ValueOrHandler::from_value(v, doc)),
        ))
    }

    fn get_value(&mut self) -> LoroValue {
        let ans = self.to_vec();
        LoroValue::List(ans.into())
    }

    fn get_child_index(&self, id: &ContainerID) -> Option<Index> {
        self.get_child_container_index(id).map(Index::Seq)
    }

    fn contains_child(&self, id: &ContainerID) -> bool {
        self.contains_child_container(id)
    }

    fn get_child_containers(&self) -> Vec<ContainerID> {
        let mut ans = Vec::new();
        for elem in self.list.iter() {
            if elem.v.is_container() {
                ans.push(elem.v.as_container().unwrap().clone());
            }
        }
        ans
    }

    #[doc = "Get a list of ops that can be used to restore the state to the current state"]
    fn encode_snapshot(&self, mut encoder: StateSnapshotEncoder) -> Vec<u8> {
        for elem in self.list.iter() {
            let id_span: IdLpSpan = elem.id.idlp().into();
            encoder.encode_op(id_span, || unimplemented!());
        }

        Vec::new()
    }

    #[doc = "Restore the state to the state represented by the ops that exported by `get_snapshot_ops`"]
    fn import_from_snapshot_ops(&mut self, ctx: StateSnapshotDecodeContext) -> LoroResult<()> {
        assert_eq!(ctx.mode, EncodeMode::OutdatedSnapshot);
        let mut index = 0;
        for op in ctx.ops {
            let value = op.op.content.as_list().unwrap().as_insert().unwrap().0;
            let list = ctx
                .oplog
                .arena
                .get_values(value.0.start as usize..value.0.end as usize);
            let len = list.len();
            self.insert_batch(index, list, op.id_full());
            index += len;
        }
        Ok(())
    }

    fn fork(&self, _config: &Configure) -> Self {
        self.clone()
    }
}

mod snapshot {
    use std::io::Read;

    use loro_common::{Counter, Lamport, PeerID};
    use serde_columnar::columnar;

    use crate::{encoding::value_register::ValueRegister, state::ContainerCreationContext};

    use super::*;
    #[columnar(vec, ser, de, iterable)]
    #[derive(Debug, Clone)]
    struct EncodedListId {
        #[columnar(strategy = "DeltaRle")]
        peer_idx: usize,
        #[columnar(strategy = "DeltaRle")]
        counter: i32,
        #[columnar(strategy = "DeltaRle")]
        lamport_sub_counter: i32,
    }

    #[columnar(ser, de)]
    struct EncodedListIds {
        #[columnar(class = "vec", iter = "EncodedListId")]
        ids: Vec<EncodedListId>,
    }

    impl FastStateSnapshot for ListState {
        /// Encodes the ListState snapshot in a compact binary format:
        /// 1. Encodes the list values using postcard serialization
        /// 2. Encodes a table of unique peer IDs
        /// 3. For each element, encodes its ID as:
        ///    - Index of the peer ID in the table (LEB128)
        ///    - Counter (LEB128)
        ///    - Lamport timestamp (LEB128)
        fn encode_snapshot_fast<W: Write>(&mut self, mut w: W) {
            let value = self.get_value().into_list().unwrap();
            postcard::to_io(&*value, &mut w).unwrap();
            let mut peers: ValueRegister<PeerID> = ValueRegister::new();
            let mut ids = Vec::with_capacity(self.len());
            for elem in self.iter_with_id() {
                let id = elem.id;
                let peer_idx = peers.register(&id.peer);
                ids.push(EncodedListId {
                    peer_idx,
                    counter: id.counter,
                    lamport_sub_counter: (id.lamport as i32 - id.counter),
                });
            }

            let peers = peers.unwrap_vec();
            leb128::write::unsigned(&mut w, peers.len() as u64).unwrap();
            for peer in peers {
                w.write_all(&peer.to_le_bytes()).unwrap();
            }

            let id_bytes = serde_columnar::to_vec(&EncodedListIds { ids }).unwrap();
            w.write_all(&id_bytes).unwrap();
        }
        fn decode_value(bytes: &[u8]) -> LoroResult<(LoroValue, &[u8])> {
            let (value, bytes) = postcard::take_from_bytes(bytes).map_err(|_| {
                loro_common::LoroError::DecodeError(
                    "Decode list value failed".to_string().into_boxed_str(),
                )
            })?;
            let value: Vec<LoroValue> = value;
            Ok((LoroValue::List(value.into()), bytes))
        }

        fn decode_snapshot_fast(
            idx: ContainerIdx,
            (v, mut bytes): (LoroValue, &[u8]),
            _ctx: ContainerCreationContext,
        ) -> LoroResult<Self>
        where
            Self: Sized,
        {
            let peer_num = leb128::read::unsigned(&mut bytes).unwrap() as usize;
            let mut peers = Vec::with_capacity(peer_num);
            for _ in 0..peer_num {
                let mut buf = [0u8; 8];
                bytes.read_exact(&mut buf).unwrap();
                peers.push(PeerID::from_le_bytes(buf));
            }

            let EncodedListIds { ids } = serde_columnar::from_bytes(bytes).unwrap();

            let list = v.as_list().unwrap();
            let mut ans = Self::new(idx);
            for (i, id) in ids.into_iter().enumerate() {
                ans.insert(
                    i,
                    list[i].clone(),
                    IdFull::new(
                        peers[id.peer_idx],
                        id.counter as Counter,
                        (id.lamport_sub_counter + id.counter) as Lamport,
                    ),
                );
            }

            Ok(ans)
        }
    }
}

#[cfg(test)]
mod test {
    use itertools::Itertools;
    use loro_common::{Counter, Lamport};

    use crate::state::ContainerCreationContext;

    use super::*;

    #[test]
    fn test() {
        let mut list = ListState::new(ContainerIdx::from_index_and_type(
            0,
            loro_common::ContainerType::List,
        ));
        fn id(name: &str) -> ContainerID {
            ContainerID::new_root(name, crate::ContainerType::List)
        }
        list.insert(0, LoroValue::Container(id("abc")), IdFull::new(0, 0, 0));
        list.insert(0, LoroValue::Container(id("x")), IdFull::new(0, 0, 0));
        assert_eq!(list.get_child_container_index(&id("x")), Some(0));
        assert_eq!(list.get_child_container_index(&id("abc")), Some(1));
        list.insert(1, LoroValue::Bool(false), IdFull::new(0, 0, 0));
        assert_eq!(list.get_child_container_index(&id("x")), Some(0));
        assert_eq!(list.get_child_container_index(&id("abc")), Some(2));
    }

    #[test]
    fn test_list_fast_snapshot() {
        let mut list = ListState::new(ContainerIdx::from_index_and_type(
            0,
            loro_common::ContainerType::List,
        ));
        let mut w = Vec::new();
        list.encode_snapshot_fast(&mut w);
        println!("Empty: {}", w.len());

        list.insert(0, LoroValue::I64(0), IdFull::new(0, 0, 0));
        list.insert(1, LoroValue::I64(2), IdFull::new(1, 1, 1));
        list.insert(2, LoroValue::I64(4), IdFull::new(1, 2, 2));
        let mut w = Vec::new();
        list.encode_snapshot_fast(&mut w);
        assert!(w.len() <= 39, "w.len() = {}", w.len());
        let (v, left) = ListState::decode_value(&w).unwrap();
        let mut new_list = ListState::decode_snapshot_fast(
            ContainerIdx::from_index_and_type(0, loro_common::ContainerType::List),
            (v.clone(), left),
            ContainerCreationContext {
                configure: &Default::default(),
                peer: 0,
            },
        )
        .unwrap();
        new_list.check();
        assert_eq!(
            new_list.get_value(),
            vec![LoroValue::I64(0), LoroValue::I64(2), LoroValue::I64(4)].into()
        );
        assert_eq!(new_list.get_value(), v);
        let v = new_list.list.iter().collect_vec();
        assert_eq!(v[0].id.peer, 0);
        assert_eq!(v[0].id.counter, 0 as Counter);
        assert_eq!(v[0].id.lamport, 0 as Lamport);

        assert_eq!(v[1].id.peer, 1);
        assert_eq!(v[1].id.counter, 1 as Counter);
        assert_eq!(v[1].id.lamport, 1 as Lamport);

        assert_eq!(v[2].id.peer, 1);
        assert_eq!(v[2].id.counter, 2 as Counter);
        assert_eq!(v[2].id.lamport, 2 as Lamport);
    }
}
