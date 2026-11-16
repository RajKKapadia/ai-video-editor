//! Group attempts at the same line before the model chooses one.
//!
//! The planner is more reliable picking the best of N listed attempts than
//! discovering repetitions in a flat transcript. Groups are found locally from
//! word overlap, carry simple delivery measurements, and are later used to
//! check that the draft leaves a single take of every line.
use std::collections::HashMap;

use serde_json::{Value, json};

use super::{Proposal, Range, TimelineUtterance, UtteranceRange, resolve};

/// Longest attempt considered, in consecutive utterances.
const MAX_SPAN: usize = 3;
const MIN_TOKENS: usize = 4;
const MIN_SIMILARITY: f64 = 0.7;
const MAX_GROUPS: usize = 64;
const MAX_MEMBERS: usize = 6;
/// Bigrams shared by more spans than this are too common to suggest a retake.
const MAX_POSTING: usize = 64;
/// Fraction of a member's speech that must be removed for it to count as cut.
const REMOVED_FRACTION: f64 = 0.6;
const FILLERS: &[&str] = &[
    "um", "umm", "uh", "uhh", "uhm", "er", "erm", "ah", "hmm", "mm",
];

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
}

struct Span {
    /// Zero-based, inclusive utterance indices.
    first: usize,
    last: usize,
    tokens: Vec<String>,
}

