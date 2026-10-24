//! The edit overlay: changes layered over the immutable source.
//!
//! A source row is identified by the byte offset of its record, which never
//! changes while the file is open and does not depend on indexing progress.
//! Inserted rows and columns get ids. Every mutation is recorded as an
//! invertible [`Op`] for undo/redo. Nothing here ever copies the table: the
//! overlay holds only what changed.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// A row: a source record (by offset) or an inserted row (by id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RowRef {
    Src(u64),
    New(u32),
}

/// A column: a source field index or an inserted column (by id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ColRef {
    Src(u32),
    New(u32),
}

/// Rows inserted before the source record at this offset.
/// [`END`] anchors rows after the last record.
pub const END: u64 = u64::MAX;

/// A range of deleted source records `[start, end)` (record offsets) and
/// how many records it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deleted {
    pub start: u64,
    pub end: u64,
    pub records: u64,
}

/// One cell change, with the value before and after (`None` = not edited).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellChange {
    pub row: RowRef,
    pub col: ColRef,
    pub old: Option<String>,
    pub new: Option<String>,
}

/// An invertible change.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Cells(Vec<CellChange>),
    /// `ids` inserted at index `at` of the rows anchored before `anchor`.
    InsertRows {
        anchor: u64,
        at: usize,
        ids: Vec<u32>,
    },
    /// Inserted rows removed from their anchors (anchor, index, id) and
    /// source ranges deleted.
    DeleteRows {
        new_rows: Vec<(u64, usize, u32)>,
        ranges: Vec<Deleted>,
    },
    /// The column order before and after.
    Columns {
        before: Option<Vec<ColRef>>,
        after: Option<Vec<ColRef>>,
    },
    Batch(Vec<Op>),
}

/// Unsaved changes of one document.
#[derive(Clone, Debug, Default)]
pub struct Overlay {
    cells: BTreeMap<u64, BTreeMap<ColRef, String>>,
    new_rows: HashMap<u32, BTreeMap<ColRef, String>>,
    inserts: BTreeMap<u64, Vec<u32>>,
    deleted: BTreeMap<u64, Deleted>,
    columns: Option<Vec<ColRef>>,
    next_id: u32,
    undo: Vec<Op>,
    redo: Vec<Op>,
    generation: u64,
}

