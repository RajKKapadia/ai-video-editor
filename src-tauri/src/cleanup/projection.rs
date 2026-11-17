//! Projects source-coordinate transcript words through the current timeline.
//!
//! Words are stored once, in source seconds, with stable IDs. A timeline edit
//! only changes which source ranges are listed, so the transcript "after a cut"
//! is this projection: cut words become hidden (not invalid) and reappear on
//! Undo without another transcription. Only source ranges that were never
//! analysed need transcribing, and `unanalysed` reports exactly those.

use super::Range;
use std::collections::{HashMap, HashSet};

const EPSILON: f64 = 0.000_001;

/// One timeline segment: a source range of a media item, in timeline order.
#[derive(Clone, Debug)]
pub(super) struct Segment<'a> {
    pub media: &'a str,
    pub source: Range,
}

#[derive(Clone, Debug)]
pub(super) struct StoredWord<'a> {
    pub id: &'a str,
    pub media: &'a str,
    pub source: Range,
    pub text: &'a str,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct VisibleWord<'a> {
    pub id: &'a str,
    pub segment: usize,
    pub timeline: Range,
    /// The word straddles a segment edge; only part of it is audible.
    pub clipped: bool,
    pub text: &'a str,
}

#[derive(Debug, Default)]
pub(super) struct Projection<'a> {
    /// Visible words in timeline order. A source word repeated on the timeline
    /// appears once per occurrence.
    pub visible: Vec<VisibleWord<'a>>,
    /// Stored words not audible anywhere on the current timeline.
    pub hidden: Vec<&'a str>,
}