fn spans(utterances: &[TimelineUtterance<'_>]) -> Vec<Span> {
    let texts: Vec<Vec<String>> = utterances
        .iter()
        .map(|line| {
            words(line.text)
                .filter(|word| !FILLERS.contains(&word.as_str()))
                .collect()
        })
        .collect();
    let mut result = Vec::new();
    for first in 0..utterances.len() {
        let mut tokens = Vec::new();
        for (last, text) in texts.iter().enumerate().skip(first).take(MAX_SPAN) {
            if resolve(range(first, last), utterances).is_none() {
                break;
            }
            tokens.extend(text.iter().cloned());
            if tokens.len() >= MIN_TOKENS {
                result.push(Span {
                    first,
                    last,
                    tokens: tokens.clone(),
                });
            }
        }
    }
    result
}

fn range(first: usize, last: usize) -> UtteranceRange {
    UtteranceRange {
        first: first as u32 + 1,
        last: last as u32 + 1,
    }
}

fn longest_common_subsequence(left: &[String], right: &[String]) -> usize {
    let mut previous = vec![0; right.len() + 1];
    let mut current = vec![0; right.len() + 1];
    for word in left {
        for (index, other) in right.iter().enumerate() {
            current[index + 1] = if word == other {
                previous[index] + 1
            } else {
                previous[index + 1].max(current[index])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

/// Similarity score for two attempts, or `None` when they are not the same line.
fn score(left: &[String], right: &[String]) -> Option<f64> {
    let common = longest_common_subsequence(left, right);
    let shorter = left.len().min(right.len());
    let longer = left.len().max(right.len());
    let prefix = left
        .iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count();
    // A short abandoned start may be a small part of the complete take, so a
    // shared opening also qualifies when the overall overlap is low.
    let similar = common >= MIN_TOKENS
        && common as f64 / shorter as f64 >= MIN_SIMILARITY
        && (common * 4 >= longer || prefix >= MIN_TOKENS);
    // Favour longer matches, but penalise spans padded with unrelated speech.
    similar.then(|| common as f64 * 2.0 * common as f64 / (left.len() + right.len()) as f64)
}

/// Groups of attempts at the same line, each member a range of consecutive
/// utterances. Members never overlap, within or across groups.
pub(super) fn take_groups(utterances: &[TimelineUtterance<'_>]) -> Vec<Vec<UtteranceRange>> {
    let spans = spans(utterances);
    let mut index = HashMap::<(&str, &str), Vec<usize>>::new();
    for (id, span) in spans.iter().enumerate() {
        let mut seen = std::collections::HashSet::new();
        for pair in span.tokens.windows(2) {
            let key = (pair[0].as_str(), pair[1].as_str());
            if seen.insert(key) {
                index.entry(key).or_default().push(id);
            }
        }
    }
    let mut shared = HashMap::<(usize, usize), usize>::new();
    for ids in index.values().filter(|ids| ids.len() <= MAX_POSTING) {
        for (offset, &left) in ids.iter().enumerate() {
            for &right in &ids[offset + 1..] {
                if spans[left].last < spans[right].first {
                    *shared.entry((left, right)).or_default() += 1;
                }
            }
        }
    }
    let mut pairs: Vec<(f64, usize, usize)> = shared
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .filter_map(|((left, right), _)| {
            score(&spans[left].tokens, &spans[right].tokens).map(|score| (score, left, right))
        })
        .collect();
    pairs.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(spans[a.1].first.cmp(&spans[b.1].first))
            .then(spans[a.2].first.cmp(&spans[b.2].first))
    });

    let mut owner = vec![None::<usize>; utterances.len()];
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let free = |owner: &[Option<usize>], span: &Span| {
        owner[span.first..=span.last].iter().all(Option::is_none)
    };
    for (_, left, right) in pairs {
        let (a, b) = (&spans[left], &spans[right]);
        let group = match (free(&owner, a), free(&owner, b)) {
            (true, true) if groups.len() < MAX_GROUPS => {
                groups.push(vec![left, right]);
                groups.len() - 1
            }
            // Extend a group only from one of its exact members.
            (false, true) => match groups.iter().position(|members| members.contains(&left)) {
                Some(group) if groups[group].len() < MAX_MEMBERS => {
                    groups[group].push(right);
                    group
                }
                _ => continue,
            },
            (true, false) => match groups.iter().position(|members| members.contains(&right)) {
                Some(group) if groups[group].len() < MAX_MEMBERS => {
                    groups[group].push(left);
                    group
                }
                _ => continue,
            },
            _ => continue,
        };
        for id in [left, right] {
            for slot in &mut owner[spans[id].first..=spans[id].last] {
                *slot = Some(group);
            }
        }
    }
    let mut groups: Vec<Vec<UtteranceRange>> = groups
        .into_iter()
        .map(|mut members| {
            members.sort_by_key(|&id| spans[id].first);
            members
                .into_iter()
                .map(|id| range(spans[id].first, spans[id].last))
                .collect()
        })
        .collect();
    groups.sort_by_key(|members| members[0].first);
    groups
}

/// `[first ID, last ID, seconds, words per second, filler words, restarts]`.
pub(super) fn member_row(member: UtteranceRange, utterances: &[TimelineUtterance<'_>]) -> Value {
    let Some(lines) = resolve(member, utterances) else {
        return json!([member.first, member.last]);
    };
    let tokens: Vec<String> = lines.iter().flat_map(|line| words(line.text)).collect();
    let seconds = lines.last().unwrap().speech.end - lines[0].speech.start;
    let fillers = tokens
        .iter()
        .filter(|word| FILLERS.contains(&word.as_str()))
        .count();
    // Immediate word or two-word repetitions ("the the", "I want I want").
    let restarts = tokens.windows(2).filter(|pair| pair[0] == pair[1]).count()
        + tokens
            .windows(4)
            .filter(|four| four[0] == four[2] && four[1] == four[3] && four[0] != four[1])
            .count();
    let round = |value: f64, scale: f64| (value * scale).round() / scale;
    json!([
        member.first,
        member.last,
        round(seconds, 100.0),
        round(tokens.len() as f64 / seconds.max(0.1), 10.0),
        fillers,
        restarts,
    ])
}

fn overlap(left: Range, right: Range) -> f64 {
    (left.end.min(right.end) - left.start.max(right.start)).max(0.0)
}

fn removed(
    member: UtteranceRange,
    proposals: &[Proposal],
    utterances: &[TimelineUtterance<'_>],
) -> bool {
    let Some(lines) = resolve(member, utterances) else {
        return false;
    };
    let total: f64 = lines.iter().map(|line| line.speech.duration()).sum();
    let cut: f64 = lines
        .iter()
        .map(|line| {
            proposals
                .iter()
                .filter(|proposal| proposal.kind != "pause")
                .map(|proposal| overlap(proposal.remove, line.speech))
                .sum::<f64>()
                .min(line.speech.duration())
        })
        .sum();
    total > 0.0 && cut >= total * REMOVED_FRACTION
}

/// Groups in which the proposals still leave more than one attempt.
pub(super) fn unresolved(
    groups: &[Vec<UtteranceRange>],
    proposals: &[Proposal],
    utterances: &[TimelineUtterance<'_>],
) -> Vec<Vec<UtteranceRange>> {
    groups
        .iter()
        .filter_map(|members| {
            let kept: Vec<_> = members
                .iter()
                .copied()
                .filter(|&member| !removed(member, proposals, utterances))
                .collect();
            (kept.len() > 1).then_some(kept)
        })
        .collect()
}

pub(super) fn describe(members: &[UtteranceRange]) -> String {
    let names: Vec<String> = members
        .iter()
        .map(|member| {
            if member.first == member.last {
                member.first.to_string()
            } else {
                format!("{}–{}", member.first, member.last)
            }
        })
        .collect();
    match names.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(texts: &[&'static str]) -> Vec<TimelineUtterance<'static>> {
        texts
            .iter()
            .enumerate()
            .map(|(index, text)| TimelineUtterance {
                run: 0,
                ordinal: index,
                last: index + 1 == texts.len(),
                speech: Range::new(index as f64 * 3.0, index as f64 * 3.0 + 2.0),
                cut_start: Some(index as f64 * 3.0 - 0.1),
                cut_end: Some(index as f64 * 3.0 + 2.1),
                text,
                words: Vec::new(),
            })
            .collect()
    }

    fn ids(groups: &[Vec<UtteranceRange>]) -> Vec<Vec<(u32, u32)>> {
        groups
            .iter()
            .map(|members| members.iter().map(|m| (m.first, m.last)).collect())
            .collect()
    }

    #[test]
    fn groups_false_starts_retakes_and_distant_repeats() {
        let lines = lines(&[
            "So today we are going to",
            "So today we are going to build a small weather app",
            "First open the terminal and create a project",
            "Um first open the the terminal and create a new project",
            "Then install the dependencies",
            "Okay that is the setup done",
            "So today we are going to build a weather app with React",
        ]);
        let groups = take_groups(&lines);
        assert_eq!(
            ids(&groups),
            vec![vec![(1, 1), (2, 2), (7, 7)], vec![(3, 3), (4, 4)]]
        );
        // The second attempt at the terminal step has a filler and a restart.
        let row = member_row(groups[1][1], &lines);
        assert_eq!((row[4].as_u64(), row[5].as_u64()), (Some(1), Some(1)));
    }

    #[test]
    fn distinct_steps_are_not_grouped() {
        let lines = lines(&[
            "Open the settings panel and pick a theme",
            "Close the settings panel and save your work",
            "Now export the video as MP4",
        ]);
        assert!(take_groups(&lines).is_empty());
    }

    #[test]
    fn a_multi_utterance_take_is_one_member() {
        let lines = lines(&[
            "Click the export button at the top",
            "then choose the MP4 format",
            "Something unrelated about lighting here",
            "Click the export button at the top then choose the MP4 format",
        ]);
        assert_eq!(ids(&take_groups(&lines)), vec![vec![(1, 2), (4, 4)]]);
    }

    #[test]
    fn reports_groups_that_still_keep_several_takes() {
        let lines = lines(&[
            "So today we are going to",
            "So today we are going to build a small weather app",
            "Then install the dependencies",
        ]);
        let groups = take_groups(&lines);
        assert_eq!(unresolved(&groups, &[], &lines).len(), 1);
        let cut = Proposal {
            kind: "retake",
            remove: Range::new(-0.1, 2.1),
            keep: Some(lines[1].speech),
            removed_text: None,
            kept_text: None,
            reason: None,
        };
        assert!(unresolved(&groups, &[cut], &lines).is_empty());
        assert_eq!(describe(&groups[0]), "1 and 2");
    }
}
