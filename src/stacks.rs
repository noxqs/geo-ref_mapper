//! Groups of pictures shown as a single tile, the way Lightroom does it.
//!
//! The library owns the data; this is the shape the interface needs it in —
//! "which stack is this picture in", "what does this stack contain", "is this
//! the cover" — each answerable without a query, because the Mosaic asks them
//! once per tile per frame.
//!
//! The rules, settled with morgan and enforced here as well as in the schema:
//!
//!   * a stack belongs to **one folder**
//!   * a picture belongs to **at most one stack**
//!   * a stack has **at least two** members; one is just a picture
//!   * the **cover** is the first member, and setting a cover is a reordering

use std::collections::HashMap;

/// Every stack, indexed for the questions the Mosaic asks.
#[derive(Clone, Default, Debug)]
pub struct Stacks {
    by_id: HashMap<i64, Vec<i64>>,
    /// Which stack each picture is in.
    of_image: HashMap<i64, i64>,
}

impl Stacks {
    pub fn from_rows(rows: Vec<crate::db::StackRow>) -> Self {
        let mut by_id = HashMap::new();
        let mut of_image = HashMap::new();
        for row in rows {
            for image in &row.members {
                of_image.insert(*image, row.id);
            }
            by_id.insert(row.id, row.members);
        }
        Self { by_id, of_image }
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// The stack a picture belongs to, if any.
    pub fn of(&self, image: i64) -> Option<i64> {
        self.of_image.get(&image).copied()
    }

    pub fn members(&self, stack: i64) -> &[i64] {
        self.by_id
            .get(&stack)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The picture that stands for the stack.
    pub fn cover(&self, stack: i64) -> Option<i64> {
        self.members(stack).first().copied()
    }

    /// True when this picture is the one shown for its stack. Every other
    /// member is hidden while stacks are collapsed.
    pub fn is_cover(&self, image: i64) -> bool {
        self.of(image)
            .and_then(|stack| self.cover(stack))
            .is_some_and(|cover| cover == image)
    }

    pub fn len(&self, stack: i64) -> usize {
        self.members(stack).len()
    }

    /// Expands a selection so that picking a stack means picking everything in
    /// it — which is what Delete and Export both act on.
    ///
    /// Only whole stacks expand: a member selected on its own stays itself, so
    /// deleting one picture from inside an open stack deletes one picture.
    pub fn expand_covers(&self, images: &[i64]) -> Vec<i64> {
        let mut out = Vec::new();
        for image in images {
            match self.of(*image) {
                Some(stack) if self.is_cover(*image) => out.extend_from_slice(self.members(stack)),
                _ => out.push(*image),
            }
        }
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stacks() -> Stacks {
        Stacks::from_rows(vec![crate::db::StackRow {
            id: 7,
            folder: "/photos".to_owned(),
            members: vec![10, 11, 12],
        }])
    }

    #[test]
    fn the_first_member_is_the_cover() {
        let stacks = stacks();
        assert_eq!(stacks.cover(7), Some(10));
        assert!(stacks.is_cover(10));
        assert!(!stacks.is_cover(11), "the rest are hidden behind it");
        assert_eq!(stacks.len(7), 3);
        assert_eq!(stacks.of(12), Some(7));
        assert_eq!(stacks.of(99), None);
    }

    #[test]
    fn selecting_a_stack_selects_all_of_it_but_a_member_is_just_itself() {
        let stacks = stacks();
        // The cover stands for the whole stack: Delete and Export take all.
        assert_eq!(stacks.expand_covers(&[10]), vec![10, 11, 12]);
        // A member picked inside an open stack is one picture, not three —
        // which is what makes deleting one from inside a stack work.
        assert_eq!(stacks.expand_covers(&[11]), vec![11]);
        // Loose pictures pass through untouched.
        assert_eq!(stacks.expand_covers(&[42, 10]), vec![42, 10, 11, 12]);
    }
}
