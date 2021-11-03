mod anchor;
mod operation_queue;
mod point;
mod point_utf16;
#[cfg(any(test, feature = "test-support"))]
pub mod random_char_iter;
pub mod rope;
mod selection;
#[cfg(test)]
mod tests;

pub use anchor::*;
use anyhow::{anyhow, Result};
use clock::ReplicaId;
use operation_queue::OperationQueue;
pub use point::*;
pub use point_utf16::*;
#[cfg(any(test, feature = "test-support"))]
pub use random_char_iter::*;
use rope::TextDimension;
pub use rope::{Chunks, Rope, TextSummary};
pub use selection::*;
use std::{
    cmp::{self, Reverse},
    iter::Iterator,
    ops::{self, Range},
    str,
    sync::Arc,
    time::{Duration, Instant},
};
pub use sum_tree::Bias;
use sum_tree::{FilterCursor, SumTree};

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Default)]
pub struct DeterministicState;

#[cfg(any(test, feature = "test-support"))]
impl std::hash::BuildHasher for DeterministicState {
    type Hasher = seahash::SeaHasher;

    fn build_hasher(&self) -> Self::Hasher {
        seahash::SeaHasher::new()
    }
}

#[cfg(any(test, feature = "test-support"))]
type HashMap<K, V> = std::collections::HashMap<K, V, DeterministicState>;

#[cfg(any(test, feature = "test-support"))]
type HashSet<T> = std::collections::HashSet<T, DeterministicState>;

#[cfg(not(any(test, feature = "test-support")))]
type HashMap<K, V> = std::collections::HashMap<K, V>;

#[cfg(not(any(test, feature = "test-support")))]
type HashSet<T> = std::collections::HashSet<T>;

#[derive(Clone)]
pub struct Buffer {
    fragments: SumTree<Fragment>,
    visible_text: Rope,
    deleted_text: Rope,
    pub version: clock::Global,
    last_edit: clock::Local,
    undo_map: UndoMap,
    history: History,
    selections: HashMap<SelectionSetId, SelectionSet>,
    deferred_ops: OperationQueue,
    deferred_replicas: HashSet<ReplicaId>,
    replica_id: ReplicaId,
    remote_id: u64,
    local_clock: clock::Local,
    lamport_clock: clock::Lamport,
}

#[derive(Clone, Debug)]
pub struct Transaction {
    start: clock::Global,
    end: clock::Global,
    edits: Vec<clock::Local>,
    ranges: Vec<Range<FullOffset>>,
    selections_before: HashMap<SelectionSetId, Arc<AnchorRangeMap<SelectionState>>>,
    selections_after: HashMap<SelectionSetId, Arc<AnchorRangeMap<SelectionState>>>,
    first_edit_at: Instant,
    last_edit_at: Instant,
}

impl Transaction {
    pub fn starting_selection_set_ids<'a>(&'a self) -> impl Iterator<Item = SelectionSetId> + 'a {
        self.selections_before.keys().copied()
    }

    fn push_edit(&mut self, edit: &EditOperation) {
        self.edits.push(edit.timestamp.local());
        self.end.observe(edit.timestamp.local());

        let mut other_ranges = edit.ranges.iter().peekable();
        let mut new_ranges = Vec::new();
        let insertion_len = edit.new_text.as_ref().map_or(0, |t| t.len());
        let mut delta = 0;

        for mut self_range in self.ranges.iter().cloned() {
            self_range.start += delta;
            self_range.end += delta;

            while let Some(other_range) = other_ranges.peek() {
                let mut other_range = (*other_range).clone();
                other_range.start += delta;
                other_range.end += delta;

                if other_range.start <= self_range.end {
                    other_ranges.next().unwrap();
                    delta += insertion_len;

                    if other_range.end < self_range.start {
                        new_ranges.push(other_range.start..other_range.end + insertion_len);
                        self_range.start += insertion_len;
                        self_range.end += insertion_len;
                    } else {
                        self_range.start = cmp::min(self_range.start, other_range.start);
                        self_range.end = cmp::max(self_range.end, other_range.end) + insertion_len;
                    }
                } else {
                    break;
                }
            }

            new_ranges.push(self_range);
        }

        for other_range in other_ranges {
            new_ranges.push(other_range.start + delta..other_range.end + delta + insertion_len);
            delta += insertion_len;
        }

        self.ranges = new_ranges;
    }
}

#[derive(Clone)]
pub struct History {
    // TODO: Turn this into a String or Rope, maybe.
    pub base_text: Arc<str>,
    ops: HashMap<clock::Local, EditOperation>,
    undo_stack: Vec<Transaction>,
    redo_stack: Vec<Transaction>,
    transaction_depth: usize,
    group_interval: Duration,
}

impl History {
    pub fn new(base_text: Arc<str>) -> Self {
        Self {
            base_text,
            ops: Default::default(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            transaction_depth: 0,
            group_interval: Duration::from_millis(300),
        }
    }

    fn push(&mut self, op: EditOperation) {
        self.ops.insert(op.timestamp.local(), op);
    }

    fn start_transaction(
        &mut self,
        start: clock::Global,
        selections_before: HashMap<SelectionSetId, Arc<AnchorRangeMap<SelectionState>>>,
        now: Instant,
    ) {
        self.transaction_depth += 1;
        if self.transaction_depth == 1 {
            self.undo_stack.push(Transaction {
                start: start.clone(),
                end: start,
                edits: Vec::new(),
                ranges: Vec::new(),
                selections_before,
                selections_after: Default::default(),
                first_edit_at: now,
                last_edit_at: now,
            });
        }
    }

    fn end_transaction(
        &mut self,
        selections_after: HashMap<SelectionSetId, Arc<AnchorRangeMap<SelectionState>>>,
        now: Instant,
    ) -> Option<&Transaction> {
        assert_ne!(self.transaction_depth, 0);
        self.transaction_depth -= 1;
        if self.transaction_depth == 0 {
            if self.undo_stack.last().unwrap().ranges.is_empty() {
                self.undo_stack.pop();
                None
            } else {
                let transaction = self.undo_stack.last_mut().unwrap();
                transaction.selections_after = selections_after;
                transaction.last_edit_at = now;
                Some(transaction)
            }
        } else {
            None
        }
    }

    fn group(&mut self) {
        let mut new_len = self.undo_stack.len();
        let mut transactions = self.undo_stack.iter_mut();

        if let Some(mut transaction) = transactions.next_back() {
            while let Some(prev_transaction) = transactions.next_back() {
                if transaction.first_edit_at - prev_transaction.last_edit_at <= self.group_interval
                    && transaction.start == prev_transaction.end
                {
                    transaction = prev_transaction;
                    new_len -= 1;
                } else {
                    break;
                }
            }
        }

        let (transactions_to_keep, transactions_to_merge) = self.undo_stack.split_at_mut(new_len);
        if let Some(last_transaction) = transactions_to_keep.last_mut() {
            for transaction in &*transactions_to_merge {
                for edit_id in &transaction.edits {
                    last_transaction.push_edit(&self.ops[edit_id]);
                }
            }

            if let Some(transaction) = transactions_to_merge.last_mut() {
                last_transaction.last_edit_at = transaction.last_edit_at;
                last_transaction
                    .selections_after
                    .extend(transaction.selections_after.drain());
                last_transaction.end = transaction.end.clone();
            }
        }

        self.undo_stack.truncate(new_len);
    }

    fn push_undo(&mut self, edit_id: clock::Local) {
        assert_ne!(self.transaction_depth, 0);
        let last_transaction = self.undo_stack.last_mut().unwrap();
        last_transaction.push_edit(&self.ops[&edit_id]);
    }

    fn pop_undo(&mut self) -> Option<&Transaction> {
        assert_eq!(self.transaction_depth, 0);
        if let Some(transaction) = self.undo_stack.pop() {
            self.redo_stack.push(transaction);
            self.redo_stack.last()
        } else {
            None
        }
    }

    fn pop_redo(&mut self) -> Option<&Transaction> {
        assert_eq!(self.transaction_depth, 0);
        if let Some(transaction) = self.redo_stack.pop() {
            self.undo_stack.push(transaction);
            self.undo_stack.last()
        } else {
            None
        }
    }
}

#[derive(Clone, Default, Debug)]
struct UndoMap(HashMap<clock::Local, Vec<(clock::Local, u32)>>);

impl UndoMap {
    fn insert(&mut self, undo: &UndoOperation) {
        for (edit_id, count) in &undo.counts {
            self.0.entry(*edit_id).or_default().push((undo.id, *count));
        }
    }

    fn is_undone(&self, edit_id: clock::Local) -> bool {
        self.undo_count(edit_id) % 2 == 1
    }

    fn was_undone(&self, edit_id: clock::Local, version: &clock::Global) -> bool {
        let undo_count = self
            .0
            .get(&edit_id)
            .unwrap_or(&Vec::new())
            .iter()
            .filter(|(undo_id, _)| version.observed(*undo_id))
            .map(|(_, undo_count)| *undo_count)
            .max()
            .unwrap_or(0);
        undo_count % 2 == 1
    }

