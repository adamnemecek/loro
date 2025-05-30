use std::ops::Range;

use append_only_bytes::BytesSlice;
use enum_as_inner::EnumAsInner;
use loro_common::{ContainerType, HasId, HasIdSpan, IdLp, LoroValue, ID};
use rle::{HasLength, Mergable, Sliceable};
use serde::{Deserialize, Serialize};

use crate::{
    container::richtext::TextStyleInfoFlag,
    op::{ListSlice, SliceRange},
    utils::string_slice::unicode_range_to_byte_range,
    InternalString,
};

/// `len` and `pos` is measured in unicode char for text.
// Note: It will be encoded into binary format, so the order of its fields should not be changed.
#[derive(EnumAsInner, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ListOp<'a> {
    Insert {
        slice: ListSlice<'a>,
        pos: usize,
    },
    Delete(DeleteSpanWithId),
    Move {
        from: u32,
        to: u32,
        elem_id: IdLp,
    },
    Set {
        elem_id: IdLp,
        value: LoroValue,
    },
    /// StyleStart and StyleEnd must be paired because the end of a style must take an OpID position.
    StyleStart {
        start: u32,
        end: u32,
        key: InternalString,
        info: TextStyleInfoFlag,
        value: LoroValue,
    },
    StyleEnd,
}

#[derive(EnumAsInner, Debug, Clone)]
pub enum InnerListOp {
    // TODO: this is only used for list now? We should rename it to InsertList
    Insert {
        slice: SliceRange,
        pos: usize,
    },
    // Note: len may not equal to slice.len() because for text len is unicode len while the slice
    // is utf8 bytes.
    InsertText {
        slice: BytesSlice,
        unicode_start: u32,
        unicode_len: u32,
        pos: u32,
    },
    Delete(DeleteSpanWithId),
    Move {
        from: u32,
        /// Element id
        elem_id: IdLp,
        to: u32,
    },
    Set {
        elem_id: IdLp,
        value: LoroValue,
    },
    /// StyleStart and StyleEnd must be paired.
    /// The next op of StyleStart must be StyleEnd.
    StyleStart {
        start: u32,
        end: u32,
        key: InternalString,
        value: LoroValue,
        info: TextStyleInfoFlag,
    },
    StyleEnd,
}

impl ListOp<'_> {
    pub fn new_del(id_start: ID, pos: usize, len: usize) -> Self {
        assert!(len != 0);
        Self::Delete(DeleteSpanWithId::new(id_start, pos as isize, len as isize))
    }
}

impl InnerListOp {
    pub fn new_del(id: ID, pos: usize, len: isize) -> Self {
        assert!(len != 0);
        Self::Delete(DeleteSpanWithId {
            id_start: id,
            span: DeleteSpan {
                pos: pos as isize,
                signed_len: len,
            },
        })
    }

    pub fn new_insert(slice: Range<u32>, pos: usize) -> Self {
        Self::Insert {
            slice: SliceRange(slice),
            pos,
        }
    }

    pub(crate) fn estimate_storage_size(&self, container_type: ContainerType) -> usize {
        match self {
            Self::Insert { slice, .. } => match container_type {
                ContainerType::MovableList | ContainerType::List => 4 * slice.atom_len(),
                ContainerType::Text => slice.atom_len(),
                _ => unreachable!(),
            },
            Self::InsertText { slice, .. } => slice.len(),
            Self::Delete(..) => 8,
            Self::Move { .. } => 8,
            Self::Set { .. } => 7,
            Self::StyleStart { .. } => 10,
            Self::StyleEnd => 1,
        }
    }
}

impl HasLength for DeleteSpan {
    fn content_len(&self) -> usize {
        self.signed_len.unsigned_abs()
    }
}

/// Delete span that the initial id is `id_start`.
///
/// This span may be reversed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSpanWithId {
    /// This is the target id with the smallest counter no matter whether the span is reversed.
    /// So it's always the id of the leftmost element of the target span.
    pub id_start: ID,
    /// The deleted position span
    pub span: DeleteSpan,
}

impl DeleteSpanWithId {
    pub fn new(id_start: ID, pos: isize, len: isize) -> Self {
        debug_assert!(len != 0);
        Self {
            id_start,
            span: DeleteSpan {
                pos,
                signed_len: len,
            },
        }
    }

