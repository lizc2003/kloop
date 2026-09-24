//! What `edit_file` can say when all three matching layers came up empty:
//! where in the file the `old_string` most nearly is, and which lines differ.
//!
//! Without it a miss says only "not found", and the usual reaction is to
//! retype `old_string` from memory — which costs a round trip and brings drift
//! of its own. A near match that clears the threshold hands the model the file's
//! own lines to copy; anything less close gets no guess at all, because a
//! plausible-looking wrong "closest match" is worse than none.

use super::fold;
use super::normalize_newlines;

/// Below this, the target is not in this file in any form worth pointing at.
const MIN_SIMILARITY_PERCENT: usize = 60;
/// A longer `old_string` is taken to be a different target altogether.
const MAX_OLD_CHARS: usize = 2_000;
const MAX_FILE_LINES: usize = 10_000;
/// More differing lines than this and a listing would only invite retyping
/// the block between them from memory; a read is the better advice.
pub(crate) const MAX_LISTED_LINES: usize = 3;
/// Quoted lines are cut here so one pathological line cannot flood the error.
const MAX_QUOTED_CHARS: usize = 120;
/// With no line of `old_string` surviving anywhere in the file, only a short
/// block is worth a scan of every window.
const MAX_UNSEEDED_OLD_CHARS: usize = 256;
const MAX_SEEDED_WINDOWS: usize = 256;
const MAX_DISTANCE_CELLS: usize = 30_000_000;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ClosestMatch {
    /// 1-based file line the nearest window starts on.
    pub start_line: usize,
    /// Lines in the file window, which may be one more or one fewer than
    /// `old_string` has.
    pub window_lines: usize,
    pub similarity_percent: usize,
    pub shape: ClosestShape,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ClosestShape {
    /// Few enough differences to list; `drift` is zero when the line counts
    /// agree.
    Listed {
        lines: Vec<LineDifference>,
        drift: LineDrift,
    },
    /// Too far apart to list: `changed_lines` in-place differences, `drift`
    /// whole lines one side has and the other lacks.
    TooFar {
        changed_lines: usize,
        drift: LineDrift,
    },
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct LineDifference {
    pub file_line: usize,
    /// 1-based line within `old_string`.
    pub your_line: usize,
    /// Both quoted as Rust debug strings, so a tab or a trailing space shows.
    pub file_text: String,
    pub your_text: String,
    pub first_difference: FirstDifference,
}

/// The first character (1-based column, in the model's line) where the two
/// lines stop agreeing once punctuation is folded. `None` on a side means that
/// line ends there.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct FirstDifference {
    pub column: usize,
    pub yours: Option<char>,
    pub file: Option<char>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct LineDrift {
    pub your_extra: usize,
    pub your_extra_all_blank: bool,
    pub file_extra: usize,
    pub file_extra_all_blank: bool,
}

impl LineDrift {
    pub(crate) fn lines(&self) -> usize {
        self.your_extra + self.file_extra
    }
}

pub(crate) fn closest_match(current: &str, old: &str) -> Option<ClosestMatch> {
    let old = normalize_newlines(old);
    let old_lines = trimmed_lines(&old);
    if old_lines.is_empty() {
        return None;
    }
    let folded_old: Vec<Vec<char>> = old_lines.iter().map(|line| fold(line).chars).collect();
    let old_block = join_lines(&folded_old);
    if old_block.is_empty() || old_block.len() > MAX_OLD_CHARS {
        return None;
    }

    let current = normalize_newlines(current);
    let file_lines = trimmed_lines(&current);
    let height = old_lines.len();
    if file_lines.len() > MAX_FILE_LINES || file_lines.len() < height {
        return None;
    }
    let folded_file: Vec<Vec<char>> = file_lines.iter().map(|line| fold(line).chars).collect();
    let mut prefix_chars = vec![0usize; folded_file.len() + 1];
    for (index, line) in folded_file.iter().enumerate() {
        prefix_chars[index + 1] = prefix_chars[index] + line.len();
    }
    let window_chars = |(start, lines): (usize, usize)| {
        lines - 1 + prefix_chars[start + lines] - prefix_chars[start]
    };

    let windows = candidate_windows(&folded_old, &folded_file, old_block.len(), window_chars)?;
    // similarity ≥ 60% ⇔ distance ≤ 40% of the block.
    let max_distance = old_block.len() * (100 - MIN_SIMILARITY_PERCENT) / 100;
    let mut best: Option<((usize, usize), usize)> = None;
    let mut cells = 0usize;
    for window in windows {
        let bound = match best {
            Some((_, 0)) => break,
            Some((_, distance)) => distance - 1,
            None => max_distance,
        };
        if window_chars(window).abs_diff(old_block.len()) > bound {
            continue;
        }
        cells += old_block.len() * (2 * bound + 1);
        if cells > MAX_DISTANCE_CELLS {
            break;
        }
        let (start, lines) = window;
        let text = join_lines(&folded_file[start..start + lines]);
        if let Some(distance) = banded_edit_distance(&old_block, &text, bound) {
            best = Some((window, distance));
        }
    }
    let ((start, window_lines), distance) = best?;
    let similarity_percent =
        ((old_block.len() - distance) * 100 + old_block.len() / 2) / old_block.len();

    let range = start..start + window_lines;
    let alignment = align(
        &old_lines,
        &folded_old,
        &file_lines[range.clone()],
        &folded_file[range],
        start,
    );
    let changed_lines = alignment.changed.len();
    let shape = if changed_lines > MAX_LISTED_LINES || alignment.drift.lines() > MAX_LISTED_LINES {
        ClosestShape::TooFar {
            changed_lines,
            drift: alignment.drift,
        }
    } else if changed_lines == 0 && alignment.drift.lines() == 0 {
        // Equal once folded, yet no layer matched: only trailing blank lines,
        // which this view trims, can differ. Nothing here would help.
        return None;
    } else {
        ClosestShape::Listed {
            lines: alignment.changed,
            drift: alignment.drift,
        }
    };
    Some(ClosestMatch {
        start_line: start + 1,
        window_lines,
        similarity_percent,
        shape,
    })
}

/// `(start, lines)` windows worth measuring, most promising first. Every line
/// of `old_string` found verbatim (folded) in the file is a seed, and the
/// rarest seed pins the fewest windows — so the cost follows the file's
/// repetition, not its length, and a match deep in the file is found as
/// readily as one near the top.
///
/// Heights one either side of the block's are tried too, with the start
/// allowed to slip by one: a block with one line too many or too few would
/// otherwise be measured against a window carrying a line of unrelated framing,
/// and a short block can drop below the threshold on that alone.
fn candidate_windows(
    folded_old: &[Vec<char>],
    folded_file: &[Vec<char>],
    old_chars: usize,
    window_chars: impl Fn((usize, usize)) -> usize,
) -> Option<Vec<(usize, usize)>> {
    let height = folded_old.len();
    let heights: Vec<usize> = [height, height - 1, height + 1]
        .into_iter()
        .filter(|lines| (1..=folded_file.len()).contains(lines))
        .collect();
    let seed = folded_old
        .iter()
        .enumerate()
        .map(|(offset, line)| {
            let hits: Vec<usize> = folded_file
                .iter()
                .enumerate()
                .filter(|(_, file_line)| *file_line == line)
                .map(|(index, _)| index)
                .collect();
            (offset, hits)
        })
        .filter(|(_, hits)| !hits.is_empty())
        .min_by_key(|(_, hits)| hits.len());
    let seeded = seed.is_some();
    let starts: Vec<usize> = match seed {
        Some((offset, hits)) => hits
            .into_iter()
            .filter_map(|hit| hit.checked_sub(offset))
            .flat_map(|start| [start, start + 1].into_iter().chain(start.checked_sub(1)))
            .collect(),
        None if old_chars <= MAX_UNSEEDED_OLD_CHARS => (0..folded_file.len()).collect(),
        None => return None,
    };
    let mut windows: Vec<(usize, usize)> = heights
        .iter()
        .flat_map(|&lines| starts.iter().map(move |&start| (start, lines)))
        .filter(|&(start, lines)| start + lines <= folded_file.len())
        .collect();
    let mut seen = std::collections::HashSet::new();
    windows.retain(|window| seen.insert(*window));
    if seeded && windows.len() > MAX_SEEDED_WINDOWS {
        // The cap decides which windows are reachable at all; rank by promise
        // so it does not quietly favour the top of the file.
        windows.sort_by_key(|window| window_chars(*window).abs_diff(old_chars));
        windows.truncate(MAX_SEEDED_WINDOWS);
    }
    Some(windows)
}

/// Lines of a logical-LF text, without the empty lines a trailing newline
/// (or several) leaves at the end.
fn trimmed_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

fn join_lines(lines: &[Vec<char>]) -> Vec<char> {
    let mut joined = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            joined.push('\n');
        }
        joined.extend_from_slice(line);
    }
    joined
}