    fn undo_count(&self, edit_id: clock::Local) -> u32 {
        self.0
            .get(&edit_id)
            .unwrap_or(&Vec::new())
            .iter()
            .map(|(_, undo_count)| *undo_count)
            .max()
            .unwrap_or(0)
    }
}

struct Edits<'a, D: TextDimension<'a>, F: FnMut(&FragmentSummary) -> bool> {
    visible_cursor: rope::Cursor<'a>,
    deleted_cursor: rope::Cursor<'a>,
    fragments_cursor: Option<FilterCursor<'a, F, Fragment, FragmentTextSummary>>,
    undos: &'a UndoMap,
    since: &'a clock::Global,
    old_end: D,
    new_end: D,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Edit<D> {
    pub old: Range<D>,
    pub new: Range<D>,
}

impl<D1, D2> Edit<(D1, D2)> {
    pub fn flatten(self) -> (Edit<D1>, Edit<D2>) {
        (
            Edit {
                old: self.old.start.0..self.old.end.0,
                new: self.new.start.0..self.new.end.0,
            },
            Edit {
                old: self.old.start.1..self.old.end.1,
                new: self.new.start.1..self.new.end.1,
            },
        )
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct InsertionTimestamp {
    pub replica_id: ReplicaId,
    pub local: clock::Seq,
    pub lamport: clock::Seq,
}

impl InsertionTimestamp {
    fn local(&self) -> clock::Local {
        clock::Local {
            replica_id: self.replica_id,
            value: self.local,
        }
    }

    fn lamport(&self) -> clock::Lamport {
        clock::Lamport {
            replica_id: self.replica_id,
            value: self.lamport,
        }
    }
}

#[derive(Eq, PartialEq, Clone, Debug)]
struct Fragment {
    timestamp: InsertionTimestamp,
    len: usize,
    visible: bool,
    deletions: HashSet<clock::Local>,
    max_undos: clock::Global,
}

#[derive(Eq, PartialEq, Clone, Debug)]
pub struct FragmentSummary {
    text: FragmentTextSummary,
    max_version: clock::Global,
    min_insertion_version: clock::Global,
    max_insertion_version: clock::Global,
}

#[derive(Copy, Default, Clone, Debug, PartialEq, Eq)]
struct FragmentTextSummary {
    visible: usize,
    deleted: usize,
}

impl<'a> sum_tree::Dimension<'a, FragmentSummary> for FragmentTextSummary {
    fn add_summary(&mut self, summary: &'a FragmentSummary, _: &Option<clock::Global>) {
        self.visible += summary.text.visible;
        self.deleted += summary.text.deleted;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operation {
    Edit(EditOperation),
    Undo {
        undo: UndoOperation,
        lamport_timestamp: clock::Lamport,
    },
    UpdateSelections {
        set_id: SelectionSetId,
        selections: Arc<AnchorRangeMap<SelectionState>>,
        lamport_timestamp: clock::Lamport,
    },
    RemoveSelections {
        set_id: SelectionSetId,
        lamport_timestamp: clock::Lamport,
    },
    SetActiveSelections {
        set_id: Option<SelectionSetId>,
        lamport_timestamp: clock::Lamport,
    },
    #[cfg(test)]
    Test(clock::Lamport),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditOperation {
    pub timestamp: InsertionTimestamp,
    pub version: clock::Global,
    pub ranges: Vec<Range<FullOffset>>,
    pub new_text: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UndoOperation {
    pub id: clock::Local,
    pub counts: HashMap<clock::Local, u32>,
    pub ranges: Vec<Range<FullOffset>>,
    pub version: clock::Global,
}

impl Buffer {
    pub fn new(replica_id: u16, remote_id: u64, history: History) -> Buffer {
        let mut fragments = SumTree::new();

        let visible_text = Rope::from(history.base_text.as_ref());
        if visible_text.len() > 0 {
            fragments.push(
                Fragment {
                    timestamp: Default::default(),
                    len: visible_text.len(),
                    visible: true,
                    deletions: Default::default(),
                    max_undos: Default::default(),
                },
                &None,
            );
        }

        Buffer {
            visible_text,
            deleted_text: Rope::new(),
            fragments,
            version: clock::Global::new(),
            last_edit: clock::Local::default(),
            undo_map: Default::default(),
            history,
            selections: HashMap::default(),
            deferred_ops: OperationQueue::new(),
            deferred_replicas: HashSet::default(),
            replica_id,
            remote_id,
            local_clock: clock::Local::new(replica_id),
            lamport_clock: clock::Lamport::new(replica_id),
        }
    }

    pub fn version(&self) -> clock::Global {
        self.version.clone()
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            visible_text: self.visible_text.clone(),
            deleted_text: self.deleted_text.clone(),
            undo_map: self.undo_map.clone(),
            fragments: self.fragments.clone(),
            version: self.version.clone(),
        }
    }

    pub fn content<'a>(&'a self) -> Content<'a> {
        self.into()
    }

    pub fn as_rope(&self) -> &Rope {
        &self.visible_text
    }

    pub fn text_summary_for_range(&self, range: Range<usize>) -> TextSummary {
        self.content().text_summary_for_range(range)
    }

    pub fn anchor_before<T: ToOffset>(&self, position: T) -> Anchor {
        self.anchor_at(position, Bias::Left)
    }

    pub fn anchor_after<T: ToOffset>(&self, position: T) -> Anchor {
        self.anchor_at(position, Bias::Right)
    }

    pub fn anchor_at<T: ToOffset>(&self, position: T, bias: Bias) -> Anchor {
        self.content().anchor_at(position, bias)
    }

    pub fn anchor_range_set<E>(&self, entries: E) -> AnchorRangeSet
    where
        E: IntoIterator<Item = Range<(usize, Bias)>>,
    {
        self.content().anchor_range_set(entries)
    }

    pub fn point_for_offset(&self, offset: usize) -> Result<Point> {
        self.content().point_for_offset(offset)
    }

    pub fn clip_point(&self, point: Point, bias: Bias) -> Point {
        self.content().clip_point(point, bias)
    }

    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        self.visible_text.clip_offset(offset, bias)
    }

    pub fn replica_id(&self) -> ReplicaId {
        self.local_clock.replica_id
    }

    pub fn remote_id(&self) -> u64 {
        self.remote_id
    }

    pub fn text_summary(&self) -> TextSummary {
        self.visible_text.summary()
    }

    pub fn len(&self) -> usize {
        self.content().len()
    }

    pub fn line_len(&self, row: u32) -> u32 {
        self.content().line_len(row)
    }

    pub fn max_point(&self) -> Point {
        self.visible_text.max_point()
    }

    pub fn row_count(&self) -> u32 {
        self.max_point().row + 1
    }

    pub fn text(&self) -> String {
        self.text_for_range(0..self.len()).collect()
    }

    pub fn text_for_range<'a, T: ToOffset>(&'a self, range: Range<T>) -> Chunks<'a> {
        self.content().text_for_range(range)
    }

    pub fn chars(&self) -> impl Iterator<Item = char> + '_ {
        self.chars_at(0)
    }

    pub fn chars_at<'a, T: 'a + ToOffset>(
        &'a self,
        position: T,
    ) -> impl Iterator<Item = char> + 'a {
        self.content().chars_at(position)
    }

