//! # Index
//!
//! There are several types of indexes:
//!
//! - Unicode index: the index of a unicode code point in the text.
//! - Entity index: unicode index + style anchor index. Each unicode code point or style anchor is an entity.
//! - Utf16 index
//!
//! In [crate::op::Op], we always use entity index to persist richtext ops.
//!
//! The users of this type can only operate on unicode index or utf16 index, but calculated entity index will be provided.

pub(crate) mod config;
mod fugue_span;
pub(crate) mod richtext_state;
pub(crate) mod str_slice;
mod style_range_map;
mod tracker;

use crate::{change::Lamport, delta::StyleMeta, utils::string_slice::StringSlice, InternalString};
use fugue_span::*;
use loro_common::{Counter, IdFull, IdLp, LoroValue, PeerID, ID};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;

pub(crate) use fugue_span::{RichtextChunk, RichtextChunkValue};
pub(crate) use richtext_state::RichtextState;
pub(crate) use style_range_map::Styles;
pub(crate) use tracker::{CrdtRopeDelta, Tracker as RichtextTracker};

/// This is the data structure that represents a span of rich text.
/// It's used to communicate with the frontend.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RichtextSpan {
    pub text: StringSlice,
    pub attributes: StyleMeta,
}

/// This is used to communicate with the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Style {
    pub key: InternalString,
    pub data: LoroValue,
}

// TODO: change visibility back to crate after #116 is done
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct StyleOp {
    pub(crate) lamport: Lamport,
    pub(crate) peer: PeerID,
    pub(crate) cnt: Counter,
    pub(crate) key: InternalString,
    pub(crate) value: LoroValue,
    pub(crate) info: TextStyleInfoFlag,
}

#[derive(Debug, Hash, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub(crate) enum StyleKey {
    Key(InternalString),
}

impl StyleKey {
    pub fn key(&self) -> &InternalString {
        match self {
            Self::Key(key) => key,
        }
    }
}

impl StyleOp {
    pub fn to_style(&self) -> Style {
        Style {
            key: self.key.clone(),
            data: self.value.clone(),
        }
    }

    pub fn to_value(&self) -> LoroValue {
        self.value.clone()
    }

    pub(crate) fn get_style_key(&self) -> StyleKey {
        StyleKey::Key(self.key.clone())
    }

    #[cfg(test)]
    pub fn new_for_test(n: isize, key: &str, value: LoroValue, info: TextStyleInfoFlag) -> Self {
        Self {
            lamport: n as Lamport,
            peer: n as PeerID,
            cnt: n as Counter,
            key: key.to_string().into(),
            value,
            info,
        }
    }

    #[inline(always)]
    pub fn id(&self) -> ID {
        ID::new(self.peer, self.cnt)
    }

    pub fn idlp(&self) -> IdLp {
        IdLp::new(self.peer, self.lamport)
    }

    pub fn id_full(&self) -> IdFull {
        IdFull::new(self.peer, self.cnt, self.lamport)
    }
}

impl PartialOrd for StyleOp {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for StyleOp {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.lamport
            .cmp(&other.lamport)
            .then(self.peer.cmp(&other.peer))
    }
}

/// TODO: We can remove this type already
///
/// A compact representation of a rich text style config.
///
/// Note: we assume style with the same key has the same `Mergeable` and `isContainer` value.
///
/// - 0              (1st bit)
/// - Expand Before  (2nd bit): when inserting new text before this style, whether the new text should inherit this style.
/// - Expand After   (3rd bit): when inserting new text after  this style, whether the new text should inherit this style.
/// - 0              (4th bit):
/// - 0              (5th bit):
/// - 0              (6th bit)
/// - 0              (7th bit)
/// - 0              (8th bit):
#[derive(Default, Clone, Copy, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TextStyleInfoFlag {
    data: u8,
}

impl Debug for TextStyleInfoFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextStyleInfo")
            // write data in binary format
            .field("data", &format!("{:#010b}", self.data))
            .field("expand_before", &self.expand_before())
            .field("expand_after", &self.expand_after())
            .finish()
    }
}

