// TODO: refactor, extract common code with text
use std::{
    ops::Range,
    sync::{Arc, Mutex},
};

use rle::{
    rle_tree::{tree_trait::CumulateTreeTrait, HeapMode},
    HasLength, RleTree, Sliceable,
};
use smallvec::{smallvec, SmallVec};

use crate::{
    container::{
        list::list_op::ListOp,
        registry::{ContainerInstance, ContainerWrapper},
        text::{
            text_content::{ListSlice, SliceRange},
            tracker::{Effect, Tracker},
        },
        Container, ContainerID, ContainerType,
    },
    context::Context,
    dag::DagUtils,
    debug_log,
    id::{Counter, ID},
    op::{Content, Op, RemoteOp, RichOp},
    span::{HasCounterSpan, HasId, HasIdSpan, IdSpan},
    value::LoroValue,
    version::IdSpanVector,
    LogStore,
};

#[derive(Debug)]
pub struct ListContainer {
    id: ContainerID,
    state: RleTree<SliceRange, CumulateTreeTrait<SliceRange, 8, HeapMode>>,
    raw_data: Pool,
    tracker: Tracker,
    head: SmallVec<[ID; 2]>,
}

#[derive(Debug, Default)]
struct Pool(Vec<LoroValue>);

impl Pool {
    #[inline(always)]
    pub fn alloc<V: Into<LoroValue>>(&mut self, s: V) -> Range<u32> {
        self.0.push(s.into());
        (self.0.len() - 1) as u32..self.0.len() as u32
    }

    #[inline(always)]
    pub fn alloc_arr(&mut self, values: Vec<LoroValue>) -> Range<u32> {
        let start = self.0.len() as u32;
        for v in values {
            self.0.push(v);
        }
        start..self.0.len() as u32
    }