    pub fn reversed_chars_at<'a, T: 'a + ToOffset>(
        &'a self,
        position: T,
    ) -> impl Iterator<Item = char> + 'a {
        self.content().reversed_chars_at(position)
    }

    pub fn chars_for_range<T: ToOffset>(&self, range: Range<T>) -> impl Iterator<Item = char> + '_ {
        self.text_for_range(range).flat_map(str::chars)
    }

    pub fn bytes_at<T: ToOffset>(&self, position: T) -> impl Iterator<Item = u8> + '_ {
        let offset = position.to_offset(self);
        self.visible_text.bytes_at(offset)
    }

    pub fn contains_str_at<T>(&self, position: T, needle: &str) -> bool
    where
        T: ToOffset,
    {
        let position = position.to_offset(self);
        position == self.clip_offset(position, Bias::Left)
            && self
                .bytes_at(position)
                .take(needle.len())
                .eq(needle.bytes())
    }

    pub fn deferred_ops_len(&self) -> usize {
        self.deferred_ops.len()
    }

    pub fn edit<R, I, S, T>(&mut self, ranges: R, new_text: T) -> EditOperation
    where
        R: IntoIterator<IntoIter = I>,
        I: ExactSizeIterator<Item = Range<S>>,
        S: ToOffset,
        T: Into<String>,
    {
        let new_text = new_text.into();
        let new_text_len = new_text.len();
        let new_text = if new_text_len > 0 {
            Some(new_text)
        } else {
            None
        };

        self.start_transaction(None).unwrap();
        let timestamp = InsertionTimestamp {
            replica_id: self.replica_id,
            local: self.local_clock.tick().value,
            lamport: self.lamport_clock.tick().value,
        };
        let edit = self.apply_local_edit(ranges.into_iter(), new_text, timestamp);

        self.history.push(edit.clone());
        self.history.push_undo(edit.timestamp.local());
        self.last_edit = edit.timestamp.local();
        self.version.observe(edit.timestamp.local());
        self.end_transaction(None);
        edit
    }

    fn apply_local_edit<S: ToOffset>(
        &mut self,
        ranges: impl ExactSizeIterator<Item = Range<S>>,
        new_text: Option<String>,
        timestamp: InsertionTimestamp,
    ) -> EditOperation {
        let mut edit = EditOperation {
            timestamp,
            version: self.version(),
            ranges: Vec::with_capacity(ranges.len()),
            new_text: None,
        };

        let mut ranges = ranges
            .map(|range| range.start.to_offset(&*self)..range.end.to_offset(&*self))
            .peekable();

        let mut new_ropes =
            RopeBuilder::new(self.visible_text.cursor(0), self.deleted_text.cursor(0));
        let mut old_fragments = self.fragments.cursor::<FragmentTextSummary>();
        let mut new_fragments =
            old_fragments.slice(&ranges.peek().unwrap().start, Bias::Right, &None);
        new_ropes.push_tree(new_fragments.summary().text);

        let mut fragment_start = old_fragments.start().visible;
        for range in ranges {
            let fragment_end = old_fragments.end(&None).visible;

            // If the current fragment ends before this range, then jump ahead to the first fragment
            // that extends past the start of this range, reusing any intervening fragments.
            if fragment_end < range.start {
                // If the current fragment has been partially consumed, then consume the rest of it
                // and advance to the next fragment before slicing.
                if fragment_start > old_fragments.start().visible {
                    if fragment_end > fragment_start {
                        let mut suffix = old_fragments.item().unwrap().clone();
                        suffix.len = fragment_end - fragment_start;
                        new_ropes.push_fragment(&suffix, suffix.visible);
                        new_fragments.push(suffix, &None);
                    }
                    old_fragments.next(&None);
                }

                let slice = old_fragments.slice(&range.start, Bias::Right, &None);
                new_ropes.push_tree(slice.summary().text);
                new_fragments.push_tree(slice, &None);
                fragment_start = old_fragments.start().visible;
            }

            let full_range_start = FullOffset(range.start + old_fragments.start().deleted);

            // Preserve any portion of the current fragment that precedes this range.
            if fragment_start < range.start {
                let mut prefix = old_fragments.item().unwrap().clone();
                prefix.len = range.start - fragment_start;
                new_ropes.push_fragment(&prefix, prefix.visible);
                new_fragments.push(prefix, &None);
                fragment_start = range.start;
            }

            // Insert the new text before any existing fragments within the range.
            if let Some(new_text) = new_text.as_deref() {
                new_ropes.push_str(new_text);
                new_fragments.push(
                    Fragment {
                        timestamp,
                        len: new_text.len(),
                        deletions: Default::default(),
                        max_undos: Default::default(),
                        visible: true,
                    },
                    &None,
                );
            }

            // Advance through every fragment that intersects this range, marking the intersecting
            // portions as deleted.
            while fragment_start < range.end {
                let fragment = old_fragments.item().unwrap();
                let fragment_end = old_fragments.end(&None).visible;
                let mut intersection = fragment.clone();
                let intersection_end = cmp::min(range.end, fragment_end);
                if fragment.visible {
                    intersection.len = intersection_end - fragment_start;
                    intersection.deletions.insert(timestamp.local());
                    intersection.visible = false;
                }
                if intersection.len > 0 {
                    new_ropes.push_fragment(&intersection, fragment.visible);
                    new_fragments.push(intersection, &None);
                    fragment_start = intersection_end;
                }
                if fragment_end <= range.end {
                    old_fragments.next(&None);
                }
            }

            let full_range_end = FullOffset(range.end + old_fragments.start().deleted);
            edit.ranges.push(full_range_start..full_range_end);
        }

        // If the current fragment has been partially consumed, then consume the rest of it
        // and advance to the next fragment before slicing.
        if fragment_start > old_fragments.start().visible {
            let fragment_end = old_fragments.end(&None).visible;
            if fragment_end > fragment_start {
                let mut suffix = old_fragments.item().unwrap().clone();
                suffix.len = fragment_end - fragment_start;
                new_ropes.push_fragment(&suffix, suffix.visible);
                new_fragments.push(suffix, &None);
            }
            old_fragments.next(&None);
        }

        let suffix = old_fragments.suffix(&None);
        new_ropes.push_tree(suffix.summary().text);
        new_fragments.push_tree(suffix, &None);
        let (visible_text, deleted_text) = new_ropes.finish();
        drop(old_fragments);

        self.fragments = new_fragments;
        self.visible_text = visible_text;
        self.deleted_text = deleted_text;
        edit.new_text = new_text;
        edit
    }

    pub fn apply_ops<I: IntoIterator<Item = Operation>>(&mut self, ops: I) -> Result<()> {
        let mut deferred_ops = Vec::new();
        for op in ops {
            if self.can_apply_op(&op) {
                self.apply_op(op)?;
            } else {
                self.deferred_replicas.insert(op.replica_id());
                deferred_ops.push(op);
            }
        }
        self.deferred_ops.insert(deferred_ops);
        self.flush_deferred_ops()?;
        Ok(())
    }

    fn apply_op(&mut self, op: Operation) -> Result<()> {
        match op {
            Operation::Edit(edit) => {
                if !self.version.observed(edit.timestamp.local()) {
                    self.apply_remote_edit(
                        &edit.version,
                        &edit.ranges,
                        edit.new_text.as_deref(),
                        edit.timestamp,
                    );
                    self.version.observe(edit.timestamp.local());
                    self.history.push(edit);
                }
            }
            Operation::Undo {
                undo,
                lamport_timestamp,
            } => {
                if !self.version.observed(undo.id) {
                    self.apply_undo(&undo)?;
                    self.version.observe(undo.id);
                    self.lamport_clock.observe(lamport_timestamp);
                }
            }
            Operation::UpdateSelections {
                set_id,
                selections,
                lamport_timestamp,
            } => {
                if let Some(set) = self.selections.get_mut(&set_id) {
                    set.selections = selections;
                } else {
                    self.selections.insert(
                        set_id,
                        SelectionSet {
                            id: set_id,
                            selections,
                            active: false,
                        },
                    );
                }
                self.lamport_clock.observe(lamport_timestamp);
            }
            Operation::RemoveSelections {
                set_id,
                lamport_timestamp,
            } => {
                self.selections.remove(&set_id);
                self.lamport_clock.observe(lamport_timestamp);
            }
            Operation::SetActiveSelections {
                set_id,
                lamport_timestamp,
            } => {
                for (id, set) in &mut self.selections {
                    if id.replica_id == lamport_timestamp.replica_id {
                        if Some(*id) == set_id {
                            set.active = true;
                        } else {
                            set.active = false;
                        }
                    }
                }
                self.lamport_clock.observe(lamport_timestamp);
            }
            #[cfg(test)]
            Operation::Test(_) => {}
        }
        Ok(())
    }

    fn apply_remote_edit(
        &mut self,
        version: &clock::Global,
        ranges: &[Range<FullOffset>],
        new_text: Option<&str>,
        timestamp: InsertionTimestamp,
    ) {
        if ranges.is_empty() {
            return;
        }

        let cx = Some(version.clone());
        let mut new_ropes =
            RopeBuilder::new(self.visible_text.cursor(0), self.deleted_text.cursor(0));
        let mut old_fragments = self.fragments.cursor::<VersionedFullOffset>();
        let mut new_fragments = old_fragments.slice(
            &VersionedFullOffset::Offset(ranges[0].start),
            Bias::Left,
            &cx,
        );
        new_ropes.push_tree(new_fragments.summary().text);

        let mut fragment_start = old_fragments.start().full_offset();
        for range in ranges {
            let fragment_end = old_fragments.end(&cx).full_offset();

            // If the current fragment ends before this range, then jump ahead to the first fragment
            // that extends past the start of this range, reusing any intervening fragments.
            if fragment_end < range.start {
                // If the current fragment has been partially consumed, then consume the rest of it
                // and advance to the next fragment before slicing.
                if fragment_start > old_fragments.start().full_offset() {
                    if fragment_end > fragment_start {
                        let mut suffix = old_fragments.item().unwrap().clone();
                        suffix.len = fragment_end.0 - fragment_start.0;
                        new_ropes.push_fragment(&suffix, suffix.visible);
                        new_fragments.push(suffix, &None);
                    }
                    old_fragments.next(&cx);
                }

                let slice =
                    old_fragments.slice(&VersionedFullOffset::Offset(range.start), Bias::Left, &cx);
                new_ropes.push_tree(slice.summary().text);
                new_fragments.push_tree(slice, &None);
                fragment_start = old_fragments.start().full_offset();
            }

            // If we are at the end of a non-concurrent fragment, advance to the next one.
            let fragment_end = old_fragments.end(&cx).full_offset();
            if fragment_end == range.start && fragment_end > fragment_start {
                let mut fragment = old_fragments.item().unwrap().clone();
                fragment.len = fragment_end.0 - fragment_start.0;
                new_ropes.push_fragment(&fragment, fragment.visible);
                new_fragments.push(fragment, &None);
                old_fragments.next(&cx);
                fragment_start = old_fragments.start().full_offset();
            }

            // Skip over insertions that are concurrent to this edit, but have a lower lamport
            // timestamp.
            while let Some(fragment) = old_fragments.item() {
                if fragment_start == range.start
                    && fragment.timestamp.lamport() > timestamp.lamport()
                {
                    new_ropes.push_fragment(fragment, fragment.visible);
                    new_fragments.push(fragment.clone(), &None);
                    old_fragments.next(&cx);
                    debug_assert_eq!(fragment_start, range.start);
                } else {
                    break;
                }
            }
            debug_assert!(fragment_start <= range.start);

            // Preserve any portion of the current fragment that precedes this range.
            if fragment_start < range.start {
                let mut prefix = old_fragments.item().unwrap().clone();
                prefix.len = range.start.0 - fragment_start.0;
                fragment_start = range.start;
                new_ropes.push_fragment(&prefix, prefix.visible);
                new_fragments.push(prefix, &None);
            }

            // Insert the new text before any existing fragments within the range.
            if let Some(new_text) = new_text {
                new_ropes.push_str(new_text);
                new_fragments.push(
                    Fragment {
                        timestamp,
                        len: new_text.len(),
                        deletions: Default::default(),
                        max_undos: Default::default(),
                        visible: true,
                    },
                    &None,
                );
            }

            // Advance through every fragment that intersects this range, marking the intersecting
            // portions as deleted.
            while fragment_start < range.end {
                let fragment = old_fragments.item().unwrap();
                let fragment_end = old_fragments.end(&cx).full_offset();
                let mut intersection = fragment.clone();
                let intersection_end = cmp::min(range.end, fragment_end);
                if fragment.was_visible(version, &self.undo_map) {
                    intersection.len = intersection_end.0 - fragment_start.0;
                    intersection.deletions.insert(timestamp.local());
                    intersection.visible = false;
                }
                if intersection.len > 0 {
                    new_ropes.push_fragment(&intersection, fragment.visible);
                    new_fragments.push(intersection, &None);
                    fragment_start = intersection_end;
                }
                if fragment_end <= range.end {
                    old_fragments.next(&cx);
                }
            }
        }

        // If the current fragment has been partially consumed, then consume the rest of it
        // and advance to the next fragment before slicing.
        if fragment_start > old_fragments.start().full_offset() {
            let fragment_end = old_fragments.end(&cx).full_offset();
            if fragment_end > fragment_start {
                let mut suffix = old_fragments.item().unwrap().clone();
                suffix.len = fragment_end.0 - fragment_start.0;
                new_ropes.push_fragment(&suffix, suffix.visible);
                new_fragments.push(suffix, &None);
            }
            old_fragments.next(&cx);
        }

        let suffix = old_fragments.suffix(&cx);
        new_ropes.push_tree(suffix.summary().text);
        new_fragments.push_tree(suffix, &None);
        let (visible_text, deleted_text) = new_ropes.finish();
        drop(old_fragments);

        self.fragments = new_fragments;
        self.visible_text = visible_text;
        self.deleted_text = deleted_text;
        self.local_clock.observe(timestamp.local());
        self.lamport_clock.observe(timestamp.lamport());
    }

    fn apply_undo(&mut self, undo: &UndoOperation) -> Result<()> {
        self.undo_map.insert(undo);

        let mut cx = undo.version.clone();
        for edit_id in undo.counts.keys().copied() {
            cx.observe(edit_id);
        }
        let cx = Some(cx);

        let mut old_fragments = self.fragments.cursor::<VersionedFullOffset>();
        let mut new_fragments = old_fragments.slice(
            &VersionedFullOffset::Offset(undo.ranges[0].start),
            Bias::Right,
            &cx,
        );
        let mut new_ropes =
            RopeBuilder::new(self.visible_text.cursor(0), self.deleted_text.cursor(0));
        new_ropes.push_tree(new_fragments.summary().text);

        for range in &undo.ranges {
            let mut end_offset = old_fragments.end(&cx).full_offset();

            if end_offset < range.start {
                let preceding_fragments = old_fragments.slice(
                    &VersionedFullOffset::Offset(range.start),
                    Bias::Right,
                    &cx,
                );
                new_ropes.push_tree(preceding_fragments.summary().text);
                new_fragments.push_tree(preceding_fragments, &None);
            }

            while end_offset <= range.end {
                if let Some(fragment) = old_fragments.item() {
                    let mut fragment = fragment.clone();
                    let fragment_was_visible = fragment.visible;

                    if fragment.was_visible(&undo.version, &self.undo_map)
                        || undo.counts.contains_key(&fragment.timestamp.local())
                    {
                        fragment.visible = fragment.is_visible(&self.undo_map);
                        fragment.max_undos.observe(undo.id);
                    }
                    new_ropes.push_fragment(&fragment, fragment_was_visible);
                    new_fragments.push(fragment, &None);

                    old_fragments.next(&cx);
                    if end_offset == old_fragments.end(&cx).full_offset() {
                        let unseen_fragments = old_fragments.slice(
                            &VersionedFullOffset::Offset(end_offset),
                            Bias::Right,
                            &cx,
                        );
                        new_ropes.push_tree(unseen_fragments.summary().text);
                        new_fragments.push_tree(unseen_fragments, &None);
                    }
                    end_offset = old_fragments.end(&cx).full_offset();
                } else {
                    break;
                }
            }
        }

        let suffix = old_fragments.suffix(&cx);
        new_ropes.push_tree(suffix.summary().text);
        new_fragments.push_tree(suffix, &None);

        drop(old_fragments);
        let (visible_text, deleted_text) = new_ropes.finish();
        self.fragments = new_fragments;
        self.visible_text = visible_text;
        self.deleted_text = deleted_text;
        Ok(())
    }

    fn flush_deferred_ops(&mut self) -> Result<()> {
        self.deferred_replicas.clear();
        let mut deferred_ops = Vec::new();
        for op in self.deferred_ops.drain().cursor().cloned() {
            if self.can_apply_op(&op) {
                self.apply_op(op)?;
            } else {
                self.deferred_replicas.insert(op.replica_id());
                deferred_ops.push(op);
            }
        }
        self.deferred_ops.insert(deferred_ops);
        Ok(())
    }

    fn can_apply_op(&self, op: &Operation) -> bool {
        if self.deferred_replicas.contains(&op.replica_id()) {
            false
        } else {
            match op {
                Operation::Edit(edit) => self.version >= edit.version,
                Operation::Undo { undo, .. } => self.version >= undo.version,
                Operation::UpdateSelections { selections, .. } => {
                    self.version >= *selections.version()
                }
                Operation::RemoveSelections { .. } => true,
                Operation::SetActiveSelections { set_id, .. } => {
                    set_id.map_or(true, |set_id| self.selections.contains_key(&set_id))
                }
                #[cfg(test)]
                Operation::Test(_) => true,
            }
        }
    }

    pub fn peek_undo_stack(&self) -> Option<&Transaction> {
        self.history.undo_stack.last()
    }

    pub fn start_transaction(
        &mut self,
        selection_set_ids: impl IntoIterator<Item = SelectionSetId>,
    ) -> Result<()> {
        self.start_transaction_at(selection_set_ids, Instant::now())
    }

    pub fn start_transaction_at(
        &mut self,
        selection_set_ids: impl IntoIterator<Item = SelectionSetId>,
        now: Instant,
    ) -> Result<()> {
        let selections = selection_set_ids
            .into_iter()
            .map(|set_id| {
                let set = self
                    .selections
                    .get(&set_id)
                    .expect("invalid selection set id");
                (set_id, set.selections.clone())
            })
            .collect();
        self.history
            .start_transaction(self.version.clone(), selections, now);
        Ok(())
    }

    pub fn end_transaction(&mut self, selection_set_ids: impl IntoIterator<Item = SelectionSetId>) {
        self.end_transaction_at(selection_set_ids, Instant::now());
    }

    pub fn end_transaction_at(
        &mut self,
        selection_set_ids: impl IntoIterator<Item = SelectionSetId>,
        now: Instant,
    ) -> Option<clock::Global> {
        let selections = selection_set_ids
            .into_iter()
            .map(|set_id| {
                let set = self
                    .selections
                    .get(&set_id)
                    .expect("invalid selection set id");
                (set_id, set.selections.clone())
            })
            .collect();

        if let Some(transaction) = self.history.end_transaction(selections, now) {
            let since = transaction.start.clone();
            self.history.group();
            Some(since)
        } else {
            None
        }
    }

    pub fn remove_peer(&mut self, replica_id: ReplicaId) {
        self.selections
            .retain(|set_id, _| set_id.replica_id != replica_id)
    }

    pub fn base_text(&self) -> &Arc<str> {
        &self.history.base_text
    }

    pub fn history(&self) -> impl Iterator<Item = &EditOperation> {
        self.history.ops.values()
    }

    pub fn undo(&mut self) -> Vec<Operation> {
        let mut ops = Vec::new();
        if let Some(transaction) = self.history.pop_undo().cloned() {
            let selections = transaction.selections_before.clone();
            ops.push(self.undo_or_redo(transaction).unwrap());
            for (set_id, selections) in selections {
                ops.extend(self.restore_selection_set(set_id, selections));
            }
        }
        ops
    }

    pub fn redo(&mut self) -> Vec<Operation> {
        let mut ops = Vec::new();
        if let Some(transaction) = self.history.pop_redo().cloned() {
            let selections = transaction.selections_after.clone();
            ops.push(self.undo_or_redo(transaction).unwrap());
            for (set_id, selections) in selections {
                ops.extend(self.restore_selection_set(set_id, selections));
            }
        }
        ops
    }

    fn undo_or_redo(&mut self, transaction: Transaction) -> Result<Operation> {
        let mut counts = HashMap::default();
        for edit_id in transaction.edits {
            counts.insert(edit_id, self.undo_map.undo_count(edit_id) + 1);
        }

        let undo = UndoOperation {
            id: self.local_clock.tick(),
            counts,
            ranges: transaction.ranges,
            version: transaction.start.clone(),
        };
        self.apply_undo(&undo)?;
        self.version.observe(undo.id);

        Ok(Operation::Undo {
            undo,
            lamport_timestamp: self.lamport_clock.tick(),
        })
    }

    pub fn selection_set(&self, set_id: SelectionSetId) -> Result<&SelectionSet> {
        self.selections
            .get(&set_id)
            .ok_or_else(|| anyhow!("invalid selection set id {:?}", set_id))
    }

    pub fn selection_sets(&self) -> impl Iterator<Item = (&SelectionSetId, &SelectionSet)> {
        self.selections.iter()
    }

    fn build_selection_anchor_range_map<T: ToOffset>(
        &self,
        selections: &[Selection<T>],
    ) -> Arc<AnchorRangeMap<SelectionState>> {
        Arc::new(
            self.content()
                .anchor_range_map(selections.iter().map(|selection| {
                    let start = selection.start.to_offset(self);
                    let end = selection.end.to_offset(self);
                    let range = (start, Bias::Left)..(end, Bias::Left);
                    let state = SelectionState {
                        id: selection.id,
                        reversed: selection.reversed,
                        goal: selection.goal,
                    };
                    (range, state)
                })),
        )
    }

    pub fn update_selection_set<T: ToOffset>(
        &mut self,
        set_id: SelectionSetId,
        selections: &[Selection<T>],
    ) -> Result<Operation> {
        let selections = self.build_selection_anchor_range_map(selections);
        let set = self
            .selections
            .get_mut(&set_id)
            .ok_or_else(|| anyhow!("invalid selection set id {:?}", set_id))?;
        set.selections = selections.clone();
        Ok(Operation::UpdateSelections {
            set_id,
            selections,
            lamport_timestamp: self.lamport_clock.tick(),
        })
    }

    pub fn restore_selection_set(
        &mut self,
        set_id: SelectionSetId,
        selections: Arc<AnchorRangeMap<SelectionState>>,
    ) -> Result<Operation> {
        let set = self
            .selections
            .get_mut(&set_id)
            .ok_or_else(|| anyhow!("invalid selection set id {:?}", set_id))?;
        set.selections = selections.clone();
        Ok(Operation::UpdateSelections {
            set_id,
            selections,
            lamport_timestamp: self.lamport_clock.tick(),
        })
    }

    pub fn add_selection_set<T: ToOffset>(&mut self, selections: &[Selection<T>]) -> Operation {
        let selections = self.build_selection_anchor_range_map(selections);
        let set_id = self.lamport_clock.tick();
        self.selections.insert(
            set_id,
            SelectionSet {
                id: set_id,
                selections: selections.clone(),
                active: false,
            },
        );
        Operation::UpdateSelections {
            set_id,
            selections,
            lamport_timestamp: set_id,
        }
    }

    pub fn add_raw_selection_set(&mut self, id: SelectionSetId, selections: SelectionSet) {
        self.selections.insert(id, selections);
    }

    pub fn set_active_selection_set(
        &mut self,
        set_id: Option<SelectionSetId>,
    ) -> Result<Operation> {
        if let Some(set_id) = set_id {
            assert_eq!(set_id.replica_id, self.replica_id());
        }

        for (id, set) in &mut self.selections {
            if id.replica_id == self.local_clock.replica_id {
                if Some(*id) == set_id {
                    set.active = true;
                } else {
                    set.active = false;
                }
            }
        }

        Ok(Operation::SetActiveSelections {
            set_id,
            lamport_timestamp: self.lamport_clock.tick(),
        })
    }

    pub fn remove_selection_set(&mut self, set_id: SelectionSetId) -> Result<Operation> {
        self.selections
            .remove(&set_id)
            .ok_or_else(|| anyhow!("invalid selection set id {:?}", set_id))?;
        Ok(Operation::RemoveSelections {
            set_id,
            lamport_timestamp: self.lamport_clock.tick(),
        })
    }

    pub fn edits_since<'a, D>(
        &'a self,
        since: &'a clock::Global,
    ) -> impl 'a + Iterator<Item = Edit<D>>
    where
        D: 'a + TextDimension<'a> + Ord,
    {
        self.content().edits_since(since)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Buffer {
    fn random_byte_range(&mut self, start_offset: usize, rng: &mut impl rand::Rng) -> Range<usize> {
        let end = self.clip_offset(rng.gen_range(start_offset..=self.len()), Bias::Right);
        let start = self.clip_offset(rng.gen_range(start_offset..=end), Bias::Right);
        start..end
    }

    pub fn randomly_edit<T>(
        &mut self,
        rng: &mut T,
        old_range_count: usize,
    ) -> (Vec<Range<usize>>, String, Operation)
    where
        T: rand::Rng,
    {
        let mut old_ranges: Vec<Range<usize>> = Vec::new();
        for _ in 0..old_range_count {
            let last_end = old_ranges.last().map_or(0, |last_range| last_range.end + 1);
            if last_end > self.len() {
                break;
            }
            old_ranges.push(self.random_byte_range(last_end, rng));
        }
        let new_text_len = rng.gen_range(0..10);
        let new_text: String = crate::random_char_iter::RandomCharIter::new(&mut *rng)
            .take(new_text_len)
            .collect();
        log::info!(
            "mutating buffer {} at {:?}: {:?}",
            self.replica_id,
            old_ranges,
            new_text
        );
        let op = self.edit(old_ranges.iter().cloned(), new_text.as_str());
        (old_ranges, new_text, Operation::Edit(op))
    }

    pub fn randomly_mutate<T>(&mut self, rng: &mut T) -> Vec<Operation>
    where
        T: rand::Rng,
    {
        use rand::prelude::*;

        let mut ops = vec![self.randomly_edit(rng, 5).2];

        // Randomly add, remove or mutate selection sets.
        let replica_selection_sets = &self
            .selection_sets()
            .map(|(set_id, _)| *set_id)
            .filter(|set_id| self.replica_id == set_id.replica_id)
            .collect::<Vec<_>>();
        let set_id = replica_selection_sets.choose(rng);
        if set_id.is_some() && rng.gen_bool(1.0 / 6.0) {
            ops.push(self.remove_selection_set(*set_id.unwrap()).unwrap());
        } else {
            let mut ranges = Vec::new();
            for _ in 0..5 {
                ranges.push(self.random_byte_range(0, rng));
            }
            let new_selections = self.selections_from_ranges(ranges).unwrap();

            let op = if set_id.is_none() || rng.gen_bool(1.0 / 5.0) {
                self.add_selection_set(&new_selections)
            } else {
                self.update_selection_set(*set_id.unwrap(), &new_selections)
                    .unwrap()
            };
            ops.push(op);
        }

        ops
    }

    pub fn randomly_undo_redo(&mut self, rng: &mut impl rand::Rng) -> Vec<Operation> {
        use rand::prelude::*;

        let mut ops = Vec::new();
        for _ in 0..rng.gen_range(1..=5) {
            if let Some(transaction) = self.history.undo_stack.choose(rng).cloned() {
                log::info!(
                    "undoing buffer {} transaction {:?}",
                    self.replica_id,
                    transaction
                );
                ops.push(self.undo_or_redo(transaction).unwrap());
            }
        }
        ops
    }

    fn selections_from_ranges<I>(&self, ranges: I) -> Result<Vec<Selection<usize>>>
    where
        I: IntoIterator<Item = Range<usize>>,
    {
        use std::sync::atomic::{self, AtomicUsize};

        static NEXT_SELECTION_ID: AtomicUsize = AtomicUsize::new(0);

        let mut ranges = ranges.into_iter().collect::<Vec<_>>();
        ranges.sort_unstable_by_key(|range| range.start);

        let mut selections = Vec::<Selection<usize>>::with_capacity(ranges.len());
        for mut range in ranges {
            let mut reversed = false;
            if range.start > range.end {
                reversed = true;
                std::mem::swap(&mut range.start, &mut range.end);
            }

            if let Some(selection) = selections.last_mut() {
                if selection.end >= range.start {
                    selection.end = range.end;
                    continue;
                }
            }

            selections.push(Selection {
                id: NEXT_SELECTION_ID.fetch_add(1, atomic::Ordering::SeqCst),
                start: range.start,
                end: range.end,
                reversed,
                goal: SelectionGoal::None,
            });
        }
        Ok(selections)
    }

    #[cfg(test)]
    pub fn selection_ranges<'a, D>(&'a self, set_id: SelectionSetId) -> Result<Vec<Range<D>>>
    where
        D: 'a + TextDimension<'a>,
    {
        Ok(self
            .selection_set(set_id)?
            .selections(self)
            .map(move |selection| {
                if selection.reversed {
                    selection.end..selection.start
                } else {
                    selection.start..selection.end
                }
            })
            .collect())
    }

    #[cfg(test)]
    pub fn all_selection_ranges<'a, D>(
        &'a self,
    ) -> impl 'a + Iterator<Item = (SelectionSetId, Vec<Range<usize>>)>
    where
        D: 'a + TextDimension<'a>,
    {
        self.selections
            .keys()
            .map(move |set_id| (*set_id, self.selection_ranges(*set_id).unwrap()))
    }
}

