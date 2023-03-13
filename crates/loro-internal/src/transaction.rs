use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::{Arc, Mutex, RwLock, Weak},
};

use fxhash::{FxHashMap, FxHashSet};
use smallvec::smallvec;

use crate::{
    container::{
        registry::{ContainerIdx, ContainerInner},
        ContainerID, ContainerTrait,
    },
    event::{Diff, RawEvent},
    hierarchy::Hierarchy,
    id::ClientID,
    log_store::LoroEncoder,
    transaction::op::Value,
    version::Frontiers,
    ContainerType, LogStore, LoroCore, LoroError, LoroValue, Map,
};

use self::op::{ListTxnOps, MapTxnOps, TextTxnOps, TransactionOp};

pub(crate) mod op;

pub trait Transact {
    fn transact(&self) -> TransactionWrap;
}

impl Transact for LoroCore {
    fn transact(&self) -> TransactionWrap {
        TransactionWrap(Rc::new(RefCell::new(Transaction::new(
            Arc::downgrade(&self.log_store),
            Arc::downgrade(&self.hierarchy),
        ))))
    }
}

impl Transact for TransactionWrap {
    fn transact(&self) -> TransactionWrap {
        TransactionWrap(Rc::clone(&self.0))
    }
}

impl AsMut<Transaction> for Transaction {
    fn as_mut(&mut self) -> &mut Transaction {
        self
    }
}

pub struct TransactionWrap(pub(crate) Rc<RefCell<Transaction>>);

pub struct Transaction {
    pub(crate) client_id: ClientID,
    pub(crate) store: Weak<RwLock<LogStore>>,
    pub(crate) hierarchy: Weak<Mutex<Hierarchy>>,
    // sort by [ContainerIdx]
    pending_ops: BTreeMap<ContainerIdx, Vec<TransactionOp>>,
    compressed_op: Vec<TransactionOp>,
    created_container: FxHashMap<ContainerIdx, FxHashSet<ContainerIdx>>,
    deleted_container: FxHashSet<ContainerIdx>,
    pending_events: FxHashMap<ContainerID, RawEvent>,
    start_vv: Frontiers,
    latest_vv: Frontiers,
    committed: bool,
}

impl Transaction {
    pub(crate) fn new(store: Weak<RwLock<LogStore>>, hierarchy: Weak<Mutex<Hierarchy>>) -> Self {
        let (client_id, start_vv): (u64, Frontiers) = {
            let store = store.upgrade().unwrap();
            let store = store.try_read().unwrap();
            (store.this_client_id, store.frontiers().into())
        };
        Self {
            client_id,
            store,
            hierarchy,
            pending_ops: Default::default(),
            compressed_op: Default::default(),
            created_container: Default::default(),
            deleted_container: Default::default(),
            pending_events: Default::default(),
            latest_vv: start_vv.clone(),
            start_vv,
            committed: false,
        }
    }

    pub(crate) fn next_container_idx(&mut self) -> ContainerIdx {
        let store = self.store.upgrade().unwrap();
        let store = store.try_read().unwrap();
        store.next_container_idx()
    }

    pub(crate) fn get_container_inner(&self, idx: &ContainerIdx) -> ContainerInner {
        let store = self.store.upgrade().unwrap();
        let store = store.try_read().unwrap();
        let inner = store.get_container_by_idx(idx);
        if let Some(inner) = inner {
            ContainerInner::Instance(inner)
        } else {
            todo!()
        }
    }

    pub(crate) fn push(
        &mut self,
        op: TransactionOp,
        created_container: Option<ContainerIdx>,
    ) -> Result<(), LoroError> {
        if let Some(idx) = created_container {
            self.created_container
                .entry(op.container_idx())
                .or_insert_with(FxHashSet::default)
                .insert(idx);
        }
        self.pending_ops
            .entry(op.container_idx())
            .or_insert_with(Vec::new)
            .push(op);
        Ok(())
    }

    fn apply_ops_queue_event(&mut self, store: &mut LogStore, hierarchy: &mut Hierarchy) {
        let compressed_op = std::mem::take(&mut self.compressed_op);
        for op in compressed_op {
            let idx = op.container_idx();
            let type_ = op.container_type();
            let diff = smallvec![match type_ {
                ContainerType::List => {
                    Diff::List(op.as_list().unwrap().1.clone().into_event_format())
                }
                ContainerType::Map => {
                    let container = store.reg.get_by_idx(&idx).unwrap();
                    let map = Map::from_instance(container, store.this_client_id);
                    // we need lookup the container to know the diff is added or updated
                    Diff::Map(op.as_map().unwrap().1.clone().into_event_format(&map))
                }
                ContainerType::Text => Diff::Text(op.as_text().unwrap().1.clone()),
            }];
            let container = store.reg.get_by_idx(&idx).unwrap();
            let container = container.upgrade().unwrap();
            let mut container = container.try_lock().unwrap();
            let container_id = container.id().clone();
            let store_ops = container.apply_txn_op(store, op);
            drop(container);
            let (_, new_version) = store.append_local_ops(&store_ops);
            let new_version = new_version.into();
            // update latest vv
            let _version = std::mem::replace(&mut self.latest_vv, new_version);
            if let Some(event) = if hierarchy.should_notify(&container_id) {
                hierarchy
                    .get_abs_path(&store.reg, &container_id)
                    .map(|abs_path| RawEvent {
                        container_id: container_id.clone(),
                        old_version: self.start_vv.clone(),
                        new_version: _version,
                        diff,
                        local: true,
                        abs_path,
                    })
            } else {
                None
            } {
                self.append_event(event);
            }
        }
    }