    #[inline(always)]
    pub fn slice(&self, range: &Range<u32>) -> &[LoroValue] {
        &self.0[range.start as usize..range.end as usize]
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl ListContainer {
    pub(crate) fn new(id: ContainerID) -> Self {
        Self {
            id,
            raw_data: Pool::default(),
            tracker: Tracker::new(Default::default(), 0),
            state: Default::default(),
            // TODO: should be eq to log_store frontier?
            head: Default::default(),
        }
    }

    pub fn insert_batch<C: Context>(&mut self, ctx: &C, pos: usize, values: Vec<LoroValue>) {
        if values.is_empty() {
            return;
        }

        let store = ctx.log_store();
        let mut store = store.write().unwrap();
        let id = store.next_id();
        let slice = self.raw_data.alloc_arr(values);
        self.state.insert(pos, slice.clone().into());
        let op = Op::new(
            id,
            Content::List(ListOp::Insert {
                slice: slice.into(),
                pos,
            }),
            store.get_or_create_container_idx(&self.id),
        );
        let last_id = ID::new(
            store.this_client_id,
            op.counter + op.atom_len() as Counter - 1,
        );
        store.append_local_ops(&[op]);
        self.head = smallvec![last_id];
    }

    pub fn insert<C: Context, V: Into<LoroValue>>(
        &mut self,
        ctx: &C,
        pos: usize,
        value: V,
    ) -> Option<ID> {
        let store = ctx.log_store();
        let mut store = store.write().unwrap();
        let id = store.next_id();
        let slice = self.raw_data.alloc(value);
        self.state.insert(pos, slice.clone().into());
        let op = Op::new(
            id,
            Content::List(ListOp::Insert {
                slice: slice.into(),
                pos,
            }),
            store.get_or_create_container_idx(&self.id),
        );
        let last_id = ID::new(
            store.this_client_id,
            op.counter + op.atom_len() as Counter - 1,
        );
        store.append_local_ops(&[op]);
        self.head = smallvec![last_id];

        Some(id)
    }

    pub fn delete<C: Context>(&mut self, ctx: &C, pos: usize, len: usize) -> Option<ID> {
        if len == 0 {
            return None;
        }

        if self.state.len() < pos + len {
            panic!("deletion out of range");
        }

        let store = ctx.log_store();
        let mut store = store.write().unwrap();
        let id = store.next_id();
        let op = Op::new(
            id,
            Content::List(ListOp::new_del(pos, len)),
            store.get_or_create_container_idx(&self.id),
        );

        let last_id = ID::new(store.this_client_id, op.ctr_last());
        store.append_local_ops(&[op]);
        self.state.delete_range(Some(pos), Some(pos + len));
        self.head = smallvec![last_id];
        Some(id)
    }

    pub fn insert_obj<C: Context>(
        &mut self,
        ctx: &C,
        pos: usize,
        obj: ContainerType,
    ) -> ContainerID {
        let m = ctx.log_store();
        let mut store = m.write().unwrap();
        let container_id = store.create_container(obj, self.id.clone());
        // TODO: we can avoid this lock
        drop(store);
        self.insert(
            ctx,
            pos,
            LoroValue::Unresolved(Box::new(container_id.clone())),
        );
        container_id
    }

    pub fn values_len(&self) -> usize {
        self.state.len()
    }

    pub fn check(&mut self) {
        self.tracker.check();
    }

    #[cfg(feature = "test_utils")]
    pub fn debug_inspect(&mut self) {
        println!(
            "Text Container {:?}, Raw String size={}, Tree=>\n",
            self.id,
            self.raw_data.len(),
        );
        self.state.debug_inspect();
    }
}

impl Container for ListContainer {
    fn id(&self) -> &ContainerID {
        &self.id
    }

    fn type_(&self) -> ContainerType {
        ContainerType::Text
    }

    // TODO: move main logic to tracker module
    fn apply(&mut self, id_span: IdSpan, store: &LogStore) {
        debug_log!("APPLY ENTRY client={}", store.this_client_id);
        let self_idx = store.get_container_idx(&self.id).unwrap();
        let new_op_id = id_span.id_last();
        // TODO: may reduce following two into one op
        let common_ancestors = store.find_common_ancestor(&[new_op_id], &self.head);
        let vv = store.get_vv();
        if common_ancestors == self.head {
            let latest_head = smallvec![new_op_id];
            let path = store.find_path(&self.head, &latest_head);
            if path.right.len() == 1 {
                // linear updates, we can apply them directly
                let start = vv.get(&new_op_id.client_id).copied().unwrap_or(0);
                for op in store.iter_ops_at_id_span(
                    IdSpan::new(new_op_id.client_id, start, new_op_id.counter + 1),
                    self.id.clone(),
                ) {
                    let op = op.get_sliced();
                    debug_log!("APPLY {:?}", &op);
                    match &op.content {
                        Content::List(op) => match op {
                            ListOp::Insert { slice, pos } => {
                                self.state.insert(*pos, slice.as_slice().unwrap().clone())
                            }
                            ListOp::Delete(span) => self.state.delete_range(
                                Some(span.start() as usize),
                                Some(span.end() as usize),
                            ),
                        },
                        Content::Container(_) => {}
                        _ => unreachable!(),
                    }
                }

                self.head = latest_head;
                return;
            } else {
                let path: Vec<_> = store.iter_causal(&self.head, path.right).collect();
                if path
                    .iter()
                    .all(|x| x.forward.is_empty() && x.retreat.is_empty())
                {
                    // if we don't need to retreat or forward, we can update the state directly
                    for iter in path {
                        let change = iter
                            .data
                            .slice(iter.slice.start as usize, iter.slice.end as usize);
                        for op in change.ops.iter() {
                            if op.container == self_idx {
                                debug_log!("APPLY 1 {:?}", &op);
                                match &op.content {
                                    Content::List(op) => match op {
                                        ListOp::Insert { slice, pos } => self
                                            .state
                                            .insert(*pos, slice.as_slice().unwrap().clone()),
                                        ListOp::Delete(span) => self.state.delete_range(
                                            Some(span.start() as usize),
                                            Some(span.end() as usize),
                                        ),
                                    },
                                    Content::Container(_) => {}
                                    _ => unreachable!(),
                                }
                            }
                        }
                    }

                    self.head = latest_head;
                    return;
                }
            }
        }

        let path_to_head = store.find_path(&common_ancestors, &self.head);
        let mut common_ancestors_vv = vv.clone();
        common_ancestors_vv.retreat(&path_to_head.right);
        let mut latest_head: SmallVec<[ID; 2]> = self.head.clone();
        latest_head.retain(|x| !common_ancestors_vv.includes_id(*x));
        latest_head.push(new_op_id);
        // println!("{}", store.mermaid());
        debug_log!(
            "START FROM HEADS={:?} new_op_id={} self.head={:?}",
            &common_ancestors,
            new_op_id,
            &self.head
        );

        let tracker_head = if (common_ancestors.is_empty() && !self.tracker.start_vv().is_empty())
            || !common_ancestors.iter().all(|x| self.tracker.contains(*x))
        {
            debug_log!("NewTracker");
            self.tracker = Tracker::new(common_ancestors_vv, Counter::MAX / 2);
            common_ancestors
        } else {
            debug_log!("OldTracker");
            self.tracker.checkout_to_latest();
            self.tracker.all_vv().get_frontiers()
        };

        // stage 1
        let path = store.find_path(&tracker_head, &latest_head);
        debug_log!("path={:?}", &path);
        for iter in store.iter_causal(&tracker_head, path.right) {
            // TODO: avoid this clone
            let change = iter
                .data
                .slice(iter.slice.start as usize, iter.slice.end as usize);
            debug_log!(
                "Stage1 retreat:{} forward:{}\n{}",
                format!("{:?}", &iter.retreat).red(),
                format!("{:?}", &iter.forward).red(),
                format!("{:#?}", &change).blue(),
            );
            self.tracker.retreat(&iter.retreat);
            self.tracker.forward(&iter.forward);
            for op in change.ops.iter() {
                if op.container == self_idx && op.content.as_list().is_some() {
                    // TODO: convert op to local
                    self.tracker.apply(
                        ID {
                            client_id: change.id.client_id,
                            counter: op.counter,
                        },
                        &op.content,
                    )
                }
            }
        }

        // stage 2
        // TODO: reduce computations
        let path = store.find_path(&self.head, &latest_head);
        debug_log!("BEFORE CHECKOUT");
        self.tracker.checkout(vv);
        debug_log!("AFTER CHECKOUT");
        debug_log!(
            "[Stage 2]: Iterate path: {} from {} => {}",
            format!("{:?}", path.right).red(),
            format!("{:?}", self.head).red(),
            format!("{:?}", latest_head).red(),
        );
        debug_log!(
            "BEFORE EFFECT STATE={:?}",
            self.get_value().as_list().unwrap()
        );
        for effect in self.tracker.iter_effects(&path.right) {
            debug_log!("EFFECT: {:?}", &effect);
            match effect {
                Effect::Del { pos, len } => self.state.delete_range(Some(pos), Some(pos + len)),
                Effect::Ins { pos, content } => {
                    self.state.insert(pos, content.as_slice().unwrap().clone());
                }
            }
            debug_log!("AFTER EFFECT");
        }
        debug_log!(
            "AFTER EFFECT STATE={:?}",
            self.get_value().as_list().unwrap()
        );

        self.head = latest_head;
        debug_log!("--------------------------------");
    }

    fn tracker_checkout(&mut self, _vv: &crate::VersionVector) {
        todo!()
    }

    // TODO: maybe we need to let this return Cow
    fn get_value(&self) -> LoroValue {
        let mut values = Vec::new();
        for range in self.state.iter() {
            let content = range.as_ref();
            for value in self.raw_data.slice(&content.0) {
                values.push(value.clone());
            }
        }

        values.into()
    }

    fn to_export(&mut self, op: &mut RemoteOp, _gc: bool) {
        for content in op.contents.iter_mut() {
            if let Some((slice, _pos)) = content.as_list_mut().and_then(|x| x.as_insert_mut()) {
                if let Some(change) = if let ListSlice::Slice(ranges) = slice {
                    Some(self.raw_data.slice(&ranges.0))
                } else {
                    None
                } {
                    *slice = ListSlice::RawData(change.to_vec());
                }
            }
        }
    }

    fn to_import(&mut self, op: &mut RemoteOp) {
        for content in op.contents.iter_mut() {
            if let Some((slice, _pos)) = content.as_list_mut().and_then(|x| x.as_insert_mut()) {
                if let Some(slice_range) = match std::mem::take(slice) {
                    ListSlice::RawData(data) => Some(self.raw_data.alloc_arr(data)),
                    _ => unreachable!(),
                } {
                    *slice = slice_range.into();
                }
            }
        }
    }

    fn update_state_directly(&mut self, op: &RichOp) {
        match &op.get_sliced().content {
            Content::List(op) => match op {
                ListOp::Insert { slice, pos } => {
                    self.state.insert(*pos, slice.as_slice().unwrap().clone())
                }
                ListOp::Delete(span) => self
                    .state
                    .delete_range(Some(span.start() as usize), Some(span.end() as usize)),
            },
            Content::Container(_) => {}
            _ => unreachable!(),
        }
    }

    fn track_retreat(&mut self, spans: &IdSpanVector) {
        self.tracker.retreat(spans);
    }

    fn track_forward(&mut self, spans: &IdSpanVector) {
        self.tracker.forward(spans);
    }

    fn track_apply(&mut self, rich_op: &RichOp) {
        let content = rich_op.get_sliced().content;
        let id = rich_op.id_start();
        if self
            .tracker
            .all_vv()
            .includes_id(id.inc(content.atom_len() as Counter - 1))
        {
            self.tracker
                .forward(&id.to_span(content.atom_len()).to_id_span_vec());
            return;
        }

        if self.tracker.all_vv().includes_id(id) {
            let this_ctr = self.tracker.all_vv().get(&id.client_id).unwrap();
            let shift = this_ctr - id.counter;
            self.tracker
                .forward(&id.to_span(shift as usize).to_id_span_vec());
            self.tracker.apply(
                id.inc(shift),
                &content.slice(shift as usize, content.atom_len()),
            );
        } else {
            self.tracker.apply(id, &content)
        }
    }

    fn apply_tracked_effects_from(
        &mut self,
        from: &crate::VersionVector,
        effect_spans: &IdSpanVector,
    ) {
        self.tracker.checkout(from);
        for effect in self.tracker.iter_effects(effect_spans) {
            match effect {
                Effect::Del { pos, len } => self.state.delete_range(Some(pos), Some(pos + len)),
                Effect::Ins { pos, content } => {
                    let v = match content {
                        ListSlice::Slice(slice) => slice.clone(),
                        ListSlice::Unknown(u) => ListSlice::unknown_range(u),
                        _ => unreachable!(),
                    };

                    self.state.insert(pos, v)
                }
            }
        }
    }
}

pub struct List {
    instance: Arc<Mutex<ContainerInstance>>,
}

impl Clone for List {
    fn clone(&self) -> Self {
        Self {
            instance: Arc::clone(&self.instance),
        }
    }
}

impl List {
    pub fn insert_batch<C: Context>(&mut self, ctx: &C, pos: usize, values: Vec<LoroValue>) {
        self.with_container(|x| x.insert_batch(ctx, pos, values))
    }

    pub fn insert<C: Context, V: Into<LoroValue>>(
        &mut self,
        ctx: &C,
        pos: usize,
        value: V,
    ) -> Option<ID> {
        self.with_container(|x| x.insert(ctx, pos, value))
    }

    pub fn insert_obj<C: Context>(
        &mut self,
        ctx: &C,
        pos: usize,
        obj: ContainerType,
    ) -> ContainerID {
        self.with_container(|x| x.insert_obj(ctx, pos, obj))
    }

    pub fn delete<C: Context>(&mut self, ctx: &C, pos: usize, len: usize) -> Option<ID> {
        self.with_container(|text| text.delete(ctx, pos, len))
    }

    pub fn values_len(&self) -> usize {
        self.with_container(|text| text.values_len())
    }
}

impl ContainerWrapper for List {
    type Container = ListContainer;

    fn with_container<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Self::Container) -> R,
    {
        let mut container_instance = self.instance.lock().unwrap();
        let list = container_instance.as_list_mut().unwrap();
        f(list)
    }
}

impl From<Arc<Mutex<ContainerInstance>>> for List {
    fn from(text: Arc<Mutex<ContainerInstance>>) -> Self {
        List { instance: text }
    }
}