    #[inline]
    pub fn start(&self) -> isize {
        self.span.start()
    }

    #[inline]
    pub fn end(&self) -> isize {
        self.span.end()
    }

    #[inline]
    pub fn last(&self) -> isize {
        self.span.last()
    }

    #[inline]
    pub fn is_reversed(&self) -> bool {
        self.span.is_reversed()
    }
}

impl HasLength for DeleteSpanWithId {
    fn content_len(&self) -> usize {
        self.span.content_len()
    }
}

impl HasId for DeleteSpanWithId {
    fn id_start(&self) -> ID {
        self.id_start
    }
}

impl Mergable for DeleteSpanWithId {
    /// If two spans are mergeable, their ids should be continuous.
    /// LHS's end id should be equal to RHS's start id.
    /// But their spans may be in a reversed order.
    fn is_mergable(&self, rhs: &Self, _conf: &()) -> bool
    where
        Self: Sized,
    {
        let this = self.span;
        let other = rhs.span;
        // merge continuous deletions:
        // note that the previous deletions will affect the position of the later deletions
        match (self.span.bidirectional(), rhs.span.bidirectional()) {
            (true, true) => {
                (this.pos == other.pos && self.id_start.inc(1) == rhs.id_start)
                    || (this.pos == other.pos + 1 && self.id_start == rhs.id_start.inc(1))
            }
            (true, false) => {
                if this.pos == other.prev_pos() {
                    if other.signed_len > 0 {
                        self.id_start.inc(1) == rhs.id_start
                    } else {
                        self.id_start == rhs.id_end()
                    }
                } else {
                    false
                }
            }
            (false, true) => {
                if this.next_pos() == other.pos {
                    if this.signed_len > 0 {
                        self.id_end() == rhs.id_start
                    } else {
                        self.id_start == rhs.id_start.inc(1)
                    }
                } else {
                    false
                }
            }
            (false, false) => {
                if this.next_pos() == other.pos && this.direction() == other.direction() {
                    if self.span.signed_len > 0 {
                        self.id_end() == rhs.id_start
                    } else {
                        self.id_start == rhs.id_end()
                    }
                } else {
                    false
                }
            }
        }
    }

    fn merge(&mut self, rhs: &Self, _conf: &())
    where
        Self: Sized,
    {
        self.id_start.counter = rhs.id_start.counter.min(self.id_start.counter);
        self.span.merge(&rhs.span, &())
    }
}

impl Sliceable for DeleteSpanWithId {
    fn slice(&self, from: usize, to: usize) -> Self {
        Self {
            id_start: if self.span.signed_len > 0 {
                self.id_start.inc(from as i32)
            } else {
                // If the span is reversed, the id_start should be affected by `to`
                //
                // Example:
                //
                // a b c
                // - - -  <-- deletions happen backward
                // 0 1 2  <-- counter of the IDs
                // ↑
                // id_start
                //
                // If from=1, to=2
                // a b c
                // - - -  <-- deletions happen backward
                // 0 1 2  <-- counter of the IDs
                //   ↑
                //   id_start
                self.id_start.inc((self.atom_len() - to) as i32)
            },
            span: self.span.slice(from, to),
        }
    }
}

/// `len` can be negative so that we can merge text deletions efficiently.
/// It looks like [crate::span::CounterSpan], but how should they merge ([Mergable] impl) and slice ([Sliceable] impl) are very different
///
/// len cannot be zero;
///
/// pos: 5, len: -3 eq a range of (2, 5]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
// Note: It will be encoded into binary format, so the order of its fields should not be changed.
pub struct DeleteSpan {
    pub pos: isize,
    pub signed_len: isize,
}

impl DeleteSpan {
    pub fn new(pos: isize, len: isize) -> Self {
        debug_assert!(len != 0);
        Self {
            pos,
            signed_len: len,
        }
    }

    #[inline(always)]
    pub fn start(&self) -> isize {
        if self.signed_len > 0 {
            self.pos
        } else {
            self.pos + 1 + self.signed_len
        }
    }