fn timeline_starts(timeline: &[Segment<'_>]) -> Vec<f64> {
    let mut cursor = 0.0;
    timeline
        .iter()
        .map(|segment| {
            let start = cursor;
            cursor += segment.source.duration().max(0.0);
            start
        })
        .collect()
}

pub(super) fn project<'a>(timeline: &[Segment<'_>], words: &[StoredWord<'a>]) -> Projection<'a> {
    let starts = timeline_starts(timeline);
    let mut by_media: HashMap<&str, Vec<&StoredWord<'a>>> = HashMap::new();
    for word in words {
        by_media.entry(word.media).or_default().push(word);
    }
    for list in by_media.values_mut() {
        list.sort_by(|a, b| a.source.start.total_cmp(&b.source.start));
    }
    let mut projection = Projection::default();
    let mut seen = HashSet::new();
    for (index, segment) in timeline.iter().enumerate() {
        let Some(list) = by_media.get(segment.media) else { continue };
        let from = list.partition_point(|word| word.source.end <= segment.source.start + EPSILON);
        for word in &list[from..] {
            if word.source.start >= segment.source.end - EPSILON {
                break;
            }
            let start = word.source.start.max(segment.source.start);
            let end = word.source.end.min(segment.source.end);
            // A word counts as visible when most of it is kept; a sliver left
            // beside a cut is not something the viewer hears as that word.
            let length = word.source.duration();
            if length > EPSILON && (end - start) * 2.0 < length {
                continue;
            }
            let offset = starts[index] - segment.source.start;
            projection.visible.push(VisibleWord {
                id: word.id,
                segment: index,
                timeline: Range::new(start + offset, end + offset),
                clipped: start > word.source.start + EPSILON || end < word.source.end - EPSILON,
                text: word.text,
            });
            seen.insert(word.id);
        }
    }
    projection.hidden = words.iter().map(|word| word.id).filter(|id| !seen.contains(id)).collect();
    projection
}

/// Parts of timeline segments whose source range has no stored analysis, in
/// timeline order. Only these need transcribing.
pub(super) fn unanalysed(
    timeline: &[Segment<'_>],
    analysed: &HashMap<&str, Vec<Range>>,
) -> Vec<(usize, Range)> {
    let mut gaps = Vec::new();
    for (index, segment) in timeline.iter().enumerate() {
        let mut covered: Vec<Range> = analysed
            .get(segment.media)
            .map(|ranges| ranges.iter().copied().filter(|r| r.overlaps(segment.source)).collect())
            .unwrap_or_default();
        covered.sort_by(|a, b| a.start.total_cmp(&b.start));
        let mut cursor = segment.source.start;
        for range in covered {
            if range.start > cursor + EPSILON {
                gaps.push((index, Range::new(cursor, range.start.min(segment.source.end))));
            }
            cursor = cursor.max(range.end);
            if cursor >= segment.source.end - EPSILON {
                break;
            }
        }
        if cursor < segment.source.end - EPSILON {
            gaps.push((index, Range::new(cursor, segment.source.end)));
        }
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words() -> Vec<StoredWord<'static>> {
        // "hello there friend" at 1-2, 2-3, 3-4 s, then "take two" at 10-11, 11-12 s.
        [("w1", 1.0, 2.0, "hello"), ("w2", 2.0, 3.0, "there"), ("w3", 3.0, 4.0, "friend"),
         ("w4", 10.0, 11.0, "take"), ("w5", 11.0, 12.0, "two")]
            .into_iter()
            .map(|(id, start, end, text)| StoredWord { id, media: "a", source: Range::new(start, end), text })
            .collect()
    }
    fn seg(start: f64, end: f64) -> Segment<'static> {
        Segment { media: "a", source: Range::new(start, end) }
    }
    fn ids<'a>(projection: &Projection<'a>) -> Vec<&'a str> {
        projection.visible.iter().map(|word| word.id).collect()
    }

    #[test]
    fn full_timeline_shows_every_word_at_source_time() {
        let words = words();
        let projection = project(&[seg(0.0, 12.0)], &words);
        assert_eq!(ids(&projection), ["w1", "w2", "w3", "w4", "w5"]);
        assert_eq!(projection.visible[3].timeline, Range::new(10.0, 11.0));
        assert!(projection.hidden.is_empty());
    }

    #[test]
    fn a_cut_hides_words_and_shifts_later_ones_without_changing_ids() {
        let words = words();
        // Remove "there" (2-3 s) and the dead air 4-10 s.
        let projection = project(&[seg(0.0, 2.0), seg(3.0, 4.0), seg(10.0, 12.0)], &words);
        assert_eq!(ids(&projection), ["w1", "w3", "w4", "w5"]);
        assert_eq!(projection.hidden, ["w2"]);
        assert_eq!(projection.visible[1].timeline, Range::new(2.0, 3.0));
        assert_eq!(projection.visible[2].timeline, Range::new(3.0, 4.0));
    }

    #[test]
    fn undo_restores_the_identical_projection() {
        let words = words();
        let before = project(&[seg(0.0, 12.0)], &words);
        let _cut = project(&[seg(0.0, 2.0), seg(3.0, 12.0)], &words);
        let after = project(&[seg(0.0, 12.0)], &words);
        assert_eq!(before.visible, after.visible);
        assert_eq!(before.hidden, after.hidden);
    }

    #[test]
    fn reordered_and_repeated_segments_follow_timeline_order() {
        let words = words();
        let projection = project(&[seg(10.0, 12.0), seg(1.0, 2.0), seg(10.0, 11.0)], &words);
        assert_eq!(ids(&projection), ["w4", "w5", "w1", "w4"]);
        assert_eq!(projection.visible[2].timeline, Range::new(2.0, 3.0));
        assert_eq!(projection.visible[3].timeline, Range::new(3.0, 4.0));
    }

    #[test]
    fn mostly_cut_words_are_hidden_and_partly_cut_words_are_clipped() {
        let words = words();
        let projection = project(&[seg(0.0, 2.2), seg(3.6, 4.0)], &words);
        // "there" keeps 0.2 s of 1 s: hidden. "friend" keeps 0.4 s: hidden.
        assert_eq!(ids(&projection), ["w1"]);
        let projection = project(&[seg(0.0, 2.7)], &words);
        assert_eq!(ids(&projection), ["w1", "w2"]);
        assert!(projection.visible[1].clipped);
        assert_eq!(projection.visible[1].timeline, Range::new(2.0, 2.7));
    }

    #[test]
    fn only_newly_exposed_source_needs_transcribing() {
        let analysed = HashMap::from([("a", vec![Range::new(0.0, 12.0)])]);
        assert!(unanalysed(&[seg(0.0, 5.0), seg(8.0, 12.0)], &analysed).is_empty());
        // Extending the last segment past what was analysed exposes 12-15 s only.
        let gaps = unanalysed(&[seg(0.0, 5.0), seg(8.0, 15.0)], &analysed);
        assert_eq!(gaps, [(1, Range::new(12.0, 15.0))]);
        let split = HashMap::from([("a", vec![Range::new(0.0, 3.0), Range::new(6.0, 9.0)])]);
        assert_eq!(
            unanalysed(&[seg(0.0, 9.0)], &split),
            [(0, Range::new(3.0, 6.0))]
        );
        assert_eq!(unanalysed(&[Segment { media: "b", source: Range::new(0.0, 2.0) }], &split),
            [(0, Range::new(0.0, 2.0))]);
    }
}