#[derive(Clone)]
pub struct Snapshot {
    visible_text: Rope,
    deleted_text: Rope,
    undo_map: UndoMap,
    fragments: SumTree<Fragment>,
    version: clock::Global,
}

impl Snapshot {
    pub fn as_rope(&self) -> &Rope {
        &self.visible_text
    }

    pub fn len(&self) -> usize {
        self.visible_text.len()
    }

    pub fn line_len(&self, row: u32) -> u32 {
        self.content().line_len(row)
    }

    pub fn indent_column_for_line(&self, row: u32) -> u32 {
        self.content().indent_column_for_line(row)
    }

    pub fn text(&self) -> Rope {
        self.visible_text.clone()
    }

    pub fn text_summary(&self) -> TextSummary {
        self.visible_text.summary()
    }

    pub fn max_point(&self) -> Point {
        self.visible_text.max_point()
    }

    pub fn text_for_range<T: ToOffset>(&self, range: Range<T>) -> Chunks {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        self.visible_text.chunks_in_range(range)
    }

    pub fn text_summary_for_range<T>(&self, range: Range<T>) -> TextSummary
    where
        T: ToOffset,
    {
        let range = range.start.to_offset(self.content())..range.end.to_offset(self.content());
        self.content().text_summary_for_range(range)
    }