    #[inline(always)]
    pub fn last(&self) -> isize {
        if self.signed_len > 0 {
            self.pos + self.signed_len - 1
        } else {
            self.pos
        }
    }

    #[inline(always)]
    pub fn end(&self) -> isize {
        if self.signed_len > 0 {
            self.pos + self.signed_len
        } else {
            self.pos + 1
        }
    }

    #[inline(always)]
    pub fn to_range(self) -> Range<isize> {
        self.start()..self.end()
    }

    #[inline(always)]
    pub fn to_urange(self) -> Range<usize> {
        self.start() as usize..self.end() as usize
    }

    #[inline(always)]
    pub fn bidirectional(&self) -> bool {
        self.signed_len.abs() == 1
    }

    #[inline(always)]
    pub fn is_reversed(&self) -> bool {
        self.signed_len < 0
    }

    #[inline(always)]
    pub fn direction(&self) -> isize {
        if self.signed_len > 0 {
            1
        } else {
            -1
        }
    }

    #[inline(always)]
    pub fn next_pos(&self) -> isize {
        if self.signed_len > 0 {
            self.start()
        } else {
            self.start() - 1
        }
    }

    #[inline(always)]
    pub fn prev_pos(&self) -> isize {
        if self.signed_len > 0 {
            self.pos
        } else {
            self.end()
        }
    }

    pub fn len(&self) -> usize {
        self.signed_len.unsigned_abs()
    }
}

impl Mergable for DeleteSpan {
    fn is_mergable(&self, other: &Self, _conf: &()) -> bool
    where
        Self: Sized,
    {
        // merge continuous deletions:
        // note that the previous deletions will affect the position of the later deletions
        match (self.bidirectional(), other.bidirectional()) {
            (true, true) => self.pos == other.pos || self.pos == other.pos + 1,
            (true, false) => self.pos == other.prev_pos(),
            (false, true) => self.next_pos() == other.pos,
            (false, false) => self.next_pos() == other.pos && self.direction() == other.direction(),
        }
    }

    fn merge(&mut self, other: &Self, _conf: &())
    where
        Self: Sized,
    {
        match (self.bidirectional(), other.bidirectional()) {
            (true, true) => {
                if self.pos == other.pos {
                    self.signed_len = 2;
                } else if self.pos == other.pos + 1 {
                    self.signed_len = -2;
                } else {
                    unreachable!()
                }
            }
            (true, false) => {
                assert!(self.pos == other.prev_pos());
                self.signed_len = other.signed_len + other.direction();
            }
            (false, true) => {
                assert!(self.next_pos() == other.pos);
                self.signed_len += self.direction();
            }
            (false, false) => {
                assert!(self.next_pos() == other.pos && self.direction() == other.direction());
                self.signed_len += other.signed_len;
            }
        }
    }
}

impl Sliceable for DeleteSpan {
    fn slice(&self, from: usize, to: usize) -> Self {
        if self.signed_len > 0 {
            Self::new(self.pos, to as isize - from as isize)
        } else {
            Self::new(self.pos - from as isize, from as isize - to as isize)
        }
    }
}

impl Mergable for ListOp<'_> {
    fn is_mergable(&self, _other: &Self, _conf: &()) -> bool
    where
        Self: Sized,
    {
        match self {
            ListOp::Insert { pos, slice } => match _other {
                ListOp::Insert {
                    pos: other_pos,
                    slice: other_slice,
                } => pos + slice.content_len() == *other_pos && slice.is_mergable(other_slice, &()),
                _ => false,
            },
            &ListOp::Delete(span) => match _other {
                ListOp::Delete(other_span) => span.is_mergable(other_span, &()),
                _ => false,
            },
            ListOp::StyleStart { .. }
            | ListOp::StyleEnd { .. }
            | ListOp::Move { .. }
            | ListOp::Set { .. } => false,
        }
    }

    fn merge(&mut self, _other: &Self, _conf: &())
    where
        Self: Sized,
    {
        match self {
            ListOp::Insert { slice, .. } => match _other {
                ListOp::Insert {
                    slice: other_slice, ..
                } => {
                    slice.merge(other_slice, &());
                }
                _ => unreachable!(),
            },
            ListOp::Delete(span) => match _other {
                ListOp::Delete(other_span) => span.merge(other_span, &()),
                _ => unreachable!(),
            },
            ListOp::StyleStart { .. }
            | ListOp::StyleEnd { .. }
            | ListOp::Move { .. }
            | ListOp::Set { .. } => {
                unreachable!()
            }
        }
    }
}

