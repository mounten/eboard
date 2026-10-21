//! Which parts of a frame changed: compares the triangle lists of two frames (Myers' diff) and
//! merges the bounds of the differing triangles into a few rectangles. Pure, tested.

use std::ops::Range;

use super::raster::ClipRect;

/// Indices of the items only in the old list (`removed`) and only in the new one (`added`). The
/// others appear in both, in the same order.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Edits {
    pub removed: Vec<usize>,
    pub added: Vec<usize>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Diff {
    Edits(Edits),
    /// Too many edits: everything in these ranges counts as changed.
    Ranges {
        old: Range<usize>,
        new: Range<usize>,
    },
}

/// Diffs `old` against `new`. After trimming the common prefix and suffix, Myers' algorithm
/// looks for the shortest edit script; if that has more than `max_edits` edits, everything
/// between prefix and suffix counts as changed. Takes about `len + max_edits²` steps.
pub(crate) fn diff<T>(old: &[T], new: &[T], eq: impl Fn(&T, &T) -> bool, max_edits: usize) -> Diff {
    let prefix = old.iter().zip(new).take_while(|(a, b)| eq(a, b)).count();
    let (old_rest, new_rest) = (&old[prefix..], &new[prefix..]);
    let suffix = old_rest
        .iter()
        .rev()
        .zip(new_rest.iter().rev())
        .take_while(|(a, b)| eq(a, b))
        .count();
    let a = &old_rest[..old_rest.len() - suffix];
    let b = &new_rest[..new_rest.len() - suffix];
    let Some(mut edits) = myers(a, b, &eq, max_edits) else {
        return Diff::Ranges {
            old: prefix..prefix + a.len(),
            new: prefix..prefix + b.len(),
        };
    };
    for i in edits.removed.iter_mut().chain(&mut edits.added) {
        *i += prefix;
    }
    Diff::Edits(edits)
}

/// Myers' O((N+M)D) diff, `None` if it needs more than `max_d` edits.
fn myers<T>(a: &[T], b: &[T], eq: &impl Fn(&T, &T) -> bool, max_d: usize) -> Option<Edits> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m).min(isize::try_from(max_d).unwrap_or(isize::MAX));
    // `v[k + offset]`: the furthest x reached on diagonal k = x - y.
    let offset = max + 1;
    let mut v = vec![0isize; 2 * max as usize + 3];
    // v for k in -d..=d before step d, at `trace[d * d..]` (steps before take 1 + 3 + 5 ...).
    let mut trace: Vec<isize> = Vec::new();
    for d in 0..=max {
        trace.extend_from_slice(&v[(offset - d) as usize..=(offset + d) as usize]);
        for k in (-d..=d).step_by(2) {
            let at = |k: isize| v[(k + offset) as usize];
            let mut x = if k == -d || (k != d && at(k - 1) < at(k + 1)) {
                at(k + 1) // down: insertion
            } else {
                at(k - 1) + 1 // right: deletion
            };
            let mut y = x - k;
            while x < n && y < m && eq(&a[x as usize], &b[y as usize]) {
                x += 1;
                y += 1;
            }
            v[(k + offset) as usize] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, d, n, m));
            }
        }
    }
    None
}

/// Walks back from `(n, m)`, reached in step `last`.
fn backtrack(trace: &[isize], last: isize, n: isize, m: isize) -> Edits {
    let mut edits = Edits::default();
    let (mut x, mut y) = (n, m);
    for d in (1..=last).rev() {
        let at = |k: isize| trace[(d * d + k + d) as usize];
        let k = x - y;
        let down = k == -d || (k != d && at(k - 1) < at(k + 1));
        let prev_k = if down { k + 1 } else { k - 1 };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        if down {
            edits.added.push(prev_y as usize);
        } else {
            edits.removed.push(prev_x as usize);
        }
        (x, y) = (prev_x, prev_y);
    }
    edits.removed.reverse();
    edits.added.reverse();
    edits
}

/// Rectangles closer than this are merged: redrawing the gap costs less than going over the
/// triangles once more.
const MERGE_GAP: i64 = 16;

/// Merges `rects` into at most `max` rectangles that cover them all and don't overlap each other.
pub(crate) fn merge_rects(rects: impl IntoIterator<Item = ClipRect>, max: usize) -> Vec<ClipRect> {
    let mut out: Vec<ClipRect> = Vec::new();
    for r in rects {
        if r.is_empty() {
            continue;
        }
        match out.iter_mut().find(|o| o.near(r, MERGE_GAP)) {
            Some(o) => *o = o.union(r),
            None => out.push(r),
        }
        if out.len() > max.max(1) {
            merge_cheapest_pair(&mut out);
        }
    }
    // A union can reach rectangles it did not touch before.
    while let Some((i, j)) = (0..out.len())
        .flat_map(|i| (i + 1..out.len()).map(move |j| (i, j)))
        .find(|&(i, j)| out[i].near(out[j], MERGE_GAP))
    {
        let r = out.swap_remove(j);
        out[i] = out[i].union(r);
    }
    out
}