    fn append_event(&mut self, event: RawEvent) {
        // cache events
        let container_id = &event.container_id;
        if let Some(old) = self.pending_events.get_mut(container_id) {
            compose_two_events(old, event);
        } else {
            self.pending_events.insert(container_id.clone(), event);
        }
    }

    fn emit_events(&mut self) {
        let pending_events = std::mem::take(&mut self.pending_events);
        for (_, mut event) in pending_events {
            event.new_version = self.latest_vv.clone();
            let hierarchy = self.hierarchy.upgrade().unwrap();
            Hierarchy::notify_without_lock(hierarchy, event);
        }
    }

    fn register_container(
        &mut self,
        idx: ContainerIdx,
        type_: ContainerType,
        parent_idx: ContainerIdx,
        s: &mut LogStore,
        h: &mut Hierarchy,
    ) -> ContainerID {
        let id = s.next_id();
        let mut container_id = ContainerID::new_normal(id, type_);

        while s.reg.contains(&container_id) {
            if let ContainerID::Normal { id, .. } = &mut container_id {
                id.counter += 1;
            }
        }

        let parent_id = s.reg.get_id(parent_idx).unwrap();
        h.add_child(parent_id, &container_id);

        s.reg.register_txn(idx, container_id.clone());
        container_id
    }

    fn compress_ops(&mut self, store: &mut LogStore, hierarchy: &mut Hierarchy) {
        let pending_ops = std::mem::take(&mut self.pending_ops);
        for (idx, mut ops) in pending_ops {
            if self.deleted_container.remove(&idx) {
                continue;
            }
            let type_ = ops.first().unwrap().container_type();
            match type_ {
                ContainerType::List => {
                    ops.iter_mut()
                        .for_each(|op| self.convert_op_container(op, store, hierarchy));
                }
                ContainerType::Map => {
                    ops.iter_mut()
                        .for_each(|op| self.convert_op_container(op, store, hierarchy));
                }
                ContainerType::Text => {}
            };
            self.compressed_op.push(TransactionOp::compose(ops));
            // The rest containers that are still in `created_container` have been deleted.
            if let Some(deleted_containers) = self.created_container.remove(&idx) {
                self.deleted_container.extend(deleted_containers);
            }
        }
    }

    fn convert_op_container(
        &mut self,
        op: &mut TransactionOp,
        store: &mut LogStore,
        hierarchy: &mut Hierarchy,
    ) {
        match op {
            TransactionOp::List { container, ops } => {
                // TODO: cache the containers?
                for item in ops.iter_mut() {
                    if let Some((value, _)) = item.as_insert_mut() {
                        for v in value {
                            if let Some((type_, idx)) = v.as_container() {
                                self.created_container
                                    .get_mut(container)
                                    .unwrap()
                                    .remove(idx);
                                let id = self
                                    .register_container(*idx, *type_, *container, store, hierarchy);
                                *v = Value::Value(LoroValue::Unresolved(id.into()));
                            }
                        }
                    }
                }
            }
            TransactionOp::Map { container, ops } => {
                for (_k, v) in ops.added.iter_mut() {
                    if let Some((type_, idx)) = v.as_container() {
                        self.created_container
                            .get_mut(container)
                            .unwrap()
                            .remove(idx);
                        let id =
                            self.register_container(*idx, *type_, *container, store, hierarchy);
                        *v = Value::Value(LoroValue::Unresolved(id.into()));
                    }
                }
            }
            _ => unreachable!(),
        };
    }

    /// For now, when we get value or decode apply, we will incrementally commit the pending ops to store but will not emit events.
    ///
    pub(crate) fn implicit_commit(&mut self) {
        // TODO: transaction commit
        // 1. compress op
        // 2. maybe rebase
        // 3. batch apply op
        // 4. aggregate event
        {
            let store = self.store.upgrade().unwrap();
            let mut store = store.try_write().unwrap();
            let hierarchy = self.hierarchy.upgrade().unwrap();
            let mut hierarchy = hierarchy.try_lock().unwrap();

            self.compress_ops(&mut store, &mut hierarchy);
            self.apply_ops_queue_event(&mut store, &mut hierarchy);
        }
    }

    pub fn decode(&mut self, input: &[u8]) -> Result<(), LoroError> {
        self.implicit_commit();
        let store = self.store.upgrade().unwrap();
        let mut store = store.try_write().unwrap();
        let hierarchy = self.hierarchy.upgrade().unwrap();
        let mut hierarchy = hierarchy.try_lock().unwrap();
        let events = LoroEncoder::decode(&mut store, &mut hierarchy, input)?;
        for event in events {
            self.append_event(event)
        }
        Ok(())
    }

    pub fn commit(&mut self) {
        if self.committed {
            return;
        }
        self.committed = true;
        self.implicit_commit();
        self.emit_events();
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        self.commit()
    }
}

fn compose_two_events(a: &mut RawEvent, mut b: RawEvent) {
    let this_diff = std::mem::take(&mut a.diff).pop().unwrap();
    let other_diff = std::mem::take(&mut b.diff).pop().unwrap();
    let diff = match other_diff {
        Diff::List(x) => Diff::List(this_diff.into_list().unwrap().compose(x)),
        Diff::Map(x) => Diff::Map(this_diff.into_map().unwrap().compose(x)),
        Diff::Text(x) => Diff::Text(this_diff.into_text().unwrap().compose(x)),
    };
    a.diff.push(diff);
}