    pub fn point_for_offset(&self, offset: usize) -> Result<Point> {
        self.content().point_for_offset(offset)
    }

    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        self.visible_text.clip_offset(offset, bias)
    }

    pub fn clip_point(&self, point: Point, bias: Bias) -> Point {
        self.visible_text.clip_point(point, bias)
    }

    pub fn to_offset(&self, point: Point) -> usize {
        self.visible_text.point_to_offset(point)
    }

    pub fn to_point(&self, offset: usize) -> Point {
        self.visible_text.offset_to_point(offset)
    }

    pub fn anchor_before<T: ToOffset>(&self, position: T) -> Anchor {
        self.content().anchor_at(position, Bias::Left)
    }

    pub fn anchor_after<T: ToOffset>(&self, position: T) -> Anchor {
        self.content().anchor_at(position, Bias::Right)
    }

    pub fn edits_since<'a, D>(
        &'a self,
        since: &'a clock::Global,
    ) -> impl 'a + Iterator<Item = Edit<D>>
    where
        D: 'a + TextDimension<'a> + Ord,
    {
        self.content().edits_since(since)
    }

    pub fn version(&self) -> &clock::Global {
        &self.version
    }

    pub fn content(&self) -> Content {
        self.into()
    }
}

#[derive(Clone)]
pub struct Content<'a> {
    visible_text: &'a Rope,
    deleted_text: &'a Rope,
    undo_map: &'a UndoMap,
    fragments: &'a SumTree<Fragment>,
    version: &'a clock::Global,
}

