use loro_common::{HasCounter, HasCounterSpan, IdSpanVector};
use smallvec::smallvec;
use std::{
    cmp::Ordering,
    ops::{Deref, DerefMut},
};

use fxhash::{FxHashMap, FxHashSet};

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::{
    change::Lamport,
    id::{Counter, ID},
    oplog::AppDag,
    span::{CounterSpan, IdSpan},
    LoroError, PeerID,
};

/// [VersionVector](https://en.wikipedia.org/wiki/Version_vector)
/// is a map from [PeerID] to [Counter]. Its a right-open interval.
///
/// i.e. a [VersionVector] of `{A: 1, B: 2}` means that A has 1 atomic op and B has 2 atomic ops,
/// thus ID of `{client: A, counter: 1}` is out of the range.
#[repr(transparent)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionVector(FxHashMap<PeerID, Counter>);

/// Immutable version vector
///
/// It has O(1) clone time and O(logN) insert/delete/lookup time.
///
/// It's more memory efficient than [VersionVector] when the version vector
/// can be created from cloning and modifying other similar version vectors.
#[repr(transparent)]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImVersionVector(im::HashMap<PeerID, Counter, fxhash::FxBuildHasher>);

impl ImVersionVector {
    pub fn new() -> Self {
        Self(Default::default())
    }

    pub fn clear(&mut self) {
        self.0.clear()
    }

    pub fn get(&self, key: &PeerID) -> Option<&Counter> {
        self.0.get(key)
    }

    pub fn get_mut(&mut self, key: &PeerID) -> Option<&mut Counter> {
        self.0.get_mut(key)
    }

    pub fn insert(&mut self, k: PeerID, v: Counter) {
        self.0.insert(k, v);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> im::hashmap::Iter<'_, PeerID, Counter> {
        self.0.iter()
    }

    pub fn remove(&mut self, k: &PeerID) -> Option<Counter> {
        self.0.remove(k)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn contains_key(&self, k: &PeerID) -> bool {
        self.0.contains_key(k)
    }

    /// Convert to a [Frontiers]
    ///
    /// # Panic
    ///
    /// When self is greater than dag.vv
    pub fn to_frontiers(&self, dag: &AppDag) -> Frontiers {
        dag.im_vv_to_frontiers(self)
    }

    pub fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).unwrap()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, LoroError> {
        let vv = VersionVector::decode(bytes)?;
        Ok(Self::from_vv(&vv))
    }

    pub fn to_vv(&self) -> VersionVector {
        VersionVector(self.0.iter().map(|(&k, &v)| (k, v)).collect())
    }

    pub fn from_vv(vv: &VersionVector) -> Self {
        ImVersionVector(vv.0.iter().map(|(&k, &v)| (k, v)).collect())
    }

    pub fn extend_to_include_vv<'a>(
        &mut self,
        vv: impl Iterator<Item = (&'a PeerID, &'a Counter)>,
    ) {
        for (&client_id, &counter) in vv {
            if let Some(my_counter) = self.0.get_mut(&client_id) {
                if *my_counter < counter {
                    *my_counter = counter;
                }
            } else {
                self.0.insert(client_id, counter);
            }
        }
    }

    #[inline]
    pub fn merge(&mut self, other: &Self) {
        self.extend_to_include_vv(other.0.iter());
    }

    #[inline]
    pub fn merge_vv(&mut self, other: &VersionVector) {
        self.extend_to_include_vv(other.0.iter());
    }

    #[inline]
    pub fn set_last(&mut self, id: ID) {
        self.0.insert(id.peer, id.counter + 1);
    }

    pub fn extend_to_include_last_id(&mut self, id: ID) {
        if let Some(counter) = self.0.get_mut(&id.peer) {
            if *counter <= id.counter {
                *counter = id.counter + 1;
            }
        } else {
            self.set_last(id)
        }
    }

    pub(crate) fn includes_id(&self, x: ID) -> bool {
        if self.is_empty() {
            return false;
        }

        self.get(&x.peer).copied().unwrap_or(0) > x.counter
    }
}

// TODO: use a better data structure that is Array when small
// and hashmap when it's big
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Frontiers(SmallVec<[ID; 1]>);

impl PartialEq for Frontiers {
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() {
            return false;
        }

