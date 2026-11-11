//! Turn whole-utterance or aligned-word choices into measured edit boundaries.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Range, TimelineUtterance, UtteranceRange, join_text, resolve, safe_removal_range};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WordRange {
    first: usize,
    last: usize,
}

pub(super) fn word_range_schema() -> Value {
    json!({"anyOf": [{"type": "object", "properties": {
        "first": {"type": "integer"}, "last": {"type": "integer"}
    }, "required": ["first", "last"], "additionalProperties": false}, {"type": "null"}]})
}

pub(super) struct Selection {
    pub speech: Range,
    pub removal: Option<Range>,
    pub text: String,
}

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
}

pub(super) fn select(
    ids: UtteranceRange,
    selection: Option<WordRange>,
    all: &[TimelineUtterance<'_>],
) -> Option<Selection> {
    let lines = resolve(ids, all)?;
    let first = lines.first()?;
    let last = lines.last()?;
    let Some(selection) = selection else {
        return Some(Selection {
            speech: Range::new(first.speech.start, last.speech.end),
            removal: safe_removal_range(ids, lines, all),
            text: join_text(lines),
        });
    };
    if selection.first == 1 && !last.words.is_empty() && selection.last == last.words.len() {
        return select(ids, None, all);
    }
    // Never apply word indices to incomplete, reordered or missing alignment.
    if lines.iter().any(|line| {
        line.words.is_empty()
            || !line
                .words
                .iter()
                .flat_map(|word| words(&word.text))
                .eq(words(line.text))
            || line.words.iter().any(|word| {
                !word.speech.start.is_finite()
                    || !word.speech.end.is_finite()
                    || word.speech.duration() < 0.0
                    || word.speech.start < line.speech.start - 0.2
                    || word.speech.end > line.speech.end + 0.2
            })
            || line
                .words
                .windows(2)
                .any(|pair| pair[0].speech.end > pair[1].speech.start + 0.000_001)
    }) {
        return None;
    }
    let begin = selection.first.checked_sub(1)?;
    let end = selection.last;
    let start_word = first.words.get(begin)?;
    let end_word = last.words.get(end.checked_sub(1)?)?;
    if ids.first == ids.last && begin >= end {
        return None;
    }
    let speech = Range::new(start_word.speech.start, end_word.speech.end);
    if speech.duration() <= 0.0 {
        return None;
    }
    let start = if begin == 0 {
        first.cut_start
    } else if first.words[begin - 1].speech.duration() > 0.0 && start_word.speech.duration() > 0.0 {
        Some((first.words[begin - 1].speech.end + start_word.speech.start) * 0.5)
    } else {
        None
    };
    let finish = if end == last.words.len() {
        last.cut_end
    } else if end_word.speech.duration() > 0.0 && last.words.get(end)?.speech.duration() > 0.0 {
        Some((end_word.speech.end + last.words.get(end)?.speech.start) * 0.5)
    } else {
        None
    };
    let removal = start
        .zip(finish)
        .map(|(start, end)| Range::new(start, end))
        .filter(|range| {
            range.duration() > 0.0
                && !all.iter().enumerate().any(|(index, line)| {
                    let id = index as u32 + 1;
                    (id < ids.first || id > ids.last) && range.overlaps(line.speech)
                })
        });
    let mut selected = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let lo = if index == 0 { begin } else { 0 };
        let hi = if index + 1 == lines.len() {
            end
        } else {
            line.words.len()
        };
        selected.extend(
            line.words
                .get(lo..hi)?
                .iter()
                .map(|word| word.text.as_str()),
        );
    }
    Some(Selection {
        speech,
        removal,
        text: selected.join(" "),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleanup::audio::Word;

    fn line() -> TimelineUtterance<'static> {
        TimelineUtterance {
            run: 0,
            ordinal: 0,
            last: true,
            speech: Range::new(10.2, 13.9),
            cut_start: Some(10.0),
            cut_end: Some(14.0),
            text: "Open settings. Open settings and choose MP4.",
            words: [
                "Open",
                "settings.",
                "Open",
                "settings",
                "and",
                "choose",
                "MP4.",
            ]
            .into_iter()
            .enumerate()
            .map(|(index, text)| Word {
                speech: Range::new(10.2 + index as f64 * 0.5, 10.6 + index as f64 * 0.5),
                text: text.into(),
                anchor: None,
            })
            .collect(),
        }
    }

    #[test]
    fn zero_duration_words_remain_readable_but_cannot_anchor_internal_cuts() {
        let mut sample = line();
        sample.words[2].speech.end = sample.words[2].speech.start;
        let selection = select(
            UtteranceRange { first: 1, last: 1 },
            Some(WordRange { first: 3, last: 4 }),
            &[sample],
        )
        .unwrap();
        assert!(selection.removal.is_none());
    }

    #[test]
    fn removes_a_repeated_phrase_and_keeps_the_complete_take_inside_one_utterance() {
        let lines = [line()];
        let ids = UtteranceRange { first: 1, last: 1 };
        let removed = select(ids, Some(WordRange { first: 1, last: 2 }), &lines).unwrap();
        let kept = select(ids, Some(WordRange { first: 3, last: 7 }), &lines).unwrap();
        assert_eq!(removed.text, "Open settings.");
        assert_eq!(kept.text, "Open settings and choose MP4.");
        assert!(!removed.removal.unwrap().overlaps(kept.speech));
        let choice: super::super::CutChoice = serde_json::from_value(json!({
            "remove": {"first": 1, "last": 1}, "keep": {"first": 1, "last": 1},
            "removeWords": {"first": 1, "last": 2}, "keepWords": {"first": 3, "last": 7},
            "reason": "Keep the complete instruction",
        }))
        .unwrap();
        let proposals = super::super::cut_proposals(&lines, vec![choice]);
        assert_eq!(proposals.len(), 1);
        assert_eq!(
            proposals[0].kept_text.as_deref(),
            Some("Open settings and choose MP4.")
        );
    }

    #[test]
    fn refuses_incomplete_or_overlapping_alignment_and_out_of_range_indices() {
        let ids = UtteranceRange { first: 1, last: 1 };
        for words in [
            WordRange { first: 0, last: 2 },
            WordRange { first: 3, last: 2 },
            WordRange { first: 1, last: 8 },
        ] {
            assert!(select(ids, Some(words), &[line()]).is_none());
        }
        let mut bad = line();
        bad.words[2].speech.start = bad.words[1].speech.end - 0.2;
        assert!(select(ids, Some(WordRange { first: 1, last: 2 }), &[bad]).is_none());
        let mut incomplete = line();
        incomplete.words.pop();
        assert!(select(ids, Some(WordRange { first: 1, last: 2 }), &[incomplete]).is_none());
    }
}