/// Merges the two rectangles whose union adds the least area.
fn merge_cheapest_pair(rects: &mut Vec<ClipRect>) {
    let cost = |a: ClipRect, b: ClipRect| a.union(b).area() - a.area() - b.area();
    let pairs = (0..rects.len()).flat_map(|i| (i + 1..rects.len()).map(move |j| (i, j)));
    if let Some((i, j)) = pairs.min_by_key(|&(i, j)| cost(rects[i], rects[j])) {
        let r = rects.swap_remove(j);
        rects[i] = rects[i].union(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edits(diff: Diff) -> Edits {
        match diff {
            Diff::Edits(edits) => edits,
            Diff::Ranges { old, new } => Edits {
                removed: old.collect(),
                added: new.collect(),
            },
        }
    }

    /// Applies the edits: what is left of both lists must be the same.
    fn check(old: &[u8], new: &[u8], diff: Diff) -> Edits {
        let edits = edits(diff);
        let keep = |list: &[u8], gone: &[usize]| -> Vec<u8> {
            (0..list.len())
                .filter(|i| !gone.contains(i))
                .map(|i| list[i])
                .collect()
        };
        assert_eq!(
            keep(old, &edits.removed),
            keep(new, &edits.added),
            "{old:?} → {new:?}: {edits:?}"
        );
        edits
    }

    fn exact(old: &[u8], new: &[u8]) -> Edits {
        check(old, new, diff(old, new, |a, b| a == b, usize::MAX))
    }

    #[test]
    fn finds_the_shortest_edit() {
        let edits = exact(b"abcabba", b"cbabac");
        assert_eq!(edits.removed.len() + edits.added.len(), 5);
        assert_eq!(
            exact(b"header 12 body", b"header 13 body"),
            Edits {
                removed: vec![8],
                added: vec![8]
            }
        );
        // Two separate changes stay separate.
        assert_eq!(
            exact(b"x1yyyyyy2z", b"x3yyyyyy4z"),
            Edits {
                removed: vec![1, 8],
                added: vec![1, 8]
            }
        );
    }

    #[test]
    fn edge_cases() {
        for (old, new) in [
            (&b""[..], &b""[..]),
            (b"abc", b""),
            (b"", b"abc"),
            (b"abc", b"abc"),
            (b"abc", b"xyz"),
            (b"a", b"aaaa"),
        ] {
            exact(old, new);
        }
        assert_eq!(
            diff(b"abc", b"abc", |a, b| a == b, 0),
            Diff::Edits(Edits::default())
        );
    }

    #[test]
    fn random_lists() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..500 {
            let mut list = |len: u64| -> Vec<u8> {
                let len = next() % len;
                (0..len).map(|_| b"abc"[(next() % 3) as usize]).collect()
            };
            let (old, new) = (list(30), list(30));
            exact(&old, &new);
            check(&old, &new, diff(&old, &new, |a, b| a == b, 5));
        }
    }

    #[test]
    fn gives_up_after_max_edits() {
        let old = b"head 0123456789 tail";
        let new = b"head abcdefghij tail";
        // 20 edits needed.
        assert_eq!(exact(old, new).removed.len(), 10);
        assert_eq!(
            diff(old, new, |a, b| a == b, 19),
            Diff::Ranges {
                old: 5..15,
                new: 5..15
            }
        );
    }

    fn rect(x0: i64, y0: i64, x1: i64, y1: i64) -> ClipRect {
        ClipRect { x0, y0, x1, y1 }
    }

    #[test]
    fn merges_close_rects() {
        let merged = merge_rects([rect(0, 0, 10, 10), rect(20, 0, 30, 10)], 4);
        assert_eq!(merged, vec![rect(0, 0, 30, 10)]);
        let apart = merge_rects([rect(0, 0, 10, 10), rect(100, 100, 110, 110)], 4);
        assert_eq!(apart.len(), 2);
        assert_eq!(merge_rects([rect(5, 5, 5, 9)], 4), vec![]);
        // A union that reaches an earlier rect.
        let chain = merge_rects(
            [
                rect(0, 0, 10, 10),
                rect(200, 0, 210, 10),
                rect(5, 0, 205, 10),
            ],
            4,
        );
        assert_eq!(chain, vec![rect(0, 0, 210, 10)]);
    }

    #[test]
    fn keeps_at_most_max_rects() {
        let rects = (0..10).map(|i| rect(i * 100, i * 100, i * 100 + 10, i * 100 + 10));
        let merged = merge_rects(rects.clone(), 3);
        assert_eq!(merged.len(), 3);
        for r in rects {
            assert!(merged.iter().any(|m| m.union(r) == *m), "{r:?} lost");
        }
    }
}