        if self.len() <= 1 {
            self.0 == other.0
        } else if self.len() <= 10 {
            self.0.iter().all(|id| other.0.contains(id))
        } else {
            let set = self.0.iter().collect::<FxHashSet<_>>();
            other.iter().all(|x| set.contains(x))
        }
    }
}

impl Frontiers {
    #[inline]
    pub fn from_id(id: ID) -> Self {
        Self(smallvec![id])
    }

    #[inline]
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(&self).unwrap()
    }

    #[inline]
    pub fn decode(bytes: &[u8]) -> Result<Self, LoroError> {
        postcard::from_bytes(bytes).map_err(|_| {
            LoroError::DecodeError("Decode Frontiers error".to_string().into_boxed_str())
        })
    }

    pub fn retain_non_included(&mut self, other: &Frontiers) {
        self.retain(|id| !other.contains(id));
    }

    pub fn update_frontiers_on_new_change(&mut self, id: ID, deps: &Frontiers) {
        self.retain(|existing_id| {
            if existing_id.peer == id.peer {
                assert!(id.counter > existing_id.counter);
                return false;
            }

            let any_same = deps.iter().any(|dep| {
                if dep.peer == existing_id.peer {
                    assert!(
                        dep.counter <= existing_id.counter,
                        "Causal dependency violated"
                    );

                    dep.counter == existing_id.counter
                } else {
                    false
                }
            });

            !any_same
        });

        self.push(id);
    }

    pub fn filter_peer(&mut self, peer: PeerID) {
        self.retain(|id| id.peer != peer);
    }

    #[inline]
    pub(crate) fn with_capacity(cap: usize) -> Frontiers {
        Self(SmallVec::with_capacity(cap))
    }

    #[allow(unused)]
    pub(crate) fn merge_frontiers(&mut self, new_frontiers: &Frontiers) {
        if self.len() <= 1 {
            if self == new_frontiers {
                return;
            }

            if new_frontiers.len() == 0 {
                return;
            }

            if self.len() == 0 {
                *self = new_frontiers.clone();
                return;
            }

            if new_frontiers.len() == 1 {
                let new_id = new_frontiers[0];
                if self[0].peer == new_id.peer {
                    if self[0].counter < new_id.counter {
                        self[0].counter = new_id.counter;
                    } else {
                        return;
                    }
                } else {
                    self.push(new_id);
                    return;
                }
            }
        }

        let mut map = self
            .0
            .iter()
            .map(|id| (id.peer, id.counter))
            .collect::<FxHashMap<_, _>>();

        for id in new_frontiers.0.iter() {
            if let Some(counter) = map.get_mut(&id.peer) {
                if *counter < id.counter {
                    *counter = id.counter;
                }
            } else {
                self.0.push(*id);
            }
        }

        self.0 = map
            .into_iter()
            .map(|(peer, counter)| ID::new(peer, counter))
            .collect();
    }
}