/// Levenshtein distance when it is at most `max_distance`, computed only
/// inside that band so the cost is block length × band, not block × window.
fn banded_edit_distance(left: &[char], right: &[char], max_distance: usize) -> Option<usize> {
    if left.len().abs_diff(right.len()) > max_distance {
        return None;
    }
    let over = max_distance + 1;
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut row = vec![over; right.len() + 1];
    for i in 1..=left.len() {
        let low = i.saturating_sub(max_distance);
        let high = right.len().min(i + max_distance);
        row.fill(over);
        let mut left_cell = over;
        if low == 0 {
            left_cell = i;
            row[0] = i;
        }
        for j in low.max(1)..=high {
            let substitution = previous[j - 1] + usize::from(left[i - 1] != right[j - 1]);
            let value = substitution
                .min(previous[j] + 1)
                .min(left_cell + 1)
                .min(over);
            row[j] = value;
            left_cell = value;
        }
        std::mem::swap(&mut previous, &mut row);
    }
    let distance = previous[right.len()];
    (distance <= max_distance).then_some(distance)
}

struct Alignment {
    changed: Vec<LineDifference>,
    drift: LineDrift,
}

/// Pair the block's lines with the window's by a longest common subsequence
/// of folded lines. Position-by-position comparison cannot tell "one extra
/// blank line" from "every line after it differs": one shifted line would mask
/// the real difference behind a wall of phantom ones.
///
/// Between two anchors, leftover lines pair off in order as in-place changes
/// and only the surplus counts as drift (see [`split_gap`]). File lines left
/// over at the window's edges are the window's framing, not something the
/// model left out, so they are not drift.
fn align(
    your_lines: &[&str],
    your_folded: &[Vec<char>],
    file_lines: &[&str],
    file_folded: &[Vec<char>],
    window_start: usize,
) -> Alignment {
    let anchors = common_lines(your_folded, file_folded);
    let mut changed = Vec::new();
    let mut your_surplus: Vec<&str> = Vec::new();
    let mut file_surplus: Vec<&str> = Vec::new();
    let mut gap_start = (0usize, 0usize);
    let gap_count = anchors.len() + 1;
    for (gap, gap_end) in anchors
        .iter()
        .copied()
        .chain(std::iter::once((your_lines.len(), file_lines.len())))
        .enumerate()
    {
        let paired = (gap_end.0 - gap_start.0).min(gap_end.1 - gap_start.1);
        let leading = gap == 0;
        let trailing = gap == gap_count - 1;
        let (your_paired, your_left) =
            split_gap(your_lines, gap_start.0..gap_end.0, paired, leading);
        let (file_paired, file_left) =
            split_gap(file_lines, gap_start.1..gap_end.1, paired, leading);
        for (your_index, file_index) in your_paired.into_iter().zip(file_paired) {
            changed.push(LineDifference {
                file_line: window_start + file_index + 1,
                your_line: your_index + 1,
                file_text: quoted(file_lines[file_index]),
                your_text: quoted(your_lines[your_index]),
                first_difference: first_difference(your_lines[your_index], file_lines[file_index]),
            });
        }
        your_surplus.extend(your_left.into_iter().map(|index| your_lines[index]));
        // File lines left over at the window's edges are its framing.
        if !leading && !trailing {
            file_surplus.extend(file_left.into_iter().map(|index| file_lines[index]));
        }
        gap_start = (gap_end.0 + 1, gap_end.1 + 1);
    }
    let all_blank = |lines: &[&str]| lines.iter().all(|line| line.trim().is_empty());
    Alignment {
        changed,
        drift: LineDrift {
            your_extra: your_surplus.len(),
            your_extra_all_blank: !your_surplus.is_empty() && all_blank(&your_surplus),
            file_extra: file_surplus.len(),
            file_extra_all_blank: !file_surplus.is_empty() && all_blank(&file_surplus),
        },
    }
}