impl HasLength for ListOp<'_> {
    fn content_len(&self) -> usize {
        match self {
            ListOp::Insert { slice, .. } => slice.content_len(),
            ListOp::Delete(span) => span.atom_len(),
            ListOp::StyleStart { .. }
            | ListOp::StyleEnd { .. }
            | ListOp::Move { .. }
            | ListOp::Set { .. } => 1,
        }
    }
}

impl Sliceable for ListOp<'_> {
    fn slice(&self, from: usize, to: usize) -> Self {
        match self {
            ListOp::Insert { slice, pos } => ListOp::Insert {
                slice: slice.slice(from, to),
                pos: *pos + from,
            },
            ListOp::Delete(span) => ListOp::Delete(span.slice(from, to)),
            a @ (ListOp::StyleStart { .. }
            | ListOp::StyleEnd { .. }
            | ListOp::Move { .. }
            | ListOp::Set { .. }) => a.clone(),
        }
    }
}

impl Mergable for InnerListOp {
    fn is_mergable(&self, other: &Self, _conf: &()) -> bool
    where
        Self: Sized,
    {
        match (self, other) {
            (
                Self::Insert { pos, slice, .. },
                Self::Insert {
                    pos: other_pos,
                    slice: other_slice,
                    ..
                },
            ) => pos + slice.content_len() == *other_pos && slice.is_mergable(other_slice, &()),
            (Self::Delete(span), Self::Delete(other_span)) => span.is_mergable(other_span, &()),
            (
                Self::InsertText {
                    unicode_start,
                    slice,
                    pos,
                    unicode_len: len,
                },
                Self::InsertText {
                    slice: other_slice,
                    pos: other_pos,
                    unicode_start: other_unicode_start,
                    unicode_len: _,
                },
            ) => {
                pos + len == *other_pos
                    && slice.can_merge(other_slice)
                    && unicode_start + len == *other_unicode_start
            }
            _ => false,
        }
    }

    fn merge(&mut self, other: &Self, _conf: &())
    where
        Self: Sized,
    {
        match (self, other) {
            (
                Self::Insert { slice, .. },
                Self::Insert {
                    slice: other_slice, ..
                },
            ) => {
                slice.merge(other_slice, &());
            }
            (Self::Delete(span), Self::Delete(other_span)) => span.merge(other_span, &()),
            (
                Self::InsertText {
                    slice,
                    unicode_len: len,
                    ..
                },
                Self::InsertText {
                    slice: other_slice,
                    unicode_len: other_len,
                    ..
                },
            ) => {
                slice.merge(other_slice, &());
                *len += *other_len;
            }
            _ => unreachable!(),
        }
    }
}

impl HasLength for InnerListOp {
    fn content_len(&self) -> usize {
        match self {
            Self::Insert { slice, .. } => slice.content_len(),
            Self::InsertText {
                unicode_len: len, ..
            } => *len as usize,
            Self::Delete(span) => span.atom_len(),
            Self::StyleStart { .. }
            | Self::StyleEnd { .. }
            | Self::Move { .. }
            | Self::Set { .. } => 1,
        }
    }
}

impl Sliceable for InnerListOp {
    fn slice(&self, from: usize, to: usize) -> Self {
        match self {
            Self::Insert { slice, pos } => Self::Insert {
                slice: slice.slice(from, to),
                pos: *pos + from,
            },
            Self::InsertText {
                slice,
                unicode_start,
                unicode_len: _,
                pos,
            } => Self::InsertText {
                slice: {
                    let (a, b) = unicode_range_to_byte_range(
                        // SAFETY: we know it's a valid utf8 string
                        unsafe { std::str::from_utf8_unchecked(slice) },
                        from,
                        to,
                    );
                    slice.slice(a, b)
                },
                unicode_start: *unicode_start + from as u32,
                unicode_len: (to - from) as u32,
                pos: *pos + from as u32,
            },
            Self::Delete(span) => Self::Delete(span.slice(from, to)),
            Self::StyleStart { .. }
            | Self::StyleEnd { .. }
            | Self::Move { .. }
            | Self::Set { .. } => {
                assert!(from == 0 && to == 1);
                self.clone()
            }
        }
    }
}

