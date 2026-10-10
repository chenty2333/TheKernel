// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn size_summary_prunes_thousands_of_unusable_gaps() {
    let mut index = GapIndex::new();
    for start in (0..8192).step_by(2) {
        index.occupy(start..start + 1);
    }
    let search = GapSearch::new(0, 8196, 4, 1).unwrap();
    assert_eq!(search.in_tree(index.root.as_deref()), None);
    assert_eq!(search.visited.get(), 1);
    assert_eq!(index.find(0, 8196, 4, 1), Some(8191));
    GapNode::check(&index.root, 0..index.tail_start);
}

#[test]
fn insertion_and_removal_keep_balanced_summaries_and_merge_the_tail() {
    let mut index = GapIndex::new();
    for step in 0..257 {
        let start = (step * 73 % 257) * 4 + 1;
        index.occupy(start..start + 2);
        GapNode::check(&index.root, 0..index.tail_start);
    }
    for step in 0..257 {
        let start = (step * 151 % 257) * 4 + 1;
        index.release(start..start + 2);
        GapNode::check(&index.root, 0..index.tail_start);
    }
    assert!(index.root.is_none());
    assert_eq!(index.tail_start, 0);
}

#[test]
fn every_small_layout_matches_an_exhaustive_aligned_first_fit_oracle() {
    for occupied in 0u16..256 {
        let mut index = GapIndex::new();
        for byte in 0..8 {
            if occupied & (1 << byte) != 0 {
                index.occupy(byte..byte + 1);
            }
        }
        GapNode::check(&index.root, 0..index.tail_start);
        for lower in 0usize..=8 {
            for upper in lower..=8 {
                for size in 1usize..=4 {
                    for align in [1, 2, 4] {
                        let expected = (lower..upper).find(|&start| {
                            size.is_multiple_of(align)
                                && start.is_multiple_of(align)
                                && start + size <= upper
                                && (start..start + size).all(|byte| occupied & (1 << byte) == 0)
                        });
                        assert_eq!(
                            index.find(lower, upper, size, align),
                            expected,
                            "layout={occupied:#x}, bounds={lower}..{upper}, size={size}, \
                             align={align}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn alignment_and_bounds_never_turn_overflow_into_a_low_address() {
    let mut index = GapIndex::new();
    index.occupy(0..1);
    index.occupy(6..8);
    index.occupy(14..16);
    assert_eq!(index.find(0, 16, 4, 4), Some(8));
    assert_eq!(index.find(0, 10, 4, 4), None);
    assert_eq!(
        index.find(usize::MAX - 2, usize::MAX, 1, 1),
        Some(usize::MAX - 2)
    );
    assert_eq!(index.find(usize::MAX - 2, usize::MAX, 4, 4), None);
    assert_eq!(index.find(0, 16, 0, 1), None);
    assert_eq!(index.find(0, 16, 1, 0), None);
    assert_eq!(index.find(0, 16, 6, 3), None);
    assert_eq!(index.find(8, 4, 1, 1), None);
}
