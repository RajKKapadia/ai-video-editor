//! Measured alternatives for joining related fragments and removing repeated phrases.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Proposal, Range, TimelineUtterance, UtteranceRange, bounded, resolve};

const MAX_OPTIONS: usize = 96;
const MAX_INLINE_OVERLAP: usize = 24;
const MAX_RESTART_DISTANCE: usize = 3;

pub(super) fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

pub(super) fn aligned(line: &TimelineUtterance<'_>) -> bool {
    !line.words.is_empty()
        && line
            .words
            .iter()
            .flat_map(|word| tokens(&word.text))
            .eq(tokens(line.text))
        && line.words.iter().all(|word| {
            word.speech.start.is_finite()
                && word.speech.end.is_finite()
                && word.speech.duration() > 0.0
                && word.speech.start >= line.speech.start - 0.2
                && word.speech.end <= line.speech.end + 0.2
        })
        && line
            .words
            .windows(2)
            .all(|pair| pair[0].speech.end <= pair[1].speech.start + 0.000_001)
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Alternative {
    pub id: usize,
    group: usize,
    retain_overlap: &'static str,
    before: usize,
    after: usize,
    pub remove: Range,
    keep: Range,
    removed_text: String,
    result: String,
}

impl Alternative {
    pub fn proposal(&self, reason: &str) -> Option<Proposal> {
        let reason = bounded(reason, 350);
        (!reason.is_empty()).then(|| Proposal {
            kind: if self.after == self.before {
                "retake"
            } else {
                "splice"
            },
            remove: self.remove,
            keep: Some(self.keep),
            removed_text: Some(self.removed_text.clone()),
            kept_text: Some(self.result.clone()),
            reason: Some(reason),
        })
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Choice {
    pub id: usize,
    pub reason: String,
}

pub(super) fn choice_schema() -> Value {
    json!({"type":"array", "items": {"type":"object", "properties": {
        "id":{"type":"integer"}, "reason":{"type":"string"}
    }, "required":["id","reason"], "additionalProperties":false}})
}

fn overlap(left: &[Vec<String>], right: &[Vec<String>]) -> usize {
    (1..=left.len().min(right.len()))
        .rev()
        .find(|&count| {
            let suffix = &left[left.len() - count..];
            suffix.iter().all(|word| !word.is_empty()) && suffix == &right[..count]
        })
        .unwrap_or(0)
}

fn boundary(line: &TimelineUtterance<'_>, index: usize) -> Option<f64> {
    if index == 0 {
        line.cut_start
    } else if index == line.words.len() {
        line.cut_end
    } else {
        Some((line.words[index - 1].speech.end + line.words[index].speech.start) * 0.5)
    }
}

fn text(words: &[super::super::audio::Word]) -> String {
    words
        .iter()
        .map(|word| word.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn span(words: &[super::super::audio::Word]) -> Range {
    Range::new(
        words.first().unwrap().speech.start,
        words.last().unwrap().speech.end,
    )
}

pub(super) fn alternatives(lines: &[TimelineUtterance<'_>]) -> Vec<Alternative> {
    let aligned_words: Vec<_> = lines
        .iter()
        .map(|line| {
            aligned(line).then(|| {
                line.words
                    .iter()
                    .map(|word| tokens(&word.text))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut options = Vec::new();
    let mut group = 0;
    for (index, left) in lines.iter().enumerate() {
        let Some(a) = &aligned_words[index] else {
            continue;
        };
        for restart in 1..a.len() {
            let count = overlap(
                &a[restart.saturating_sub(MAX_INLINE_OVERLAP)..restart],
                &a[restart..],
            );
            if count == 0 {
                continue;
            }
            let start = restart - count;
            let end = restart + count;
            let result = format!(
                "{} {}",
                text(&left.words[..start]),
                text(&left.words[restart..])
            );
            group += 1;
            for (side, from, to, keep) in [
                ("right", start, restart, span(&left.words[restart..end])),
                ("left", restart, end, span(&left.words[start..restart])),
            ] {
                let Some((from_time, to_time)) = boundary(left, from).zip(boundary(left, to))
                else {
                    continue;
                };
                let remove = Range::new(from_time, to_time);
                if remove.duration() > 0.0 && !remove.overlaps(keep) {
                    options.push(Alternative {
                        id: options.len() + 1,
                        group,
                        retain_overlap: side,
                        before: index + 1,
                        after: index + 1,
                        remove,
                        keep,
                        removed_text: bounded(&text(&left.words[from..to]), 350),
                        result: bounded(&result, 700),
                    });
                    if options.len() == MAX_OPTIONS {
                        return options;
                    }
                }
            }
        }
        for next in index + 1..(index + 1 + MAX_RESTART_DISTANCE).min(lines.len()) {
            let Some(window) = resolve(
                UtteranceRange {
                    first: index as u32 + 1,
                    last: next as u32 + 1,
                },
                lines,
            ) else {
                break;
            };
            if window
                .windows(2)
                .any(|pair| pair[0].speech.end > pair[1].speech.start)
            {
                break;
            }
            let right = &lines[next];
            let Some(b) = &aligned_words[next] else {
                continue;
            };
            let count = overlap(a, b);
            if count == 0 {
                continue;
            }
            let start = a.len() - count;
            let between = super::join_text(&window[1..window.len() - 1]);
            // Expose the entire proposed deletion. The planner must judge whether
            // intervening material is an abandoned attempt or contains unique content.
            let result = format!("{} {}", text(&left.words[..start]), text(&right.words));
            group += 1;
            for (side, from, to, keep, removed_text) in [
                (
                    "right",
                    boundary(left, start),
                    right.cut_start,
                    span(&right.words[..count]),
                    format!("{} {between}", text(&left.words[start..])),
                ),
                (
                    "left",
                    left.cut_end,
                    boundary(right, count),
                    span(&left.words[start..]),
                    format!("{between} {}", text(&right.words[..count])),
                ),
            ] {
                let Some((from, to)) = from.zip(to) else {
                    continue;
                };
                let remove = Range::new(from, to);
                if remove.duration() > 0.0 && !remove.overlaps(keep) {
                    options.push(Alternative {
                        id: options.len() + 1,
                        group,
                        retain_overlap: side,
                        before: index + 1,
                        after: next + 1,
                        remove,
                        keep,
                        removed_text: bounded(&removed_text, 350),
                        result: bounded(&result, 700),
                    });
                    if options.len() == MAX_OPTIONS {
                        return options;
                    }
                }
            }
        }
    }
    options
}

pub(super) fn select(options: &[Alternative], choices: Vec<Choice>) -> Option<Vec<Proposal>> {
    let mut groups = std::collections::HashSet::new();
    choices
        .into_iter()
        .map(|choice| {
            let option = options.get(choice.id.checked_sub(1)?)?;
            if !groups.insert(option.group) {
                return None;
            }
            option.proposal(&choice.reason)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleanup::audio::Word;

    fn line(text: &'static str, start: f64, ordinal: usize) -> TimelineUtterance<'static> {
        let words: Vec<_> = text
            .split_whitespace()
            .enumerate()
            .map(|(i, word)| Word {
                text: word.into(),
                speech: Range::new(start + i as f64 * 0.4, start + i as f64 * 0.4 + 0.3),
                anchor: None,
            })
            .collect();
        let end = words.last().unwrap().speech.end;
        TimelineUtterance {
            run: 0,
            ordinal,
            last: ordinal == 1,
            speech: Range::new(start, end),
            cut_start: Some(start - 0.15),
            cut_end: Some(end + 0.15),
            text,
            words,
        }
    }

    #[test]
    fn offers_both_connections_for_a_partial_continuation() {
        let lines = [
            line("Today we are going to build an AI", 0.3, 0),
            line("AI agent from scratch.", 7.0, 1),
        ];
        let options = alternatives(&lines);
        assert_eq!(options.len(), 2);
        for option in &options {
            assert_eq!(
                option.result,
                "Today we are going to build an AI agent from scratch."
            );
            assert!(!option.remove.overlaps(option.keep));
        }
        assert!(options[0].remove.start < lines[0].words.last().unwrap().speech.start);
        assert_eq!(options[0].remove.end, 6.85);
        assert!(options[1].remove.end > lines[1].words[0].speech.end);
        assert!(
            select(
                &options,
                vec![Choice {
                    id: 1,
                    reason: "Continue the sentence".into()
                }]
            )
            .is_some()
        );
        assert!(
            select(
                &options,
                vec![
                    Choice {
                        id: 1,
                        reason: "x".into()
                    },
                    Choice {
                        id: 2,
                        reason: "y".into()
                    }
                ]
            )
            .is_none()
        );
    }

    #[test]
    fn offers_a_repeated_phrase_within_one_utterance() {
        let lines = [line("Open settings. Open settings and choose MP4.", 0.3, 0)];
        let options = alternatives(&lines);
        assert_eq!(options.len(), 2);
        assert!(
            options
                .iter()
                .all(|option| option.result == "Open settings and choose MP4.")
        );
        let retake = options[0]
            .proposal("Keep the complete instruction")
            .unwrap();
        assert_eq!(retake.kind, "retake");
        assert_eq!(retake.removed_text.as_deref(), Some("Open settings."));
    }

    #[test]
    fn offers_a_continuation_across_another_attempt_with_its_removal_text() {
        let mut lines = [
            line("Today we build an AI", 0.3, 0),
            line("Let me try that again.", 3.0, 1),
            line("AI agent from scratch.", 7.0, 2),
        ];
        let options = alternatives(&lines);
        assert_eq!(options.len(), 2);
        assert!(options.iter().all(|option| {
            option.result == "Today we build an AI agent from scratch."
                && option.removed_text.contains(lines[1].text)
        }));
        lines[1].ordinal = 2;
        assert!(alternatives(&lines).is_empty());
    }

    #[test]
    fn preserves_unknown_speech_and_rejects_missing_alignment() {
        let mut lines = [line("build an AI", 0.3, 0), line("AI agent", 4.0, 1)];
        lines[1].ordinal = 2;
        assert!(alternatives(&lines).is_empty());
        lines[1].ordinal = 1;
        lines[0].words.pop();
        assert!(alternatives(&lines).is_empty());
    }

    #[test]
    fn repeated_wording_only_creates_hypotheses() {
        let lines = [line("Very", 0.3, 0), line("very important", 2.0, 1)];
        let options = alternatives(&lines);
        assert_eq!(options.len(), 2);
        assert!(select(&options, vec![]).unwrap().is_empty());
    }
}