impl<'a> From<&'a Snapshot> for Content<'a> {
    fn from(snapshot: &'a Snapshot) -> Self {
        Self {
            visible_text: &snapshot.visible_text,
            deleted_text: &snapshot.deleted_text,
            undo_map: &snapshot.undo_map,
            fragments: &snapshot.fragments,
            version: &snapshot.version,
        }
    }
}

impl<'a> From<&'a Buffer> for Content<'a> {
    fn from(buffer: &'a Buffer) -> Self {
        Self {
            visible_text: &buffer.visible_text,
            deleted_text: &buffer.deleted_text,
            undo_map: &buffer.undo_map,
            fragments: &buffer.fragments,
            version: &buffer.version,
        }
    }
}

impl<'a> From<&'a mut Buffer> for Content<'a> {
    fn from(buffer: &'a mut Buffer) -> Self {
        Self {
            visible_text: &buffer.visible_text,
            deleted_text: &buffer.deleted_text,
            undo_map: &buffer.undo_map,
            fragments: &buffer.fragments,
            version: &buffer.version,
        }
    }
}

impl<'a> From<&'a Content<'a>> for Content<'a> {
    fn from(content: &'a Content) -> Self {
        Self {
            visible_text: &content.visible_text,
            deleted_text: &content.deleted_text,
            undo_map: &content.undo_map,
            fragments: &content.fragments,
            version: &content.version,
        }
    }
}

impl<'a> Content<'a> {
    fn max_point(&self) -> Point {
        self.visible_text.max_point()
    }

    fn len(&self) -> usize {
        self.fragments.extent::<usize>(&None)
    }