const EXPAND_BEFORE_MASK: u8 = 0b0000_0010;
const EXPAND_AFTER_MASK: u8 = 0b0000_0100;
const ALIVE_MASK: u8 = 0b1000_0000;

/// Whether to expand the style when inserting new text around it.
///
/// - Before: when inserting new text before this style, the new text should inherit this style.
/// - After: when inserting new text after this style, the new text should inherit this style.
/// - Both: when inserting new text before or after this style, the new text should inherit this style.
/// - None: when inserting new text before or after this style, the new text should **not** inherit this style.
#[derive(Clone, Copy, Eq, PartialEq, Debug, Hash)]
pub enum ExpandType {
    Before,
    After,
    Both,
    None,
}

#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum AnchorType {
    Start,
    End,
}

impl ExpandType {
    #[inline(always)]
    pub const fn expand_before(&self) -> bool {
        matches!(self, Self::Before | Self::Both)
    }

    #[inline(always)]
    pub const fn expand_after(&self) -> bool {
        matches!(self, Self::After | Self::Both)
    }

    /// 'before'|'after'|'both'|'none'
    pub fn try_from_str(s: &str) -> Option<Self> {
        match s {
            "before" => Some(Self::Before),
            "after" => Some(Self::After),
            "both" => Some(Self::Both),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// Toggle expand type between for deletion and for creation
    ///
    /// For a style that expand after, when we delete the style, we need to have another style that expands after to nullify it,
    /// so that the expand behavior is not changed.
    ///
    /// Before  -> Before
    /// After   -> After
    /// Both    -> None
    /// None    -> Both
    ///
    /// Because the creation of text styles and the deletion of the text styles have reversed expand type.
    /// This method is useful to convert between the two
    pub const fn reverse(self) -> Self {
        match self {
            Self::Before => Self::Before,
            Self::After => Self::After,
            Self::Both => Self::None,
            Self::None => Self::Both,
        }
    }
}

impl TextStyleInfoFlag {
    /// When inserting new text around this style, prefer inserting after it.
    #[inline(always)]
    pub const fn expand_before(self) -> bool {
        self.data & EXPAND_BEFORE_MASK != 0
    }

    /// When inserting new text around this style, prefer inserting before it.
    #[inline(always)]
    pub const fn expand_after(self) -> bool {
        self.data & EXPAND_AFTER_MASK != 0
    }

    pub const fn expand_type(self) -> ExpandType {
        match (self.expand_before(), self.expand_after()) {
            (true, true) => ExpandType::Both,
            (true, false) => ExpandType::Before,
            (false, true) => ExpandType::After,
            (false, false) => ExpandType::None,
        }
    }

    /// This method tells that when we can insert text before/after this style anchor, whether we insert the new text before the anchor.
    #[inline]
    pub fn prefer_insert_before(self, anchor_type: AnchorType) -> bool {
        match anchor_type {
            AnchorType::Start => {
                // If we need to expand the style, the new text should be inserted **after** the start anchor
                !self.expand_before()
            }
            AnchorType::End => {
                // If we need to expand the style, the new text should be inserted **before** the end anchor
                self.expand_after()
            }
        }
    }

    pub const fn new(expand_type: ExpandType) -> Self {
        let mut data = ALIVE_MASK;
        if expand_type.expand_before() {
            data |= EXPAND_BEFORE_MASK;
        }
        if expand_type.expand_after() {
            data |= EXPAND_AFTER_MASK;
        }

        Self { data }
    }

    #[inline(always)]
    pub const fn to_delete(self) -> Self {
        Self::new(self.expand_type().reverse())
    }

    pub const BOLD: Self = Self::new(ExpandType::After);
    pub const LINK: Self = Self::new(ExpandType::None);
    pub const COMMENT: Self = Self::new(ExpandType::None);

    pub const fn to_byte(&self) -> u8 {
        self.data
    }

    pub const fn from_byte(data: u8) -> Self {
        Self { data }
    }
}

#[cfg(test)]
mod test {

    #[test]
    fn test() {}
}