impl Overlay {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when there is nothing to save.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
            && self.inserts.is_empty()
            && self.deleted.is_empty()
            && self.columns.is_none()
    }

    /// Changes on every mutation (for caches and "unsaved" indicators).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Number of edited cells (source and inserted rows).
    pub fn edited_cells(&self) -> usize {
        self.cells.values().map(BTreeMap::len).sum::<usize>()
            + self.new_rows.values().map(BTreeMap::len).sum::<usize>()
    }

    pub fn inserted_rows(&self) -> usize {
        self.inserts.values().map(Vec::len).sum()
    }

    pub fn deleted_records(&self) -> u64 {
        self.deleted.values().map(|d| d.records).sum()
    }

    // ------------------------------------------------------------ queries

    /// The edited value of a cell, if any.
    pub fn cell(&self, row: RowRef, col: ColRef) -> Option<&str> {
        match row {
            RowRef::Src(o) => self.cells.get(&o)?.get(&col).map(String::as_str),
            RowRef::New(id) => self.new_rows.get(&id)?.get(&col).map(String::as_str),
        }
    }

    /// Edited cells of a source record.
    pub fn row_edits(&self, offset: u64) -> Option<&BTreeMap<ColRef, String>> {
        self.cells.get(&offset)
    }

    /// Cells of an inserted row.
    pub fn new_row(&self, id: u32) -> Option<&BTreeMap<ColRef, String>> {
        self.new_rows.get(&id)
    }

    /// Offsets of source records with edited cells, in order.
    pub fn edited_offsets(&self) -> impl Iterator<Item = u64> + '_ {
        self.cells.keys().copied()
    }

    /// Is the source record at `offset` deleted?
    pub fn is_deleted(&self, offset: u64) -> bool {
        self.deleted_range(offset).is_some()
    }

    /// The deleted range containing `offset`.
    pub fn deleted_range(&self, offset: u64) -> Option<Deleted> {
        self.deleted
            .range(..=offset)
            .next_back()
            .map(|(_, d)| *d)
            .filter(|d| offset < d.end)
    }

    pub fn deleted_ranges(&self) -> impl Iterator<Item = &Deleted> {
        self.deleted.values()
    }

    /// Rows inserted before the record at `anchor`.
    pub fn inserted_before(&self, anchor: u64) -> &[u32] {
        self.inserts.get(&anchor).map_or(&[], Vec::as_slice)
    }

    /// Anchors with inserted rows in `(after, upto]`, in order.
    pub fn anchors_between(
        &self,
        after: Option<u64>,
        upto: u64,
    ) -> impl Iterator<Item = (u64, &[u32])> {
        let lo = match after {
            Some(a) => std::ops::Bound::Excluded(a),
            None => std::ops::Bound::Unbounded,
        };
        self.inserts
            .range((lo, std::ops::Bound::Included(upto)))
            .map(|(k, v)| (*k, v.as_slice()))
    }

    /// All anchors with inserted rows.
    pub fn anchors(&self) -> impl Iterator<Item = (u64, &[u32])> {
        self.inserts.iter().map(|(k, v)| (*k, v.as_slice()))
    }

    /// Inserted rows anchored strictly before `offset` (for row numbers).
    pub fn inserted_count_before(&self, offset: u64) -> u64 {
        self.inserts
            .range(..offset)
            .map(|(_, v)| v.len() as u64)
            .sum()
    }

    /// Deleted records strictly before `offset` (for row numbers).
    pub fn deleted_count_before(&self, offset: u64) -> u64 {
        self.deleted
            .range(..offset)
            .map(|(_, d)| if d.end <= offset { d.records } else { 0 })
            .sum()
    }

    /// The column order, or `None` for source columns in order.
    pub fn columns(&self) -> Option<&[ColRef]> {
        self.columns.as_deref()
    }

    /// The column shown at logical index `c`.
    pub fn col_ref(&self, c: usize) -> ColRef {
        match &self.columns {
            None => ColRef::Src(c as u32),
            Some(cols) => cols.get(c).copied().unwrap_or_else(|| {
                // Ragged extras past the mapped columns pass through.
                let max_src = cols
                    .iter()
                    .filter_map(|r| {
                        if let ColRef::Src(i) = r {
                            Some(*i)
                        } else {
                            None
                        }
                    })
                    .max()
                    .map_or(0, |m| m + 1);
                ColRef::Src(max_src + (c - cols.len()) as u32)
            }),
        }
    }

    /// Immutable copy for background jobs.
    pub fn snapshot(&self) -> Arc<Overlay> {
        Arc::new(Overlay {
            undo: Vec::new(),
            redo: Vec::new(),
            ..self.clone()
        })
    }

    // ---------------------------------------------------------- mutation

    fn bump(&mut self) {
        self.generation += 1;
    }

    fn record(&mut self, op: Op) {
        self.undo.push(op);
        self.redo.clear();
        self.bump();
    }

    fn set_raw(&mut self, row: RowRef, col: ColRef, value: Option<String>) {
        match row {
            RowRef::Src(o) => match value {
                Some(v) => {
                    self.cells.entry(o).or_default().insert(col, v);
                }
                None => {
                    if let Some(m) = self.cells.get_mut(&o) {
                        m.remove(&col);
                        if m.is_empty() {
                            self.cells.remove(&o);
                        }
                    }
                }
            },
            RowRef::New(id) => {
                let m = self.new_rows.entry(id).or_default();
                match value {
                    Some(v) => {
                        m.insert(col, v);
                    }
                    None => {
                        m.remove(&col);
                    }
                }
            }
        }
    }

    /// Sets cells (one undo step). `original` gives a cell's source value,
    /// so that writing back the original removes the edit.
    pub fn set_cells(
        &mut self,
        changes: Vec<(RowRef, ColRef, String)>,
        original: impl Fn(RowRef, ColRef) -> Option<String>,
    ) {
        let mut ops = Vec::with_capacity(changes.len());
        for (row, col, value) in changes {
            let old = self.cell(row, col).map(str::to_string);
            let new = if matches!(row, RowRef::Src(_))
                && original(row, col).as_deref() == Some(value.as_str())
            {
                None
            } else {
                Some(value)
            };
            if old == new {
                continue;
            }
            self.set_raw(row, col, new.clone());
            ops.push(CellChange { row, col, old, new });
        }
        if !ops.is_empty() {
            self.record(Op::Cells(ops));
        }
    }

    /// Inserts `n` empty rows before the record at `anchor`, at index `at`
    /// among the rows already inserted there. Returns their ids.
    pub fn insert_rows(&mut self, anchor: u64, at: usize, n: usize) -> Vec<u32> {
        let ids: Vec<u32> = (0..n as u32).map(|k| self.next_id + k).collect();
        self.next_id += n as u32;
        self.apply_insert(anchor, at, &ids);
        self.record(Op::InsertRows {
            anchor,
            at,
            ids: ids.clone(),
        });
        ids
    }

    fn apply_insert(&mut self, anchor: u64, at: usize, ids: &[u32]) {
        let list = self.inserts.entry(anchor).or_default();
        let at = at.min(list.len());
        list.splice(at..at, ids.iter().copied());
    }

    /// Deletes inserted rows and source record ranges (one undo step).
    pub fn delete_rows(&mut self, new_rows: &[u32], ranges: &[Deleted]) {
        let mut removed = Vec::new();
        for &id in new_rows {
            if let Some((&anchor, list)) = self.inserts.iter_mut().find(|(_, l)| l.contains(&id)) {
                let i = list.iter().position(|&x| x == id).unwrap_or(0);
                list.remove(i);
                removed.push((anchor, i, id));
            }
        }
        self.inserts.retain(|_, l| !l.is_empty());
        let mut added = Vec::new();
        for r in ranges {
            if r.start < r.end && !self.overlaps_deleted(r) {
                self.deleted.insert(r.start, *r);
                added.push(*r);
            }
        }
        if !removed.is_empty() || !added.is_empty() {
            self.record(Op::DeleteRows {
                new_rows: removed,
                ranges: added,
            });
        }
    }

    fn overlaps_deleted(&self, r: &Deleted) -> bool {
        self.deleted
            .range(..r.end)
            .next_back()
            .is_some_and(|(_, d)| d.end > r.start)
    }

    /// Replaces the column order (one undo step). `None` restores the
    /// source order.
    pub fn set_columns(&mut self, after: Option<Vec<ColRef>>) {
        let before = self.columns.clone();
        if before == after {
            return;
        }
        self.columns = after.clone();
        self.record(Op::Columns { before, after });
    }

    /// A new column id (for inserting a column).
    pub fn new_column_id(&mut self) -> ColRef {
        let id = self.next_id;
        self.next_id += 1;
        ColRef::New(id)
    }

    /// Groups the operations recorded since `mark` (see [`Overlay::mark`])
    /// into one undo step.
    pub fn group_since(&mut self, mark: usize) {
        if self.undo.len() > mark + 1 {
            let ops = self.undo.split_off(mark);
            self.undo.push(Op::Batch(ops));
        }
    }

    /// Current depth of the undo stack.
    pub fn mark(&self) -> usize {
        self.undo.len()
    }

    // ------------------------------------------------------- undo / redo

    fn apply(&mut self, op: &Op, forward: bool) {
        match op {
            Op::Cells(changes) => {
                for c in changes {
                    self.set_raw(
                        c.row,
                        c.col,
                        if forward {
                            c.new.clone()
                        } else {
                            c.old.clone()
                        },
                    );
                }
            }
            Op::InsertRows { anchor, at, ids } => {
                if forward {
                    self.apply_insert(*anchor, *at, ids);
                } else if let Some(list) = self.inserts.get_mut(anchor) {
                    list.retain(|id| !ids.contains(id));
                    if list.is_empty() {
                        self.inserts.remove(anchor);
                    }
                }
            }
            Op::DeleteRows { new_rows, ranges } => {
                if forward {
                    for (anchor, _, id) in new_rows {
                        if let Some(list) = self.inserts.get_mut(anchor) {
                            list.retain(|x| x != id);
                        }
                    }
                    self.inserts.retain(|_, l| !l.is_empty());
                    for r in ranges {
                        self.deleted.insert(r.start, *r);
                    }
                } else {
                    for (anchor, i, id) in new_rows.iter().rev() {
                        self.apply_insert(*anchor, *i, &[*id]);
                    }
                    for r in ranges {
                        self.deleted.remove(&r.start);
                    }
                }
            }
            Op::Columns { before, after } => {
                self.columns = if forward {
                    after.clone()
                } else {
                    before.clone()
                }
            }
            Op::Batch(ops) => {
                if forward {
                    for o in ops {
                        self.apply(o, true);
                    }
                } else {
                    for o in ops.iter().rev() {
                        self.apply(o, false);
                    }
                }
            }
        }
    }

    pub fn undo(&mut self) -> bool {
        let Some(op) = self.undo.pop() else {
            return false;
        };
        self.apply(&op, false);
        self.redo.push(op);
        self.bump();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(op) = self.redo.pop() else {
            return false;
        };
        self.apply(&op, true);
        self.undo.push(op);
        self.bump();
        true
    }

    /// Drops every change and the undo history.
    pub fn clear(&mut self) {
        let generation = self.generation + 1;
        *self = Overlay {
            generation,
            next_id: self.next_id,
            ..Overlay::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: RowRef, _: ColRef) -> Option<String> {
        None
    }

    #[test]
    fn cell_edits_undo_redo() {
        let mut o = Overlay::new();
        let (r, c) = (RowRef::Src(100), ColRef::Src(2));
        o.set_cells(vec![(r, c, "x".into())], none);
        o.set_cells(vec![(r, c, "y".into())], none);
        assert_eq!(o.cell(r, c), Some("y"));
        assert!(o.undo());
        assert_eq!(o.cell(r, c), Some("x"));
        assert!(o.undo());
        assert_eq!(o.cell(r, c), None);
        assert!(o.is_empty());
        assert!(!o.undo());
        assert!(o.redo());
        assert!(o.redo());
        assert_eq!(o.cell(r, c), Some("y"));
        // A new edit clears the redo stack.
        o.undo();
        o.set_cells(vec![(r, c, "z".into())], none);
        assert!(!o.can_redo());
    }

    #[test]
    fn writing_the_original_value_removes_the_edit() {
        let mut o = Overlay::new();
        let (r, c) = (RowRef::Src(0), ColRef::Src(0));
        let orig = |_: RowRef, _: ColRef| Some("abc".to_string());
        o.set_cells(vec![(r, c, "new".into())], orig);
        assert_eq!(o.edited_cells(), 1);
        o.set_cells(vec![(r, c, "abc".into())], orig);
        assert_eq!(o.edited_cells(), 0);
        assert!(o.is_empty());
        o.undo();
        assert_eq!(o.cell(r, c), Some("new"));
    }

    #[test]
    fn rows_insert_delete_undo() {
        let mut o = Overlay::new();
        let ids = o.insert_rows(500, 0, 2);
        let more = o.insert_rows(500, 1, 1);
        assert_eq!(o.inserted_before(500), &[ids[0], more[0], ids[1]]);
        o.set_cells(
            vec![(RowRef::New(ids[0]), ColRef::Src(0), "v".into())],
            none,
        );
        o.delete_rows(
            &[more[0]],
            &[Deleted {
                start: 10,
                end: 90,
                records: 3,
            }],
        );
        assert_eq!(o.inserted_before(500), &[ids[0], ids[1]]);
        assert!(o.is_deleted(10) && o.is_deleted(89) && !o.is_deleted(90) && !o.is_deleted(9));
        assert_eq!(o.deleted_count_before(200), 3);
        assert_eq!(o.inserted_count_before(600), 2);
        o.undo();
        assert_eq!(o.inserted_before(500), &[ids[0], more[0], ids[1]]);
        assert!(!o.is_deleted(10));
        o.undo();
        o.undo();
        o.undo();
        assert!(o.is_empty());
        for _ in 0..4 {
            o.redo();
        }
        assert_eq!(o.inserted_before(500), &[ids[0], ids[1]]);
        assert_eq!(o.cell(RowRef::New(ids[0]), ColRef::Src(0)), Some("v"));
    }

    #[test]
    fn columns_and_batches() {
        let mut o = Overlay::new();
        let mark = o.mark();
        let nc = o.new_column_id();
        o.set_columns(Some(vec![ColRef::Src(1), nc, ColRef::Src(0)]));
        o.set_cells(vec![(RowRef::Src(0), nc, "head".into())], none);
        o.group_since(mark);
        assert_eq!(o.col_ref(1), nc);
        // Ragged extras after the mapped columns pass through.
        assert_eq!(o.col_ref(3), ColRef::Src(2));
        assert!(o.undo());
        assert!(o.columns().is_none());
        assert_eq!(o.cell(RowRef::Src(0), nc), None);
        assert!(o.redo());
        assert_eq!(o.cell(RowRef::Src(0), nc), Some("head"));
    }
}