    pub fn chars_at<T: ToOffset>(&self, position: T) -> impl Iterator<Item = char> + 'a {
        let offset = position.to_offset(self);
        self.visible_text.chars_at(offset)
    }

    pub fn reversed_chars_at<T: ToOffset>(&self, position: T) -> impl Iterator<Item = char> + 'a {
        let offset = position.to_offset(self);
        self.visible_text.reversed_chars_at(offset)
    }

    pub fn text_for_range<T: ToOffset>(&self, range: Range<T>) -> Chunks<'a> {
        let start = range.start.to_offset(self);
        let end = range.end.to_offset(self);
        self.visible_text.chunks_in_range(start..end)
    }

    fn line_len(&self, row: u32) -> u32 {
        let row_start_offset = Point::new(row, 0).to_offset(self);
        let row_end_offset = if row >= self.max_point().row {
            self.len()
        } else {
            Point::new(row + 1, 0).to_offset(self) - 1
        };
        (row_end_offset - row_start_offset) as u32
    }

    pub fn indent_column_for_line(&self, row: u32) -> u32 {
        let mut result = 0;
        for c in self.chars_at(Point::new(row, 0)) {
            if c == ' ' {
                result += 1;
            } else {
                break;
            }
        }
        result
    }

    fn summary_for_anchor<D>(&self, anchor: &Anchor) -> D
    where
        D: TextDimension<'a>,
    {
        let cx = Some(anchor.version.clone());
        let mut cursor = self.fragments.cursor::<(VersionedFullOffset, usize)>();
        cursor.seek(
            &VersionedFullOffset::Offset(anchor.full_offset),
            anchor.bias,
            &cx,
        );
        let overshoot = if cursor.item().map_or(false, |fragment| fragment.visible) {
            anchor.full_offset - cursor.start().0.full_offset()
        } else {
            0
        };
        self.text_summary_for_range(0..cursor.start().1 + overshoot)
    }

    fn text_summary_for_range<D>(&self, range: Range<usize>) -> D
    where
        D: TextDimension<'a>,
    {
        self.visible_text.cursor(range.start).summary(range.end)
    }

    fn summaries_for_anchors<D, T>(&self, map: &'a AnchorMap<T>) -> impl Iterator<Item = (D, &'a T)>
    where
        D: TextDimension<'a>,
    {
        let cx = Some(map.version.clone());
        let mut summary = D::default();
        let mut rope_cursor = self.visible_text.cursor(0);
        let mut cursor = self.fragments.cursor::<(VersionedFullOffset, usize)>();
        map.entries.iter().map(move |((offset, bias), value)| {
            cursor.seek_forward(&VersionedFullOffset::Offset(*offset), *bias, &cx);
            let overshoot = if cursor.item().map_or(false, |fragment| fragment.visible) {
                *offset - cursor.start().0.full_offset()
            } else {
                0
            };
            summary.add_assign(&rope_cursor.summary(cursor.start().1 + overshoot));
            (summary.clone(), value)
        })
    }

    fn summaries_for_anchor_ranges<D, T>(
        &self,
        map: &'a AnchorRangeMap<T>,
    ) -> impl Iterator<Item = (Range<D>, &'a T)>
    where
        D: TextDimension<'a>,
    {
        let cx = Some(map.version.clone());
        let mut summary = D::default();
        let mut rope_cursor = self.visible_text.cursor(0);
        let mut cursor = self.fragments.cursor::<(VersionedFullOffset, usize)>();
        map.entries.iter().map(move |(range, value)| {
            let Range {
                start: (start_offset, start_bias),
                end: (end_offset, end_bias),
            } = range;

            cursor.seek_forward(
                &VersionedFullOffset::Offset(*start_offset),
                *start_bias,
                &cx,
            );
            let overshoot = if cursor.item().map_or(false, |fragment| fragment.visible) {
                *start_offset - cursor.start().0.full_offset()
            } else {
                0
            };
            summary.add_assign(&rope_cursor.summary::<D>(cursor.start().1 + overshoot));
            let start_summary = summary.clone();

            cursor.seek_forward(&VersionedFullOffset::Offset(*end_offset), *end_bias, &cx);
            let overshoot = if cursor.item().map_or(false, |fragment| fragment.visible) {
                *end_offset - cursor.start().0.full_offset()
            } else {
                0
            };
            summary.add_assign(&rope_cursor.summary::<D>(cursor.start().1 + overshoot));
            let end_summary = summary.clone();

            (start_summary..end_summary, value)
        })
    }

    fn anchor_at<T: ToOffset>(&self, position: T, bias: Bias) -> Anchor {
        Anchor {
            full_offset: position.to_full_offset(self, bias),
            bias,
            version: self.version.clone(),
        }
    }

    pub fn anchor_map<T, E>(&self, entries: E) -> AnchorMap<T>
    where
        E: IntoIterator<Item = ((usize, Bias), T)>,
    {
        let version = self.version.clone();
        let mut cursor = self.fragments.cursor::<FragmentTextSummary>();
        let entries = entries
            .into_iter()
            .map(|((offset, bias), value)| {
                cursor.seek_forward(&offset, bias, &None);
                let full_offset = FullOffset(cursor.start().deleted + offset);
                ((full_offset, bias), value)
            })
            .collect();

        AnchorMap { version, entries }
    }

    pub fn anchor_range_map<T, E>(&self, entries: E) -> AnchorRangeMap<T>
    where
        E: IntoIterator<Item = (Range<(usize, Bias)>, T)>,
    {
        let version = self.version.clone();
        let mut cursor = self.fragments.cursor::<FragmentTextSummary>();
        let entries = entries
            .into_iter()
            .map(|(range, value)| {
                let Range {
                    start: (start_offset, start_bias),
                    end: (end_offset, end_bias),
                } = range;
                cursor.seek_forward(&start_offset, start_bias, &None);
                let full_start_offset = FullOffset(cursor.start().deleted + start_offset);
                cursor.seek_forward(&end_offset, end_bias, &None);
                let full_end_offset = FullOffset(cursor.start().deleted + end_offset);
                (
                    (full_start_offset, start_bias)..(full_end_offset, end_bias),
                    value,
                )
            })
            .collect();

        AnchorRangeMap { version, entries }
    }

    pub fn anchor_set<E>(&self, entries: E) -> AnchorSet
    where
        E: IntoIterator<Item = (usize, Bias)>,
    {
        AnchorSet(self.anchor_map(entries.into_iter().map(|range| (range, ()))))
    }

    pub fn anchor_range_set<E>(&self, entries: E) -> AnchorRangeSet
    where
        E: IntoIterator<Item = Range<(usize, Bias)>>,
    {
        AnchorRangeSet(self.anchor_range_map(entries.into_iter().map(|range| (range, ()))))
    }

    pub fn anchor_range_multimap<T, E, O>(
        &self,
        start_bias: Bias,
        end_bias: Bias,
        entries: E,
    ) -> AnchorRangeMultimap<T>
    where
        T: Clone,
        E: IntoIterator<Item = (Range<O>, T)>,
        O: ToOffset,
    {
        let mut entries = entries
            .into_iter()
            .map(|(range, value)| AnchorRangeMultimapEntry {
                range: FullOffsetRange {
                    start: range.start.to_full_offset(self, start_bias),
                    end: range.end.to_full_offset(self, end_bias),
                },
                value,
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|i| (i.range.start, Reverse(i.range.end)));
        AnchorRangeMultimap {
            entries: SumTree::from_iter(entries, &()),
            version: self.version.clone(),
            start_bias,
            end_bias,
        }
    }

    fn full_offset_for_anchor(&self, anchor: &Anchor) -> FullOffset {
        let cx = Some(anchor.version.clone());
        let mut cursor = self
            .fragments
            .cursor::<(VersionedFullOffset, FragmentTextSummary)>();
        cursor.seek(
            &VersionedFullOffset::Offset(anchor.full_offset),
            anchor.bias,
            &cx,
        );
        let overshoot = if cursor.item().is_some() {
            anchor.full_offset - cursor.start().0.full_offset()
        } else {
            0
        };
        let summary = cursor.start().1;
        FullOffset(summary.visible + summary.deleted + overshoot)
    }

    pub fn clip_point(&self, point: Point, bias: Bias) -> Point {
        self.visible_text.clip_point(point, bias)
    }

    pub fn clip_point_utf16(&self, point: PointUtf16, bias: Bias) -> PointUtf16 {
        self.visible_text.clip_point_utf16(point, bias)
    }

    fn point_for_offset(&self, offset: usize) -> Result<Point> {
        if offset <= self.len() {
            Ok(self.text_summary_for_range(0..offset))
        } else {
            Err(anyhow!("offset out of bounds"))
        }
    }

    pub fn edits_since<D>(&self, since: &'a clock::Global) -> impl 'a + Iterator<Item = Edit<D>>
    where
        D: 'a + TextDimension<'a> + Ord,
    {
        let fragments_cursor = if since == self.version {
            None
        } else {
            Some(self.fragments.filter(
                move |summary| summary.max_version.changed_since(since),
                &None,
            ))
        };

        Edits {
            visible_cursor: self.visible_text.cursor(0),
            deleted_cursor: self.deleted_text.cursor(0),
            fragments_cursor,
            undos: &self.undo_map,
            since,
            old_end: Default::default(),
            new_end: Default::default(),
        }
    }
}

struct RopeBuilder<'a> {
    old_visible_cursor: rope::Cursor<'a>,
    old_deleted_cursor: rope::Cursor<'a>,
    new_visible: Rope,
    new_deleted: Rope,
}

impl<'a> RopeBuilder<'a> {
    fn new(old_visible_cursor: rope::Cursor<'a>, old_deleted_cursor: rope::Cursor<'a>) -> Self {
        Self {
            old_visible_cursor,
            old_deleted_cursor,
            new_visible: Rope::new(),
            new_deleted: Rope::new(),
        }
    }

    fn push_tree(&mut self, len: FragmentTextSummary) {
        self.push(len.visible, true, true);
        self.push(len.deleted, false, false);
    }

    fn push_fragment(&mut self, fragment: &Fragment, was_visible: bool) {
        debug_assert!(fragment.len > 0);
        self.push(fragment.len, was_visible, fragment.visible)
    }

    fn push(&mut self, len: usize, was_visible: bool, is_visible: bool) {
        let text = if was_visible {
            self.old_visible_cursor
                .slice(self.old_visible_cursor.offset() + len)
        } else {
            self.old_deleted_cursor
                .slice(self.old_deleted_cursor.offset() + len)
        };
        if is_visible {
            self.new_visible.append(text);
        } else {
            self.new_deleted.append(text);
        }
    }

    fn push_str(&mut self, text: &str) {
        self.new_visible.push(text);
    }

    fn finish(mut self) -> (Rope, Rope) {
        self.new_visible.append(self.old_visible_cursor.suffix());
        self.new_deleted.append(self.old_deleted_cursor.suffix());
        (self.new_visible, self.new_deleted)
    }
}

impl<'a, D: TextDimension<'a> + Ord, F: FnMut(&FragmentSummary) -> bool> Iterator
    for Edits<'a, D, F>
{
    type Item = Edit<D>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut pending_edit: Option<Edit<D>> = None;
        let cursor = self.fragments_cursor.as_mut()?;

        while let Some(fragment) = cursor.item() {
            let summary = self.visible_cursor.summary(cursor.start().visible);
            self.old_end.add_assign(&summary);
            self.new_end.add_assign(&summary);
            if pending_edit
                .as_ref()
                .map_or(false, |change| change.new.end < self.new_end)
            {
                break;
            }

            if !fragment.was_visible(&self.since, &self.undos) && fragment.visible {
                let fragment_summary = self.visible_cursor.summary(cursor.end(&None).visible);
                let mut new_end = self.new_end.clone();
                new_end.add_assign(&fragment_summary);
                if let Some(pending_edit) = pending_edit.as_mut() {
                    pending_edit.new.end = new_end.clone();
                } else {
                    pending_edit = Some(Edit {
                        old: self.old_end.clone()..self.old_end.clone(),
                        new: self.new_end.clone()..new_end.clone(),
                    });
                }

                self.new_end = new_end;
            } else if fragment.was_visible(&self.since, &self.undos) && !fragment.visible {
                self.deleted_cursor.seek_forward(cursor.start().deleted);
                let fragment_summary = self.deleted_cursor.summary(cursor.end(&None).deleted);
                let mut old_end = self.old_end.clone();
                old_end.add_assign(&fragment_summary);
                if let Some(pending_edit) = pending_edit.as_mut() {
                    pending_edit.old.end = old_end.clone();
                } else {
                    pending_edit = Some(Edit {
                        old: self.old_end.clone()..old_end.clone(),
                        new: self.new_end.clone()..self.new_end.clone(),
                    });
                }

                self.old_end = old_end;
            }

            cursor.next(&None);
        }

        pending_edit
    }
}