/// Pick which of a gap's lines pair off as in-place changes (`keep` of them,
/// in order) and which are surplus. Blank lines go to the surplus first — an
/// extra blank line is the commonest drift, and pairing it against a real
/// line would report a phantom change and hide the actual one. After that the
/// surplus comes from the far end: the start of the leading gap, whose anchor
/// is below it, and the end of every other.
fn split_gap(
    lines: &[&str],
    gap: std::ops::Range<usize>,
    keep: usize,
    leading: bool,
) -> (Vec<usize>, Vec<usize>) {
    let mut surplus_left = gap.len() - keep;
    let mut surplus = Vec::new();
    for index in gap.clone() {
        if surplus_left > 0 && lines[index].trim().is_empty() {
            surplus.push(index);
            surplus_left -= 1;
        }
    }
    let mut rest: Vec<usize> = gap.filter(|index| !surplus.contains(index)).collect();
    let far_end: Vec<usize> = if leading {
        rest.drain(..surplus_left).collect()
    } else {
        rest.split_off(rest.len() - surplus_left)
    };
    surplus.extend(far_end);
    surplus.sort_unstable();
    (rest, surplus)
}

/// Index pairs of one longest common subsequence, in order.
fn common_lines(left: &[Vec<char>], right: &[Vec<char>]) -> Vec<(usize, usize)> {
    let mut lengths = vec![vec![0usize; right.len() + 1]; left.len() + 1];
    for i in (0..left.len()).rev() {
        for j in (0..right.len()).rev() {
            lengths[i][j] = if left[i] == right[j] {
                lengths[i + 1][j + 1] + 1
            } else {
                lengths[i + 1][j].max(lengths[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0usize, 0usize);
    let mut pairs = Vec::new();
    while i < left.len() && j < right.len() {
        if left[i] == right[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if lengths[i + 1][j] >= lengths[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// Compared folded, so a curly quote early in the line does not hide the
/// difference that actually stopped the match; reported in the model's own
/// characters, at the model's own column.
fn first_difference(yours: &str, file: &str) -> FirstDifference {
    let your_folded = fold(yours);
    let file_folded = fold(file);
    let index = your_folded
        .chars
        .iter()
        .zip(&file_folded.chars)
        .take_while(|(left, right)| left == right)
        .count();
    let original = |text: &str, spans: &[super::Span]| {
        spans
            .get(index)
            .and_then(|span| text[span.start..].chars().next())
    };
    let column = match your_folded.spans.get(index) {
        Some(span) => yours[..span.start].chars().count() + 1,
        None => yours.chars().count() + 1,
    };
    FirstDifference {
        column,
        yours: original(yours, &your_folded.spans),
        file: original(file, &file_folded.spans),
    }
}

fn quoted(line: &str) -> String {
    match line.char_indices().nth(MAX_QUOTED_CHARS) {
        Some((cut, _)) => format!("{:?}...", &line[..cut]),
        None => format!("{line:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(range: std::ops::RangeInclusive<usize>) -> String {
        range
            .map(|n| format!("let value_{n} = compute({n});\n"))
            .collect()
    }

    #[test]
    fn one_character_off_lists_the_line_and_the_character() {
        let file = lines(1..=20);
        let old = "let value_7 = compute(7);\nlet value_8 = compute[8);\nlet value_9 = compute(9);";
        assert_eq!(
            closest_match(&file, old),
            Some(ClosestMatch {
                start_line: 7,
                window_lines: 3,
                similarity_percent: 99,
                shape: ClosestShape::Listed {
                    lines: vec![LineDifference {
                        file_line: 8,
                        your_line: 2,
                        file_text: "\"let value_8 = compute(8);\"".into(),
                        your_text: "\"let value_8 = compute[8);\"".into(),
                        first_difference: FirstDifference {
                            column: 22,
                            yours: Some('['),
                            file: Some('('),
                        },
                    }],
                    drift: LineDrift::default(),
                },
            })
        );
    }

    #[test]
    fn an_extra_blank_line_is_named_as_line_count_drift() {
        let file = lines(1..=20);
        let old =
            "let value_7 = compute(7);\n\nlet value_8 = compute(8);\nlet value_9 = compute(9);";
        assert_eq!(
            closest_match(&file, old),
            Some(ClosestMatch {
                start_line: 7,
                window_lines: 3,
                similarity_percent: 99,
                shape: ClosestShape::Listed {
                    lines: Vec::new(),
                    drift: LineDrift {
                        your_extra: 1,
                        your_extra_all_blank: true,
                        file_extra: 0,
                        file_extra_all_blank: false,
                    },
                },
            })
        );
    }

    #[test]
    fn more_than_three_changed_lines_is_too_far_to_list() {
        let file = lines(1..=20);
        let old = "let value_7 = compute(7);\nlet value_8 = compute(80);\nlet value_9 = compute(90);\n\
                   let value_10 = compute(100);\nlet value_11 = compute(110);";
        assert_eq!(
            closest_match(&file, old),
            Some(ClosestMatch {
                start_line: 7,
                window_lines: 5,
                similarity_percent: 97,
                shape: ClosestShape::TooFar {
                    changed_lines: 4,
                    drift: LineDrift::default(),
                },
            })
        );
    }

    #[test]
    fn more_than_three_whole_lines_of_drift_is_too_far_to_list() {
        let file = lines(1..=40);
        let old = lines(10..=30).replace("compute(20);\n", "compute(20);\n\n\n\n\n");
        assert_eq!(
            closest_match(&file, &old),
            Some(ClosestMatch {
                start_line: 9,
                window_lines: 24,
                similarity_percent: 85,
                shape: ClosestShape::TooFar {
                    changed_lines: 0,
                    drift: LineDrift {
                        your_extra: 4,
                        your_extra_all_blank: true,
                        file_extra: 0,
                        file_extra_all_blank: false,
                    },
                },
            })
        );
    }

    #[test]
    fn nothing_close_enough_gives_no_guess() {
        let file = lines(1..=20);
        assert_eq!(closest_match(&file, "fn entirely_unrelated() {}"), None);
    }

    #[test]
    fn an_oversized_old_string_is_not_searched() {
        let file = lines(1..=200);
        let old: String = lines(1..=100).replace("compute(50)", "compute(5O)");
        assert!(old.len() > MAX_OLD_CHARS);
        assert_eq!(closest_match(&file, &old), None);
    }

    #[test]
    fn crlf_files_compare_without_their_carriage_returns() {
        let file = lines(1..=5).replace('\n', "\r\n");
        let old = "let value_3 = compute(3):";
        assert_eq!(
            closest_match(&file, old),
            Some(ClosestMatch {
                start_line: 3,
                window_lines: 1,
                similarity_percent: 96,
                shape: ClosestShape::Listed {
                    lines: vec![LineDifference {
                        file_line: 3,
                        your_line: 1,
                        file_text: "\"let value_3 = compute(3);\"".into(),
                        your_text: "\"let value_3 = compute(3):\"".into(),
                        first_difference: FirstDifference {
                            column: 25,
                            yours: Some(':'),
                            file: Some(';'),
                        },
                    }],
                    drift: LineDrift::default(),
                },
            })
        );
    }

    /// The column is found after folding, so the curly quotes that the
    /// tolerant layer would have forgiven do not claim to be the difference.
    #[test]
    fn first_difference_skips_what_folding_forgives() {
        assert_eq!(
            first_difference("say \"hi\" to bob", "say “hi” to rob"),
            FirstDifference {
                column: 13,
                yours: Some('b'),
                file: Some('r'),
            }
        );
        assert_eq!(
            first_difference("abc", "abcd"),
            FirstDifference {
                column: 4,
                yours: None,
                file: Some('d'),
            }
        );
    }

    #[test]
    fn banded_distance_stops_at_its_bound() {
        let chars = |text: &str| text.chars().collect::<Vec<_>>();
        assert_eq!(
            banded_edit_distance(&chars("kitten"), &chars("sitting"), 3),
            Some(3)
        );
        assert_eq!(
            banded_edit_distance(&chars("kitten"), &chars("sitting"), 2),
            None
        );
        assert_eq!(banded_edit_distance(&chars(""), &chars("ab"), 2), Some(2));
    }
}
