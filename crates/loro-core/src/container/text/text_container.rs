use rle::{RleTree, Sliceable};
use smallvec::{smallvec, SmallVec};

use crate::{
    container::{list::list_op::ListOp, Container, ContainerID, ContainerType},
    dag::DagUtils,
    id::ID,
    log_store::LogStoreWeakRef,
    op::{InsertContent, Op, OpContent},
    smstring::SmString,
    span::{HasIdSpan, IdSpan},
    value::LoroValue,
    LogStore, VersionVector,
};

use super::{
    string_pool::StringPool,
    text_content::{ListSlice, ListSliceTreeTrait},
    tracker::{Effect, Tracker},
};

#[derive(Clone, Debug)]
struct DagNode {
    id: IdSpan,
    deps: SmallVec<[ID; 2]>,
}

#[derive(Debug)]
pub struct TextContainer {
    id: ContainerID,
    log_store: LogStoreWeakRef,
    state: RleTree<ListSlice, ListSliceTreeTrait>,
    raw_str: StringPool,
    tracker: Tracker,
    state_cache: LoroValue,

    head: SmallVec<[ID; 2]>,
    vv: VersionVector,
}

impl TextContainer {
    pub fn new(id: ContainerID, log_store: LogStoreWeakRef) -> Self {
        Self {
            id,
            log_store,
            raw_str: StringPool::default(),
            tracker: Tracker::new(Default::default()),
            state_cache: LoroValue::Null,
            state: Default::default(),
            // TODO: should be eq to log_store frontier?
            head: Default::default(),
            vv: Default::default(),
        }
    }

    pub fn insert(&mut self, pos: usize, text: &str) -> Option<ID> {
        let id = if let Ok(mut store) = self.log_store.upgrade().unwrap().write() {
            let id = store.next_id();
            // let slice = ListSlice::from_range(self.raw_str.alloc(text));
            let slice = ListSlice::from_raw(SmString::from(text));
            self.state.insert(pos, slice.clone());
            let op = Op::new(
                id,
                OpContent::Normal {
                    content: InsertContent::List(ListOp::Insert { slice, pos }),
                },
                self.id.clone(),
            );
            let last_id = op.id_last();
            store.append_local_ops(vec![op]);
            self.head = smallvec![last_id];
            self.vv.set_last(last_id);
            id
        } else {
            unimplemented!()
        };

        Some(id)
    }

    pub fn delete(&mut self, pos: usize, len: usize) -> Option<ID> {
        let id = if let Ok(mut store) = self.log_store.upgrade().unwrap().write() {
            let id = store.next_id();
            let op = Op::new(
                id,
                OpContent::Normal {
                    content: InsertContent::List(ListOp::Delete { len, pos }),
                },
                self.id.clone(),
            );

            let last_id = op.id_last();
            store.append_local_ops(vec![op]);
            self.state.delete_range(Some(pos), Some(pos + len));
            self.head = smallvec![last_id];
            self.vv.set_last(last_id);
            id
        } else {
            unimplemented!()
        };

        Some(id)
    }

    pub fn text_len(&self) -> usize {
        self.state.len()
    }

    pub fn check(&mut self) {
        self.tracker.check();
    }
}

impl Container for TextContainer {
    fn id(&self) -> &ContainerID {
        &self.id
    }

    fn type_(&self) -> ContainerType {
        ContainerType::Text
    }

    // TODO: move main logic to tracker module
    // TODO: we don't need op proxy, only ids are enough
    fn apply(&mut self, id_span: IdSpan, store: &LogStore) {
        let new_op_id = id_span.id_last();
        // TODO: may reduce following two into one op
        let common_ancestors = store.find_common_ancestor(&[new_op_id], &self.head);
        let path_to_head = store.find_path(&common_ancestors, &self.head);
        let mut ancestors_vv = self.vv.clone();
        ancestors_vv.retreat(&path_to_head.right);
        let mut latest_head: SmallVec<[ID; 2]> = self.head.clone();
        latest_head.push(new_op_id);
        if common_ancestors.is_empty()
            || !common_ancestors.iter().all(|x| self.tracker.contains(*x))
        {
            self.tracker = Tracker::new(ancestors_vv);
        } else {
            self.tracker.checkout(&self.vv);
        }

        // stage 1
        let path = store.find_path(&common_ancestors, &latest_head);
        for iter in store.iter_partial(&common_ancestors, path.right) {
            self.tracker.retreat(&iter.retreat);
            self.tracker.forward(&iter.forward);
            // TODO: avoid this clone
            let change = iter
                .data
                .slice(iter.slice.start as usize, iter.slice.end as usize);
            for op in change.ops.iter() {
                if op.container == self.id {
                    // TODO: convert op to local
                    self.tracker.apply(op.id, &op.content)
                }
            }
        }

        // stage 2
        let path = store.find_path(&latest_head, &self.head);
        self.tracker.retreat(&path.left);
        // println!(
        //     "Iterate path: {:?} from {:?} => {:?}",
        //     path.left, &self.head, &latest_head
        // );
        for effect in self.tracker.iter_effects(path.left) {
            // println!("{:?}", &effect);
            match effect {
                Effect::Del { pos, len } => self.state.delete_range(Some(pos), Some(pos + len)),
                Effect::Ins { pos, content } => {
                    self.state.insert(pos, content);
                }
            }
        }

        self.head.push(new_op_id);
        self.vv.set_last(new_op_id);
        // println!("------------------------------------------------------------------------");
    }

    fn checkout_version(&mut self, _vv: &crate::VersionVector) {
        todo!()
    }

    fn get_value(&mut self) -> &LoroValue {
        let mut ans_str = SmString::new();
        for v in self.state.iter() {
            let content = v.as_ref();
            match content {
                ListSlice::Slice(range) => ans_str.push_str(&self.raw_str.get_str(range)),
                ListSlice::RawStr(raw) => ans_str.push_str(raw),
                _ => unreachable!(),
            }
        }

        self.state_cache = LoroValue::String(ans_str);
        &self.state_cache
    }

    fn to_export(&self, op: &mut Op) {
        if let Some((slice, _pos)) = op
            .content
            .as_normal_mut()
            .and_then(|c| c.as_list_mut())
            .and_then(|x| x.as_insert_mut())
        {
            let change = if let ListSlice::Slice(ranges) = slice {
                Some(self.raw_str.get_str(ranges))
            } else {
                None
            };

            if let Some(change) = change {
                *slice = ListSlice::RawStr(change);
            }
        }
    }
}