impl Fragment {
    fn is_visible(&self, undos: &UndoMap) -> bool {
        !undos.is_undone(self.timestamp.local())
            && self.deletions.iter().all(|d| undos.is_undone(*d))
    }

    fn was_visible(&self, version: &clock::Global, undos: &UndoMap) -> bool {
        (version.observed(self.timestamp.local())
            && !undos.was_undone(self.timestamp.local(), version))
            && self
                .deletions
                .iter()
                .all(|d| !version.observed(*d) || undos.was_undone(*d, version))
    }
}

impl sum_tree::Item for Fragment {
    type Summary = FragmentSummary;

    fn summary(&self) -> Self::Summary {
        let mut max_version = clock::Global::new();
        max_version.observe(self.timestamp.local());
        for deletion in &self.deletions {
            max_version.observe(*deletion);
        }
        max_version.join(&self.max_undos);

        let mut min_insertion_version = clock::Global::new();
        min_insertion_version.observe(self.timestamp.local());
        let max_insertion_version = min_insertion_version.clone();
        if self.visible {
            FragmentSummary {
                text: FragmentTextSummary {
                    visible: self.len,
                    deleted: 0,
                },
                max_version,
                min_insertion_version,
                max_insertion_version,
            }
        } else {
            FragmentSummary {
                text: FragmentTextSummary {
                    visible: 0,
                    deleted: self.len,
                },
                max_version,
                min_insertion_version,
                max_insertion_version,
            }
        }
    }
}

impl sum_tree::Summary for FragmentSummary {
    type Context = Option<clock::Global>;

    fn add_summary(&mut self, other: &Self, _: &Self::Context) {
        self.text.visible += &other.text.visible;
        self.text.deleted += &other.text.deleted;
        self.max_version.join(&other.max_version);
        self.min_insertion_version
            .meet(&other.min_insertion_version);
        self.max_insertion_version
            .join(&other.max_insertion_version);
    }
}

impl Default for FragmentSummary {
    fn default() -> Self {
        FragmentSummary {
            text: FragmentTextSummary::default(),
            max_version: clock::Global::new(),
            min_insertion_version: clock::Global::new(),
            max_insertion_version: clock::Global::new(),
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FullOffset(pub usize);

impl FullOffset {
    const MAX: Self = FullOffset(usize::MAX);
}

impl ops::AddAssign<usize> for FullOffset {
    fn add_assign(&mut self, rhs: usize) {
        self.0 += rhs;
    }
}

impl ops::Add<usize> for FullOffset {
    type Output = Self;

    fn add(mut self, rhs: usize) -> Self::Output {
        self += rhs;
        self
    }
}

impl ops::Sub for FullOffset {
    type Output = usize;

    fn sub(self, rhs: Self) -> Self::Output {
        self.0 - rhs.0
    }
}

impl<'a> sum_tree::Dimension<'a, FragmentSummary> for usize {
    fn add_summary(&mut self, summary: &FragmentSummary, _: &Option<clock::Global>) {
        *self += summary.text.visible;
    }
}

impl<'a> sum_tree::Dimension<'a, FragmentSummary> for FullOffset {
    fn add_summary(&mut self, summary: &FragmentSummary, _: &Option<clock::Global>) {
        self.0 += summary.text.visible + summary.text.deleted;
    }
}

impl<'a> sum_tree::SeekTarget<'a, FragmentSummary, FragmentTextSummary> for usize {
    fn cmp(
        &self,
        cursor_location: &FragmentTextSummary,
        _: &Option<clock::Global>,
    ) -> cmp::Ordering {
        Ord::cmp(self, &cursor_location.visible)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum VersionedFullOffset {
    Offset(FullOffset),
    Invalid,
}

impl VersionedFullOffset {
    fn full_offset(&self) -> FullOffset {
        if let Self::Offset(position) = self {
            *position
        } else {
            panic!("invalid version")
        }
    }
}

impl Default for VersionedFullOffset {
    fn default() -> Self {
        Self::Offset(Default::default())
    }
}

impl<'a> sum_tree::Dimension<'a, FragmentSummary> for VersionedFullOffset {
    fn add_summary(&mut self, summary: &'a FragmentSummary, cx: &Option<clock::Global>) {
        if let Self::Offset(offset) = self {
            let version = cx.as_ref().unwrap();
            if *version >= summary.max_insertion_version {
                *offset += summary.text.visible + summary.text.deleted;
            } else if !summary
                .min_insertion_version
                .iter()
                .all(|t| !version.observed(*t))
            {
                *self = Self::Invalid;
            }
        }
    }
}

impl<'a> sum_tree::SeekTarget<'a, FragmentSummary, Self> for VersionedFullOffset {
    fn cmp(&self, cursor_position: &Self, _: &Option<clock::Global>) -> cmp::Ordering {
        match (self, cursor_position) {
            (Self::Offset(a), Self::Offset(b)) => Ord::cmp(a, b),
            (Self::Offset(_), Self::Invalid) => cmp::Ordering::Less,
            (Self::Invalid, _) => unreachable!(),
        }
    }
}

impl Operation {
    fn replica_id(&self) -> ReplicaId {
        self.lamport_timestamp().replica_id
    }

    fn lamport_timestamp(&self) -> clock::Lamport {
        match self {
            Operation::Edit(edit) => edit.timestamp.lamport(),
            Operation::Undo {
                lamport_timestamp, ..
            } => *lamport_timestamp,
            Operation::UpdateSelections {
                lamport_timestamp, ..
            } => *lamport_timestamp,
            Operation::RemoveSelections {
                lamport_timestamp, ..
            } => *lamport_timestamp,
            Operation::SetActiveSelections {
                lamport_timestamp, ..
            } => *lamport_timestamp,
            #[cfg(test)]
            Operation::Test(lamport_timestamp) => *lamport_timestamp,
        }
    }

    pub fn is_edit(&self) -> bool {
        match self {
            Operation::Edit { .. } => true,
            _ => false,
        }
    }
}

pub trait ToOffset {
    fn to_offset<'a>(&self, content: impl Into<Content<'a>>) -> usize;

    fn to_full_offset<'a>(&self, content: impl Into<Content<'a>>, bias: Bias) -> FullOffset {
        let content = content.into();
        let offset = self.to_offset(&content);
        let mut cursor = content.fragments.cursor::<FragmentTextSummary>();
        cursor.seek(&offset, bias, &None);
        FullOffset(offset + cursor.start().deleted)
    }
}

impl ToOffset for Point {
    fn to_offset<'a>(&self, content: impl Into<Content<'a>>) -> usize {
        content.into().visible_text.point_to_offset(*self)
    }
}

impl ToOffset for PointUtf16 {
    fn to_offset<'a>(&self, content: impl Into<Content<'a>>) -> usize {
        content.into().visible_text.point_utf16_to_offset(*self)
    }
}

impl ToOffset for usize {
    fn to_offset<'a>(&self, content: impl Into<Content<'a>>) -> usize {
        assert!(*self <= content.into().len(), "offset is out of range");
        *self
    }
}

impl ToOffset for Anchor {
    fn to_offset<'a>(&self, content: impl Into<Content<'a>>) -> usize {
        content.into().summary_for_anchor(self)
    }
}

impl<'a> ToOffset for &'a Anchor {
    fn to_offset<'b>(&self, content: impl Into<Content<'b>>) -> usize {
        content.into().summary_for_anchor(self)
    }
}

pub trait ToPoint {
    fn to_point<'a>(&self, content: impl Into<Content<'a>>) -> Point;
}

impl ToPoint for Anchor {
    fn to_point<'a>(&self, content: impl Into<Content<'a>>) -> Point {
        content.into().summary_for_anchor(self)
    }
}

impl ToPoint for usize {
    fn to_point<'a>(&self, content: impl Into<Content<'a>>) -> Point {
        content.into().visible_text.offset_to_point(*self)
    }
}

impl ToPoint for Point {
    fn to_point<'a>(&self, _: impl Into<Content<'a>>) -> Point {
        *self
    }
}

pub trait FromAnchor {
    fn from_anchor<'a>(anchor: &Anchor, content: &Content<'a>) -> Self;
}

impl FromAnchor for Point {
    fn from_anchor<'a>(anchor: &Anchor, content: &Content<'a>) -> Self {
        anchor.to_point(content)
    }
}

impl FromAnchor for usize {
    fn from_anchor<'a>(anchor: &Anchor, content: &Content<'a>) -> Self {
        anchor.to_offset(content)
    }
}