#[cfg(test)]
mod test {
    use loro_common::ID;
    use rle::{Mergable, Sliceable};

    use crate::{container::list::list_op::DeleteSpanWithId, op::ListSlice};

    use super::{DeleteSpan, ListOp};

    #[test]
    fn fix_fields_order() {
        let list_op = vec![
            ListOp::Insert {
                pos: 0,
                slice: ListSlice::from_borrowed_str(""),
            },
            ListOp::Delete(DeleteSpanWithId::new(ID::new(0, 0), 0, 3)),
        ];
        let actual = postcard::to_allocvec(&list_op).unwrap();
        let list_op_buf = vec![2, 0, 1, 0, 0, 0, 1, 0, 0, 0, 6];
        assert_eq!(&actual, &list_op_buf);
        assert_eq!(
            postcard::from_bytes::<Vec<ListOp>>(&list_op_buf).unwrap(),
            list_op
        );

        let delete_span = DeleteSpan {
            pos: 0,
            signed_len: 3,
        };
        let delete_span_buf = vec![0, 6];
        assert_eq!(
            postcard::from_bytes::<DeleteSpan>(&delete_span_buf).unwrap(),
            delete_span
        );
    }

    #[test]
    fn test_del_span_merge_slice() {
        let a = DeleteSpan::new(0, 100);
        let mut b = a.slice(0, 1);
        let c = a.slice(1, 100);
        b.merge(&c, &());
        assert_eq!(b, a);

        // reverse
        let a = DeleteSpan::new(99, -100);
        let mut b = a.slice(0, 1);
        let c = a.slice(1, 100);
        b.merge(&c, &());
        assert_eq!(b, a);

        // merge bidirectional
        let mut a = DeleteSpan::new(1, -1);
        let b = DeleteSpan::new(1, -1);
        assert!(a.is_mergable(&b, &()));
        a.merge(&b, &());
        assert_eq!(a, DeleteSpan::new(1, 2));

        let mut a = DeleteSpan::new(1, -1);
        let b = DeleteSpan::new(0, -1);
        assert_eq!(b.to_range(), 0..1);
        assert!(a.is_mergable(&b, &()));
        a.merge(&b, &());
        assert_eq!(a, DeleteSpan::new(1, -2));

        // not merging
        let a = DeleteSpan::new(4, 1);
        let b = DeleteSpan::new(5, 2);
        assert!(!a.is_mergable(&b, &()));

        // next/prev span
        let a = DeleteSpan::new(6, -2);
        assert_eq!(a.next_pos(), 4);
        assert_eq!(a.prev_pos(), 7);
        let a = DeleteSpan::new(6, 2);
        assert_eq!(a.next_pos(), 6);
        assert_eq!(a.prev_pos(), 6);
        assert!(a.slice(0, 1).is_mergable(&a.slice(1, 2), &()));

        // neg merge
        let mut a = DeleteSpan::new(1, 1);
        let b = DeleteSpan::new(0, 1);
        a.merge(&b, &());
        assert_eq!(a, DeleteSpan::new(1, -2));
        assert_eq!(a.slice(0, 1), DeleteSpan::new(1, -1));
        assert_eq!(a.slice(1, 2), DeleteSpan::new(0, -1));
        assert_eq!(a.slice(0, 1).to_range(), 1..2);
        assert_eq!(a.slice(1, 2).to_range(), 0..1);
    }

    #[test]
    fn mergeable() {
        let a = DeleteSpan::new(14852, 1);
        let mut a_with_id = DeleteSpanWithId {
            id_start: ID::new(0, 9),
            span: a,
        };
        let b = DeleteSpan::new(14851, 1);
        let b_with_id = DeleteSpanWithId {
            id_start: ID::new(0, 8),
            span: b,
        };
        assert!(a_with_id.is_mergable(&b_with_id, &()));
        a_with_id.merge(&b_with_id, &());
        assert!(a_with_id.span.signed_len == -2);
    }
}