impl Deref for Frontiers {
    type Target = SmallVec<[ID; 1]>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Frontiers {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<SmallVec<[ID; 1]>> for Frontiers {
    fn from(value: SmallVec<[ID; 1]>) -> Self {
        Self(value)
    }
}

impl From<&[ID]> for Frontiers {
    fn from(value: &[ID]) -> Self {
        Self(value.into())
    }
}

impl<const N: usize> From<[ID; N]> for Frontiers {
    fn from(value: [ID; N]) -> Self {
        Self(value.as_slice().into())
    }
}

impl From<ID> for Frontiers {
    fn from(value: ID) -> Self {
        Self([value].into())
    }
}

impl From<&Vec<ID>> for Frontiers {
    fn from(value: &Vec<ID>) -> Self {
        let ids: &[ID] = value;
        Self(ids.into())
    }
}

impl From<Vec<ID>> for Frontiers {
    fn from(value: Vec<ID>) -> Self {
        Self(value.into())
    }
}

impl FromIterator<ID> for Frontiers {
    fn from_iter<I: IntoIterator<Item = ID>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl PartialEq for VersionVector {
    fn eq(&self, other: &Self) -> bool {
        self.iter()
            .all(|(client, counter)| other.get(client).unwrap_or(&0) == counter)
            && other
                .iter()
                .all(|(client, counter)| self.get(client).unwrap_or(&0) == counter)
    }
}

impl Eq for VersionVector {}

impl PartialEq for ImVersionVector {
    fn eq(&self, other: &Self) -> bool {
        self.0
            .iter()
            .all(|(client, counter)| other.0.get(client).unwrap_or(&0) == counter)
            && other
                .0
                .iter()
                .all(|(client, counter)| self.0.get(client).unwrap_or(&0) == counter)
    }
}

impl Eq for ImVersionVector {}

impl Deref for VersionVector {
    type Target = FxHashMap<PeerID, Counter>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct VersionVectorDiff {
    /// need to add these spans to move from right to left
    pub left: IdSpanVector,
    /// need to add these spans to move from left to right
    pub right: IdSpanVector,
}

impl VersionVectorDiff {
    #[inline]
    pub fn merge_left(&mut self, span: IdSpan) {
        merge(&mut self.left, span);
    }

    #[inline]
    pub fn merge_right(&mut self, span: IdSpan) {
        merge(&mut self.right, span);
    }

    #[inline]
    pub fn subtract_start_left(&mut self, span: IdSpan) {
        subtract_start(&mut self.left, span);
    }

    #[inline]
    pub fn subtract_start_right(&mut self, span: IdSpan) {
        subtract_start(&mut self.right, span);
    }

    pub fn get_id_spans_left(&self) -> impl Iterator<Item = IdSpan> + '_ {
        self.left.iter().map(|(peer, span)| IdSpan {
            peer: *peer,
            counter: *span,
        })
    }

    pub fn get_id_spans_right(&self) -> impl Iterator<Item = IdSpan> + '_ {
        self.right.iter().map(|(peer, span)| IdSpan {
            peer: *peer,
            counter: *span,
        })
    }
}

fn subtract_start(m: &mut FxHashMap<PeerID, CounterSpan>, target: IdSpan) {
    if let Some(span) = m.get_mut(&target.peer) {
        if span.start < target.counter.end {
            span.start = target.counter.end;
        }
    }
}

fn merge(m: &mut FxHashMap<PeerID, CounterSpan>, mut target: IdSpan) {
    target.normalize_();
    if let Some(span) = m.get_mut(&target.peer) {
        span.start = span.start.min(target.counter.start);
        span.end = span.end.max(target.counter.end);
    } else {
        m.insert(target.peer, target.counter);
    }
}

impl PartialOrd for VersionVector {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        let mut self_greater = true;
        let mut other_greater = true;
        let mut eq = true;
        for (client_id, other_end) in other.iter() {
            if let Some(self_end) = self.get(client_id) {
                if self_end < other_end {
                    self_greater = false;
                    eq = false;
                }
                if self_end > other_end {
                    other_greater = false;
                    eq = false;
                }
            } else if *other_end > 0 {
                self_greater = false;
                eq = false;
            }
        }

        for (client_id, self_end) in self.iter() {
            if other.contains_key(client_id) {
                continue;
            } else if *self_end > 0 {
                other_greater = false;
                eq = false;
            }
        }

        if eq {
            Some(Ordering::Equal)
        } else if self_greater {
            Some(Ordering::Greater)
        } else if other_greater {
            Some(Ordering::Less)
        } else {
            None
        }
    }
}

impl PartialOrd for ImVersionVector {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        let mut self_greater = true;
        let mut other_greater = true;
        let mut eq = true;
        for (client_id, other_end) in other.iter() {
            if let Some(self_end) = self.get(client_id) {
                if self_end < other_end {
                    self_greater = false;
                    eq = false;
                }
                if self_end > other_end {
                    other_greater = false;
                    eq = false;
                }
            } else if *other_end > 0 {
                self_greater = false;
                eq = false;
            }
        }

        for (client_id, self_end) in self.iter() {
            if other.contains_key(client_id) {
                continue;
            } else if *self_end > 0 {
                other_greater = false;
                eq = false;
            }
        }

        if eq {
            Some(Ordering::Equal)
        } else if self_greater {
            Some(Ordering::Greater)
        } else if other_greater {
            Some(Ordering::Less)
        } else {
            None
        }
    }
}

