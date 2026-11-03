//! What the list has to be told when the rows change: the rows it was last given and the rows it
//! should show now, compared by key. Rows that kept their key and place need no splice (their
//! heights are right unless their `ver` moved); one splice covers the rows between the common
//! start and the common end. Pure, so it is tested without a window.

use std::ops::Range;

use super::turns::Row;

/// How to bring the list from one set of rows to another.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Replace these rows of the old set with `count` new ones (`list.splice`).
    pub splice: Option<(Range<usize>, usize)>,
    /// Rows of the new set that kept their place but changed (`list.remeasure_items`).
    pub remeasure: Vec<usize>,
}

/// The plan from `old` to `new`.
pub fn plan(old: &[Row], new: &[Row]) -> Plan {
    let head = old
        .iter()
        .zip(new)
        .take_while(|(a, b)| a.key == b.key)
        .count();
    let room = old.len().min(new.len()) - head;
    let tail = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(room)
        .take_while(|(a, b)| a.key == b.key)
        .count();

    let removed = head..old.len() - tail;
    let added = new.len() - tail - head;
    let splice = (!removed.is_empty() || added > 0).then_some((removed, added));

    let mut remeasure: Vec<usize> = (0..head).filter(|&i| old[i].ver != new[i].ver).collect();
    remeasure.extend(
        (0..tail)
            .map(|k| (old.len() - 1 - k, new.len() - 1 - k))
            .filter(|&(o, n)| old[o].ver != new[n].ver)
            .map(|(_, n)| n)
            .rev(),
    );
    Plan { splice, remeasure }
}

#[cfg(test)]
mod tests {
    use super::super::turns::Key;
    use super::*;

    fn rows(keys: &[(u64, u64)]) -> Vec<Row> {
        keys.iter()
            .map(|&(seq, ver)| Row {
                key: Key::Item(seq),
                ver,
            })
            .collect()
    }

    #[test]
    fn identical_rows_need_nothing() {
        let a = rows(&[(1, 1), (2, 1)]);
        assert_eq!(plan(&a, &a), Plan::default());
    }

    #[test]
    fn a_new_row_at_the_end_is_one_splice_there() {
        let p = plan(&rows(&[(1, 1), (2, 1)]), &rows(&[(1, 1), (2, 1), (3, 1)]));
        assert_eq!(p.splice, Some((2..2, 1)));
        assert!(p.remeasure.is_empty());
    }

    #[test]
    fn older_rows_land_in_front() {
        let p = plan(
            &rows(&[(5, 1), (6, 1)]),
            &rows(&[(3, 1), (4, 1), (5, 1), (6, 1)]),
        );
        assert_eq!(p.splice, Some((0..0, 2)));
        assert!(p.remeasure.is_empty());
    }

    #[test]
    fn a_grown_last_row_is_measured_again_not_replaced() {
        let p = plan(&rows(&[(1, 1), (2, 1)]), &rows(&[(1, 1), (2, 2)]));
        assert_eq!(p.splice, None);
        assert_eq!(p.remeasure, vec![1]);
    }

    #[test]
    fn a_changed_row_in_the_middle_keeps_its_neighbours() {
        let p = plan(
            &rows(&[(1, 1), (2, 1), (3, 1)]),
            &rows(&[(1, 1), (2, 2), (3, 2)]),
        );
        assert_eq!(p.splice, None);
        assert_eq!(p.remeasure, vec![1, 2]);
    }

    #[test]
    fn a_removed_row_is_a_splice_of_nothing() {
        let p = plan(&rows(&[(1, 1), (2, 1), (3, 1)]), &rows(&[(1, 1), (3, 1)]));
        assert_eq!(p.splice, Some((1..2, 0)));
    }

    #[test]
    fn a_fold_replacing_rows_splices_the_difference() {
        let old = rows(&[(1, 1), (2, 1), (3, 1), (4, 1)]);
        let new = vec![
            Row {
                key: Key::Item(1),
                ver: 1,
            },
            Row {
                key: Key::Fold(4),
                ver: 9,
            },
            Row {
                key: Key::Item(4),
                ver: 1,
            },
        ];
        let p = plan(&old, &new);
        assert_eq!(p.splice, Some((1..3, 1)));
        assert!(p.remeasure.is_empty());
    }

    #[test]
    fn from_nothing_and_to_nothing() {
        let a = rows(&[(1, 1), (2, 1)]);
        assert_eq!(plan(&[], &a).splice, Some((0..0, 2)));
        assert_eq!(plan(&a, &[]).splice, Some((0..2, 0)));
        assert_eq!(plan(&[], &[]), Plan::default());
    }
}