impl DerefMut for VersionVector {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl VersionVector {
    pub fn diff(&self, rhs: &Self) -> VersionVectorDiff {
        let mut ans: VersionVectorDiff = Default::default();
        for (client_id, &counter) in self.iter() {
            if let Some(&rhs_counter) = rhs.get(client_id) {
                match counter.cmp(&rhs_counter) {
                    Ordering::Less => {
                        ans.right.insert(
                            *client_id,
                            CounterSpan {
                                start: counter,
                                end: rhs_counter,
                            },
                        );
                    }
                    Ordering::Greater => {
                        ans.left.insert(
                            *client_id,
                            CounterSpan {
                                start: rhs_counter,
                                end: counter,
                            },
                        );
                    }
                    Ordering::Equal => {}
                }
            } else {
                ans.left.insert(
                    *client_id,
                    CounterSpan {
                        start: 0,
                        end: counter,
                    },
                );
            }
        }
        for (client_id, &rhs_counter) in rhs.iter() {
            if !self.contains_key(client_id) {
                ans.right.insert(
                    *client_id,
                    CounterSpan {
                        start: 0,
                        end: rhs_counter,
                    },
                );
            }
        }

        ans
    }

    /// Returns two iterators that cover the differences between two version vectors.
    ///
    /// - The first iterator contains the spans that are in `self` but not in `rhs`
    /// - The second iterator contains the spans that are in `rhs` but not in `self`
    pub fn diff_iter<'a>(
        &'a self,
        rhs: &'a Self,
    ) -> (
        impl Iterator<Item = IdSpan> + 'a,
        impl Iterator<Item = IdSpan> + 'a,
    ) {
        (self.sub_iter(rhs), rhs.sub_iter(self))
    }

    /// Returns the spans that are in `self` but not in `rhs`
    pub fn sub_iter<'a>(&'a self, rhs: &'a Self) -> impl Iterator<Item = IdSpan> + 'a {
        self.iter().filter_map(move |(peer, &counter)| {
            if let Some(&rhs_counter) = rhs.get(peer) {
                if counter > rhs_counter {
                    Some(IdSpan {
                        peer: *peer,
                        counter: CounterSpan {
                            start: rhs_counter,
                            end: counter,
                        },
                    })
                } else {
                    None
                }
            } else if counter > 0 {
                Some(IdSpan {
                    peer: *peer,
                    counter: CounterSpan {
                        start: 0,
                        end: counter,
                    },
                })
            } else {
                None
            }
        })
    }

    /// Returns the spans that are in `self` but not in `rhs`
    pub fn sub_iter_im<'a>(
        &'a self,
        rhs: &'a ImVersionVector,
    ) -> impl Iterator<Item = IdSpan> + 'a {
        self.iter().filter_map(move |(peer, &counter)| {
            if let Some(&rhs_counter) = rhs.get(peer) {
                if counter > rhs_counter {
                    Some(IdSpan {
                        peer: *peer,
                        counter: CounterSpan {
                            start: rhs_counter,
                            end: counter,
                        },
                    })
                } else {
                    None
                }
            } else if counter > 0 {
                Some(IdSpan {
                    peer: *peer,
                    counter: CounterSpan {
                        start: 0,
                        end: counter,
                    },
                })
            } else {
                None
            }
        })
    }

    /// Iter all span from a -> b and b -> a
    pub fn iter_between<'a>(&'a self, other: &'a Self) -> impl Iterator<Item = IdSpan> + 'a {
        // PERF: can be optimized a little
        self.sub_iter(other).chain(other.sub_iter(self))
    }

    pub fn sub_vec(&self, rhs: &Self) -> IdSpanVector {
        self.sub_iter(rhs).map(|x| (x.peer, x.counter)).collect()
    }

    pub fn distance_between(&self, other: &Self) -> usize {
        let mut ans = 0;
        for (client_id, &counter) in self.iter() {
            if let Some(&other_counter) = other.get(client_id) {
                ans += (counter - other_counter).abs();
            } else if counter > 0 {
                ans += counter;
            }
        }

        for (client_id, &counter) in other.iter() {
            if !self.contains_key(client_id) {
                ans += counter;
            }
        }

        ans as usize
    }

    pub fn to_spans(&self) -> IdSpanVector {
        self.iter()
            .map(|(client_id, &counter)| {
                (
                    *client_id,
                    CounterSpan {
                        start: 0,
                        end: counter,
                    },
                )
            })
            .collect()
    }

    #[inline]
    pub fn get_frontiers(&self) -> Frontiers {
        self.iter()
            .filter_map(|(client_id, &counter)| {
                if counter > 0 {
                    Some(ID {
                        peer: *client_id,
                        counter: counter - 1,
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    #[inline]
    pub fn new() -> Self {
        Self(Default::default())
    }

    /// set the inclusive ending point. target id will be included by self
    #[inline]
    pub fn set_last(&mut self, id: ID) {
        self.0.insert(id.peer, id.counter + 1);
    }

    #[inline]
    pub fn get_last(&self, client_id: PeerID) -> Option<Counter> {
        self.0
            .get(&client_id)
            .and_then(|&x| if x == 0 { None } else { Some(x - 1) })
    }

    /// set the exclusive ending point. target id will NOT be included by self
    #[inline]
    pub fn set_end(&mut self, id: ID) {
        self.0.insert(id.peer, id.counter);
    }

    /// Update the end counter of the given client if the end is greater.
    /// Return whether updated
    #[inline]
    pub fn try_update_last(&mut self, id: ID) -> bool {
        if let Some(end) = self.0.get_mut(&id.peer) {
            if *end < id.counter + 1 {
                *end = id.counter + 1;
                true
            } else {
                false
            }
        } else {
            self.0.insert(id.peer, id.counter + 1);
            true
        }
    }

    pub fn get_missing_span(&self, target: &Self) -> Vec<IdSpan> {
        let mut ans = vec![];
        for (client_id, other_end) in target.iter() {
            if let Some(my_end) = self.get(client_id) {
                if my_end < other_end {
                    ans.push(IdSpan::new(*client_id, *my_end, *other_end));
                }
            } else {
                ans.push(IdSpan::new(*client_id, 0, *other_end));
            }
        }

        ans
    }

    pub fn merge(&mut self, other: &Self) {
        for (&client_id, &other_end) in other.iter() {
            if let Some(my_end) = self.get_mut(&client_id) {
                if *my_end < other_end {
                    *my_end = other_end;
                }
            } else {
                self.0.insert(client_id, other_end);
            }
        }
    }

    pub fn includes_vv(&self, other: &VersionVector) -> bool {
        match self.partial_cmp(other) {
            Some(ord) => match ord {
                Ordering::Less => false,
                Ordering::Equal => true,
                Ordering::Greater => true,
            },
            None => false,
        }
    }

    pub fn includes_id(&self, id: ID) -> bool {
        if let Some(end) = self.get(&id.peer) {
            if *end > id.counter {
                return true;
            }
        }
        false
    }

    pub fn intersect_span(&self, target: IdSpan) -> Option<CounterSpan> {
        if let Some(&end) = self.get(&target.peer) {
            if end > target.ctr_start() {
                let count_end = target.ctr_end();
                return Some(CounterSpan {
                    start: target.ctr_start(),
                    end: end.min(count_end),
                });
            }
        }

        None
    }

    pub fn extend_to_include_vv<'a>(
        &mut self,
        vv: impl Iterator<Item = (&'a PeerID, &'a Counter)>,
    ) {
        for (&client_id, &counter) in vv {
            if let Some(my_counter) = self.get_mut(&client_id) {
                if *my_counter < counter {
                    *my_counter = counter;
                }
            } else {
                self.0.insert(client_id, counter);
            }
        }
    }

    pub fn extend_to_include_last_id(&mut self, id: ID) {
        if let Some(counter) = self.get_mut(&id.peer) {
            if *counter <= id.counter {
                *counter = id.counter + 1;
            }
        } else {
            self.set_last(id)
        }
    }

    pub fn extend_to_include_end_id(&mut self, id: ID) {
        if let Some(counter) = self.get_mut(&id.peer) {
            if *counter < id.counter {
                *counter = id.counter;
            }
        } else {
            self.set_end(id)
        }
    }

    pub fn extend_to_include(&mut self, span: IdSpan) {
        if let Some(counter) = self.get_mut(&span.peer) {
            if *counter < span.counter.norm_end() {
                *counter = span.counter.norm_end();
            }
        } else {
            self.insert(span.peer, span.counter.norm_end());
        }
    }

    pub fn shrink_to_exclude(&mut self, span: IdSpan) {
        if span.counter.min() == 0 {
            self.remove(&span.peer);
            return;
        }

        if let Some(counter) = self.get_mut(&span.peer) {
            if *counter > span.counter.min() {
                *counter = span.counter.min();
            }
        }
    }

    pub fn forward(&mut self, spans: &IdSpanVector) {
        for span in spans.iter() {
            self.extend_to_include(IdSpan {
                peer: *span.0,
                counter: *span.1,
            });
        }
    }

    pub fn retreat(&mut self, spans: &IdSpanVector) {
        for span in spans.iter() {
            self.shrink_to_exclude(IdSpan {
                peer: *span.0,
                counter: *span.1,
            });
        }
    }

    pub fn intersection(&self, other: &VersionVector) -> VersionVector {
        let mut ans = VersionVector::new();
        for (client_id, &counter) in self.iter() {
            if let Some(&other_counter) = other.get(client_id) {
                if counter < other_counter {
                    if counter != 0 {
                        ans.insert(*client_id, counter);
                    }
                } else if other_counter != 0 {
                    ans.insert(*client_id, other_counter);
                }
            }
        }
        ans
    }

    #[inline(always)]
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).unwrap()
    }

    #[inline(always)]
    pub fn decode(bytes: &[u8]) -> Result<Self, LoroError> {
        postcard::from_bytes(bytes).map_err(|_| LoroError::DecodeVersionVectorError)
    }

    /// Convert to a [Frontiers]
    ///
    /// # Panic
    ///
    /// When self is greater than dag.vv
    pub fn to_frontiers(&self, dag: &AppDag) -> Frontiers {
        dag.vv_to_frontiers(self)
    }

    pub(crate) fn trim(&self, vv: &VersionVector) -> VersionVector {
        let mut ans = VersionVector::new();
        for (client_id, &counter) in self.iter() {
            if let Some(&other_counter) = vv.get(client_id) {
                ans.insert(*client_id, counter.min(other_counter));
            }
        }
        ans
    }

    pub fn to_im_vv(&self) -> ImVersionVector {
        ImVersionVector(self.0.iter().map(|(&k, &v)| (k, v)).collect())
    }

    pub fn from_im_vv(im_vv: &ImVersionVector) -> Self {
        VersionVector(im_vv.0.iter().map(|(&k, &v)| (k, v)).collect())
    }
}

/// Use minimal set of ids to represent the frontiers
pub fn shrink_frontiers(last_ids: &[ID], dag: &AppDag) -> Frontiers {
    // it only keep the ids of ops that are concurrent to each other

    let mut frontiers = Frontiers::default();
    let mut frontiers_vv = Vec::new();

    if last_ids.is_empty() {
        return frontiers;
    }

    if last_ids.len() == 1 {
        frontiers.push(last_ids[0]);
        return frontiers;
    }

    let mut last_ids = last_ids.to_vec();
    // sort by lamport, ascending
    last_ids.sort_by_cached_key(|x| ((dag.get_lamport(x).unwrap() as isize), x.peer));

    for id in last_ids.iter().rev() {
        let vv = dag.get_vv(*id).unwrap();
        let mut should_insert = true;
        for f_vv in frontiers_vv.iter() {
            if vv.partial_cmp(f_vv).is_some() {
                // This is not concurrent op, should be ignored in frontiers
                should_insert = false;
                break;
            }
        }

        if should_insert {
            frontiers.push(*id);
            frontiers_vv.push(vv);
        }
    }

    frontiers
}

impl Default for VersionVector {
    fn default() -> Self {
        Self::new()
    }
}

impl From<FxHashMap<PeerID, Counter>> for VersionVector {
    fn from(map: FxHashMap<PeerID, Counter>) -> Self {
        let mut im_map = FxHashMap::default();
        for (client_id, counter) in map {
            im_map.insert(client_id, counter);
        }
        Self(im_map)
    }
}

impl From<Vec<ID>> for VersionVector {
    fn from(vec: Vec<ID>) -> Self {
        let mut vv = VersionVector::new();
        for id in vec {
            vv.set_last(id);
        }

        vv
    }
}

impl FromIterator<ID> for VersionVector {
    fn from_iter<T: IntoIterator<Item = ID>>(iter: T) -> Self {
        let iter = iter.into_iter();
        let mut vv = VersionVector(FxHashMap::with_capacity_and_hasher(
            iter.size_hint().0,
            Default::default(),
        ));
        for id in iter {
            vv.set_last(id);
        }

        vv
    }
}

impl FromIterator<(PeerID, Counter)> for VersionVector {
    fn from_iter<T: IntoIterator<Item = (PeerID, Counter)>>(iter: T) -> Self {
        VersionVector(FxHashMap::from_iter(iter))
    }
}

// Note: It will be encoded into binary format, so the order of its fields should not be changed.
#[derive(Debug, PartialEq, Eq, Clone, Copy, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct TotalOrderStamp {
    pub(crate) lamport: Lamport,
    pub(crate) client_id: PeerID,
}

pub fn are_frontiers_eq(a: &[ID], b: &[ID]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    let mut a: SmallVec<[ID; 10]> = a.into();
    let mut b: SmallVec<[ID; 10]> = b.into();

    a.sort();
    b.sort();

    a == b
}

#[cfg(test)]
mod tests {
    #![allow(clippy::neg_cmp_op_on_partial_ord)]
    use super::*;
    mod cmp {
        use super::*;
        #[test]
        fn test() {
            let a: VersionVector = vec![ID::new(1, 1), ID::new(2, 2)].into();
            let b: VersionVector = vec![ID::new(1, 1), ID::new(2, 2)].into();
            assert_eq!(a.partial_cmp(&b), Some(Ordering::Equal));
            assert!(a == b);

            let a: VersionVector = vec![ID::new(1, 2), ID::new(2, 1)].into();
            let b: VersionVector = vec![ID::new(1, 1), ID::new(2, 2)].into();
            assert_eq!(a.partial_cmp(&b), None);

            assert!(!(a > b));
            assert!(!(b > a));
            assert!(!(b == a));

            let a: VersionVector = vec![ID::new(1, 2), ID::new(2, 3)].into();
            let b: VersionVector = vec![ID::new(1, 1), ID::new(2, 2)].into();
            assert_eq!(a.partial_cmp(&b), Some(Ordering::Greater));
            assert!(a > b);
            assert!(a >= b);

            let a: VersionVector = vec![ID::new(1, 0), ID::new(2, 2)].into();
            let b: VersionVector = vec![ID::new(1, 1), ID::new(2, 2)].into();
            assert_eq!(a.partial_cmp(&b), Some(Ordering::Less));
            assert!(a < b);
            assert!(a <= b);
        }
    }

    #[test]
    fn im() {
        let mut a = VersionVector::new();
        a.set_last(ID::new(1, 1));
        a.set_last(ID::new(2, 1));
        let mut b = a.clone();
        b.merge(&vec![ID::new(1, 2), ID::new(2, 2)].into());
        assert!(a != b);
        assert_eq!(a.get(&1), Some(&2));
        assert_eq!(a.get(&2), Some(&2));
        assert_eq!(b.get(&1), Some(&3));
        assert_eq!(b.get(&2), Some(&3));
    }

    #[test]
    fn field_order() {
        let tos = TotalOrderStamp {
            lamport: 0,
            client_id: 1,
        };
        let buf = vec![0, 1];
        assert_eq!(postcard::from_bytes::<TotalOrderStamp>(&buf).unwrap(), tos);
    }

    #[test]
    fn test_encode_decode_im_version_vector() {
        let vv = VersionVector::from_iter([(1, 1), (2, 2), (3, 3)]);
        let im_vv = vv.to_im_vv();
        let decoded_vv = VersionVector::from_im_vv(&im_vv);
        assert_eq!(vv, decoded_vv);
    }

    #[test]
    fn test_version_vector_encoding_decoding() {
        let mut vv = VersionVector::new();
        vv.insert(1, 10);
        vv.insert(2, 20);
        vv.insert(3, 30);

        // Encode VersionVector
        let encoded = vv.encode();

        // Decode to ImVersionVector
        let decoded_im_vv = ImVersionVector::decode(&encoded).unwrap();

        // Convert VersionVector to ImVersionVector for comparison
        let im_vv = vv.to_im_vv();

        // Compare the original ImVersionVector with the decoded one
        assert_eq!(im_vv, decoded_im_vv);

        // Convert back to VersionVector and compare
        let decoded_vv = VersionVector::from_im_vv(&decoded_im_vv);
        assert_eq!(vv, decoded_vv);
    }
}
