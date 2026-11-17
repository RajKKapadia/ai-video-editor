//! Measured restart detection for single-take cleanup.
//!
//! A restart is an earlier attempt that the speaker abandons and then says
//! again: "So today we're, uh, so today we're going to build…". The detector
//! works on aligned words across the whole timeline, so it finds restarts
//! inside one utterance, across utterances and across existing timeline cuts,
//! and it collapses chains of several attempts into one removal that keeps the
//! final attempt. Results are offered to the planner as exact word ranges and
//! clear, verbatim restarts the planner missed are added to the draft for
//! review. Rephrased repeats still rely on the planner's judgment.
use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::{
    CutChoice, Proposal, TimelineUtterance, UtteranceRange, bridge, conflicts, cut_proposals,
    draft::Draft, removes_all_speech, selections::WordRange,
};

/// Longest abandoned attempt, in spoken words, that can count as a restart.
const MAX_ATTEMPT_WORDS: usize = 120;
/// Restarts happen soon after the attempt; longer distances are left to the planner.
const MAX_ATTEMPT_SECONDS: f64 = 90.0;
const LOOKAHEAD_SLACK: usize = 12;
/// Minimum share of the attempt's words that must reappear, in order, in the restart.
const MIN_COVERAGE: f64 = 0.6;
/// Clear restarts are added automatically when the planner leaves them in place.
const CLEAR_COVERAGE: f64 = 0.85;
const MAX_EVIDENCE: usize = 64;

#[rustfmt::skip]
const FILLERS: &[&str] = &[
    "uh", "uhh", "um", "umm", "uhm", "er", "erm", "ah", "ahh", "hmm", "hm", "mm", "mhm",
];
/// Words that carry no information of their own. Anything else that appears
/// only in the removed attempt ("on" vs "off", "MP4" vs "MOV") blocks an
/// automatic cut and is shown to the planner.
#[rustfmt::skip]
const GLUE: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "so", "then", "to", "of", "that", "this", "is", "it",
    "its", "i", "im", "we", "were", "you", "youre", "just", "like", "okay", "ok", "well", "now",
    "basically", "actually", "gonna", "going", "let", "lets", "me", "also", "with", "as", "for",
    "at", "by", "from", "in", "into", "has", "have", "are", "was", "be", "been",
];
/// Automatic cuts need an attempt of at least this many spoken words; shorter
/// repeats ("click next, click next") may be intentional steps.
const CLEAR_MIN_WORDS: usize = 3;

#[derive(Clone, Debug)]
struct Token {
    norm: String,
    line: usize,
    /// Index into `TimelineUtterance::words`, or `None` without usable alignment.
    word: Option<usize>,
    start: f64,
}

impl Token {
    fn filler(&self) -> bool {
        FILLERS.contains(&self.norm.as_str())
    }

    fn informative(&self) -> bool {
        !self.filler() && !GLUE.contains(&self.norm.as_str())
    }
}

/// A measured restart: inclusive token spans for the removal and kept attempt.
#[derive(Clone, Debug)]
pub(super) struct Restart {
    remove: (usize, usize),
    keep: (usize, usize),
    coverage: f64,
    unique: Vec<String>,
    /// Verbatim enough to add without planner approval.
    clear: bool,
}

/// Tokens are words with timing where alignment matches the utterance text.
/// Utterances without usable alignment still contribute words, so restarts can
/// be found, but they can only be cut as whole utterances.
fn tokens(utterances: &[TimelineUtterance<'_>]) -> Vec<Token> {
    let mut result = Vec::new();
    for (line_index, line) in utterances.iter().enumerate() {
        let aligned = !line.words.is_empty()
            && line
                .words
                .iter()
                .flat_map(|word| words(&word.text))
                .eq(words(line.text));
        if aligned {
            for (index, word) in line.words.iter().enumerate() {
                let norm: String = words(&word.text).collect();
                if !norm.is_empty() {
                    result.push(Token {
                        norm,
                        line: line_index,
                        word: Some(index),
                        start: word.speech.start,
                    });
                }
            }
        } else {
            for piece in line.text.split_whitespace() {
                let norm: String = words(piece).collect();
                if !norm.is_empty() {
                    result.push(Token {
                        norm,
                        line: line_index,
                        word: None,
                        start: line.speech.start,
                    });
                }
            }
        }
    }
    result
}

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
}

/// Longest common subsequence of `earlier` within `later`. Returns which
/// earlier words matched and the position in `later` of the last matched word.
/// The attempt's final word may be cut off mid-word ("con" for "concurrent").
fn common_subsequence(tokens: &[Token], earlier: &[usize], later: &[usize]) -> (Vec<bool>, usize) {
    let same = |row: usize, column: usize| {
        let (left, right) = (&tokens[earlier[row]], &tokens[later[column]]);
        left.norm == right.norm
            || (row + 1 == earlier.len()
                && left.informative()
                && left.norm.chars().count() >= 3
                && right.norm.starts_with(&left.norm))
    };
    let width = later.len() + 1;
    let mut table = vec![0u16; (earlier.len() + 1) * width];
    for row in 0..earlier.len() {
        for column in 0..later.len() {
            table[(row + 1) * width + column + 1] = if same(row, column) {
                table[row * width + column] + 1
            } else {
                table[row * width + column + 1].max(table[(row + 1) * width + column])
            };
        }
    }
    let total = table[earlier.len() * width + later.len()];
    // The kept attempt ends at the first column reaching the full match count.
    let end = (1..width)
        .find(|&column| table[earlier.len() * width + column] == total)
        .unwrap_or(1);
    let mut matched = vec![false; earlier.len()];
    let (mut row, mut column) = (earlier.len(), end);
    while row > 0 && column > 0 {
        if same(row - 1, column - 1)
            && table[row * width + column] == table[(row - 1) * width + column - 1] + 1
        {
            matched[row - 1] = true;
            row -= 1;
            column -= 1;
        } else if table[(row - 1) * width + column] >= table[row * width + column - 1] {
            row -= 1;
        } else {
            column -= 1;
        }
    }
    (matched, end - 1)
}

fn detect_tokens(tokens: &[Token]) -> Vec<Restart> {
    let spoken = |index: &usize| !tokens[*index].filler();
    let mut found: Vec<Restart> = Vec::new();
    let mut start = 0;
    while start < tokens.len() {
        if tokens[start].filler() {
            start += 1;
            continue;
        }
        let mut restart = None;
        for next in start + 1..tokens.len() {
            if next - start > MAX_ATTEMPT_WORDS * 2
                || tokens[next].start - tokens[start].start > MAX_ATTEMPT_SECONDS
            {
                break;
            }
            if tokens[next].norm != tokens[start].norm {
                continue;
            }
            let earlier: Vec<usize> = (start..next).filter(spoken).collect();
            if earlier.len() < 2 || earlier.len() > MAX_ATTEMPT_WORDS {
                continue;
            }
            let later: Vec<usize> = (next..tokens.len())
                .filter(spoken)
                .take(earlier.len() + LOOKAHEAD_SLACK)
                .collect();
            // Two matching opening words anchor a restart; one shared word is
            // ordinary speech.
            if later.len() < 2 || tokens[earlier[1]].norm != tokens[later[1]].norm {
                continue;
            }
            let (mut matched, last) = common_subsequence(tokens, &earlier, &later);
            let kept: BTreeSet<&str> = later[..=last]
                .iter()
                .map(|&index| tokens[index].norm.as_str())
                .collect();
            // Transcription often splits or joins compounds ("in built" vs
            // "inbuilt", "re writes" vs "rewrites"); credit those words.
            let joined = |first: usize, second: usize| {
                kept.contains(
                    format!(
                        "{}{}",
                        tokens[earlier[first]].norm, tokens[earlier[second]].norm
                    )
                    .as_str(),
                )
            };
            for (position, hit) in matched.iter_mut().enumerate() {
                *hit = *hit
                    || (position > 0 && joined(position - 1, position))
                    || (position + 1 < earlier.len() && joined(position, position + 1));
            }
            let credited = matched.iter().filter(|&&hit| hit).count();
            let coverage = credited as f64 / earlier.len() as f64;
            if coverage + 0.000_001 < MIN_COVERAGE || (earlier.len() == 2 && credited < 2) {
                continue;
            }
            let mut unique: Vec<String> = Vec::new();
            for (position, &index) in earlier.iter().enumerate() {
                let token = &tokens[index];
                if !matched[position]
                    && token.informative()
                    && !kept.contains(token.norm.as_str())
                    && !unique.contains(&token.norm)
                {
                    unique.push(token.norm.clone());
                }
            }
            // Show the kept attempt through the end of its utterance for review.
            let mut end = later[last];
            while end + 1 < tokens.len() && tokens[end + 1].line == tokens[end].line {
                end += 1;
            }
            // A trailing word abandoned mid-thought ("…panel, then") still
            // counts as clear when it carries no content of its own.
            let clear = unique.is_empty()
                && earlier.len() >= CLEAR_MIN_WORDS
                && (coverage >= CLEAR_COVERAGE - 0.000_001 || earlier.len() - credited <= 1);
            restart = Some(Restart {
                remove: (start, next - 1),
                keep: (next, end),
                coverage,
                unique,
                clear,
            });
            break;
        }
        match restart {
            Some(restart) => {
                start = restart.keep.0;
                found.push(restart);
            }
            None => start += 1,
        }
    }
    chain(found)
}

/// Collapse attempt chains so each removal keeps the final attempt, and trim a
/// kept span that a later restart partly removes. A clear restart is never
/// merged with a doubtful later link, which would hide it; the doubtful link
/// stays a separate candidate for the planner.
fn chain(found: Vec<Restart>) -> Vec<Restart> {
    let mut result: Vec<Restart> = Vec::with_capacity(found.len());
    for restart in found {
        if let Some(previous) = result.last_mut()
            && restart.remove.0 <= previous.keep.1
        {
            if restart.remove.0 <= previous.keep.0 {
                if previous.clear && !restart.clear {
                    result.push(restart);
                    continue;
                }
                previous.remove.1 = restart.remove.1;
                previous.keep = restart.keep;
                previous.coverage = previous.coverage.min(restart.coverage);
                previous.clear &= restart.clear;
                for word in restart.unique {
                    if !previous.unique.contains(&word) {
                        previous.unique.push(word);
                    }
                }
                continue;
            }
            previous.keep.1 = restart.remove.0 - 1;
        }
        result.push(restart);
    }
    result
}

/// Convert an inclusive token span into planner coordinates.
fn span(
    tokens: &[Token],
    (from, to): (usize, usize),
    utterances: &[TimelineUtterance<'_>],
) -> Option<(UtteranceRange, Option<WordRange>)> {
    let (first, last) = (&tokens[from], &tokens[to]);
    let range = UtteranceRange {
        first: first.line as u32 + 1,
        last: last.line as u32 + 1,
    };
    let starts_line = from == 0 || tokens[from - 1].line != first.line;
    let ends_line = to + 1 == tokens.len() || tokens[to + 1].line != last.line;
    if starts_line && ends_line {
        return Some((range, None));
    }
    let begin = if starts_line { 1 } else { first.word? + 1 };
    let end = if ends_line {
        utterances[last.line].words.len()
    } else {
        last.word? + 1
    };
    Some((
        range,
        Some(WordRange {
            first: begin,
            last: end,
        }),
    ))
}

fn spoken_text(tokens: &[Token], (from, to): (usize, usize)) -> String {
    tokens[from..=to]
        .iter()
        .map(|token| token.norm.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

struct Measured {
    restart: Restart,
    remove: (UtteranceRange, Option<WordRange>),
    keep: (UtteranceRange, Option<WordRange>),
    removed_text: String,
}

fn measure(utterances: &[TimelineUtterance<'_>]) -> Vec<Measured> {
    let tokens = tokens(utterances);
    detect_tokens(&tokens)
        .into_iter()
        .filter_map(|restart| {
            Some(Measured {
                remove: span(&tokens, restart.remove, utterances)?,
                keep: span(&tokens, restart.keep, utterances)?,
                removed_text: spoken_text(&tokens, restart.remove),
                restart,
            })
        })
        .collect()
}

impl Measured {
    fn proposal(&self, utterances: &[TimelineUtterance<'_>]) -> Option<Proposal> {
        let reason = if self.restart.clear {
            "Restarted passage: the later attempt repeats this one; keeping the final attempt."
                .to_owned()
        } else {
            format!(
                "Possible restart: {:.0}% of these words repeat in the later attempt.",
                self.restart.coverage * 100.0
            )
        };
        cut_proposals(
            utterances,
            vec![CutChoice {
                remove: self.remove.0,
                keep: Some(self.keep.0),
                remove_words: self.remove.1,
                keep_words: self.keep.1,
                reason,
            }],
        )
        .pop()
    }
}

/// Measured restart evidence for the planner, clear candidates first.
pub(super) fn evidence(utterances: &[TimelineUtterance<'_>]) -> Vec<Value> {
    let mut measured = measure(utterances);
    measured.sort_by(|left, right| {
        right
            .restart
            .clear
            .cmp(&left.restart.clear)
            .then(right.restart.coverage.total_cmp(&left.restart.coverage))
    });
    measured
        .into_iter()
        .take(MAX_EVIDENCE)
        .map(|entry| {
            json!([
                entry.remove.0,
                entry.remove.1,
                entry.keep.0,
                entry.keep.1,
                (entry.restart.coverage * 100.0).round() as u32,
                entry.restart.unique,
                entry.removed_text,
            ])
        })
        .collect()
}

/// Proposals for clear restarts only, used when no planner result is available.
pub(super) fn clear_proposals(utterances: &[TimelineUtterance<'_>]) -> Vec<Proposal> {
    let mut draft = Draft {
        summary: String::new(),
        proposals: Vec::new(),
        notes: Vec::new(),
    };
    complete(&mut draft, utterances);
    draft.proposals
}

/// Single-take pass: add clear measured restarts the planner left in place and
/// report the possible ones that still need a decision.
pub(super) fn complete(draft: &mut Draft, utterances: &[TimelineUtterance<'_>]) {
    let before = draft.proposals.clone();
    let mut added = 0;
    let mut blocked = 0;
    let mut unresolved = Vec::new();
    for entry in measure(utterances) {
        let Some(proposal) = entry.proposal(utterances) else {
            continue;
        };
        // The planner already decided this passage when one of its removals
        // touches it; never second-guess a reviewed edit.
        if draft
            .proposals
            .iter()
            .any(|existing| existing.kind != "pause" && existing.remove.overlaps(proposal.remove))
        {
            continue;
        }
        if !entry.restart.clear {
            unresolved.push(proposal.remove.start);
            continue;
        }
        // Pauses inside the removed attempt become redundant; anything else
        // that conflicts (a kept take, a nearby cut) is a decision to respect.
        let redundant = |existing: &Proposal| {
            existing.kind == "pause" && contains(proposal.remove, existing.remove)
        };
        let mut trial: Vec<Proposal> = draft
            .proposals
            .iter()
            .filter(|existing| !redundant(existing))
            .cloned()
            .collect();
        trial.push(proposal);
        bridge(&mut trial, utterances);
        let (candidate, others) = trial.split_last().unwrap();
        if others.iter().any(|existing| conflicts(candidate, existing)) {
            blocked += 1;
            continue;
        }
        draft.proposals = trial;
        added += 1;
    }
    if added > 0 && removes_all_speech(&draft.proposals, utterances) {
        draft.proposals = before;
        draft
            .notes
            .push("Measured restarts were not added because they would remove all speech.".into());
        return;
    }
    draft
        .proposals
        .sort_by(|left, right| left.remove.start.total_cmp(&right.remove.start));
    let plural = |count: usize| if count == 1 { "" } else { "s" };
    if added > 0 {
        draft.notes.push(format!(
            "Added {added} measured restart{} where the same words were spoken again, keeping the final attempt for a single take.",
            plural(added)
        ));
    }
    if blocked > 0 {
        draft.notes.push(format!(
            "{blocked} measured restart{} overlapped a reviewed edit or kept take and {} left unchanged.",
            plural(blocked),
            if blocked == 1 { "was" } else { "were" }
        ));
    }
    if !unresolved.is_empty() {
        let times = unresolved
            .iter()
            .take(6)
            .map(|time| format!("{time:.1} s"))
            .collect::<Vec<_>>()
            .join(", ");
        draft.notes.push(format!(
            "{} possible repeated passage{} kept because the earlier attempt may contain different details: {times}{}.",
            unresolved.len(),
            if unresolved.len() == 1 { " was" } else { "s were" },
            if unresolved.len() > 6 { ", …" } else { "" }
        ));
    }
}

fn contains(outer: super::Range, inner: super::Range) -> bool {
    outer.start <= inner.start + 0.000_001 && outer.end >= inner.end - 0.000_001
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleanup::{Range, audio::Word};

    /// Build one aligned utterance per entry, words 0.3 s apart.
    fn timeline(texts: &[&'static str]) -> Vec<TimelineUtterance<'static>> {
        let mut clock = 0.5;
        let count = texts.len();
        texts
            .iter()
            .enumerate()
            .map(|(ordinal, text)| {
                let words: Vec<Word> = text
                    .split_whitespace()
                    .map(|piece| {
                        let word = Word {
                            speech: Range::new(clock, clock + 0.25),
                            text: piece.into(),
                            anchor: None,
                        };
                        clock += 0.3;
                        word
                    })
                    .collect();
                let speech = Range::new(words[0].speech.start, words.last().unwrap().speech.end);
                clock += 0.6;
                TimelineUtterance {
                    run: 0,
                    ordinal,
                    last: ordinal + 1 == count,
                    speech,
                    cut_start: Some(speech.start - 0.15),
                    cut_end: Some(speech.end + 0.15),
                    text,
                    words,
                }
            })
            .collect()
    }

    #[test]
    fn removes_a_restart_inside_one_utterance_at_word_level() {
        let lines = timeline(&["So today we're, uh, so today we're going to build an exporter."]);
        let proposals = clear_proposals(&lines);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].kind, "retake");
        assert_eq!(
            proposals[0].removed_text.as_deref(),
            Some("So today we're, uh,")
        );
        assert!(
            proposals[0]
                .kept_text
                .as_deref()
                .unwrap()
                .starts_with("so today we're going")
        );
        assert!(!proposals[0].remove.overlaps(proposals[0].keep.unwrap()));
    }

    #[test]
    fn collapses_a_chain_of_attempts_and_keeps_the_last_one() {
        let lines = timeline(&[
            "Open the export panel and",
            "Open the export panel, then",
            "Open the export panel and choose MP4 at full quality.",
        ]);
        let proposals = clear_proposals(&lines);
        assert_eq!(proposals.len(), 1, "{proposals:?}");
        let proposal = &proposals[0];
        assert!(proposal.remove.start < lines[0].speech.start);
        assert!(proposal.remove.end >= lines[1].speech.end);
        assert_eq!(proposal.keep.unwrap().start, lines[2].speech.start);
    }

    #[test]
    fn keeps_repeats_whose_earlier_attempt_has_different_details() {
        let lines = timeline(&[
            "You can export to MP4 here.",
            "You can export to MOV here as well.",
        ]);
        assert!(clear_proposals(&lines).is_empty());
        let evidence = evidence(&lines);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0][5], json!(["mp4"]));
        let mut draft = Draft {
            summary: "Nothing".into(),
            proposals: Vec::new(),
            notes: Vec::new(),
        };
        complete(&mut draft, &lines);
        assert!(draft.proposals.is_empty());
        assert!(draft.notes[0].contains("possible repeated passage"));
    }

    #[test]
    fn planner_can_copy_candidate_ranges_into_a_valid_retake() {
        let lines = timeline(&[
            "Before we start, check the audio. So the export, uh,",
            "so the export dialog has three tabs.",
        ]);
        let evidence = evidence(&lines);
        assert_eq!(evidence.len(), 1, "{evidence:?}");
        let row = &evidence[0];
        let choice: super::super::CutChoice = serde_json::from_value(json!({
            "remove": row[0], "removeWords": row[1], "keep": row[2], "keepWords": row[3],
            "reason": "Restart",
        }))
        .unwrap();
        let proposals = cut_proposals(&lines, vec![choice]);
        assert_eq!(proposals.len(), 1);
        assert_eq!(
            proposals[0].removed_text.as_deref(),
            Some("So the export, uh,")
        );
        assert_eq!(
            proposals[0].kept_text.as_deref(),
            Some("so the export dialog has three tabs.")
        );
        // The unrelated opening of the first utterance is retained.
        assert!(proposals[0].remove.start > lines[0].words[5].speech.end);
    }

    #[test]
    fn real_attempt_chain_with_cut_off_and_split_words() {
        // Shape of a real recording: a clear restart, a doubtful rephrase, and a
        // chain ending in a cut-off word and a compound the transcriber split.
        let lines = timeline(&[
            "Also we all know",
            "Also we all know that Rust has reliable concurrency memory management",
            "Also we all know",
            "Also rust has",
            "Also Rust has in built con",
            "Also Rust has inbuilt safety for concurrent threads memory management",
        ]);
        let proposals = clear_proposals(&lines);
        let removed: Vec<_> = proposals
            .iter()
            .map(|proposal| proposal.removed_text.clone().unwrap())
            .collect();
        assert_eq!(
            removed,
            [
                "Also we all know",
                "Also rust has Also Rust has in built con"
            ],
            "{proposals:?}"
        );
        assert_eq!(proposals[1].keep, Some(lines[5].speech));
        // The doubtful middle link still reaches the planner as evidence.
        assert!(evidence(&lines).len() >= 3);
    }

    #[test]
    fn short_or_contrasting_steps_are_left_to_the_planner() {
        for texts in [
            ["Turn it on.", "Turn it off and wait."],
            ["Click next,", "click next, then finish."],
        ] {
            let lines = timeline(&texts);
            assert!(clear_proposals(&lines).is_empty(), "{texts:?}");
            assert_eq!(evidence(&lines).len(), 1, "{texts:?}");
        }
    }

    #[test]
    fn ignores_ordinary_speech_and_distant_shared_words() {
        let lines = timeline(&[
            "We click save to store the project.",
            "Then the render queue starts and we wait for the progress bar.",
            "Finally we click the share button to publish.",
        ]);
        assert!(evidence(&lines).is_empty());
    }

    #[test]
    fn respects_planner_decisions_and_trims_redundant_pauses() {
        let lines = timeline(&[
            "Set the frame rate to thirty.",
            "Set the frame rate to thirty and press render.",
        ]);
        let pause = Proposal {
            watch: false,
            kind: "pause",
            remove: Range::new(lines[0].speech.end + 0.05, lines[0].speech.end + 0.1),
            keep: None,
            removed_text: None,
            kept_text: None,
            reason: Some("Quiet".into()),
        };
        let mut draft = Draft {
            summary: "Draft".into(),
            proposals: vec![pause],
            notes: Vec::new(),
        };
        complete(&mut draft, &lines);
        assert_eq!(draft.proposals.len(), 1, "{:?}", draft.proposals);
        assert_eq!(draft.proposals[0].kind, "retake");
        assert!(draft.notes[0].contains("Added 1 measured restart"));

        // The planner kept the earlier attempt deliberately: leave it alone.
        let keep_first = Proposal {
            watch: false,
            kind: "retake",
            remove: Range::new(lines[1].cut_start.unwrap(), lines[1].cut_end.unwrap()),
            keep: Some(lines[0].speech),
            removed_text: None,
            kept_text: None,
            reason: Some("Planner".into()),
        };
        let mut draft = Draft {
            summary: "Draft".into(),
            proposals: vec![keep_first.clone()],
            notes: Vec::new(),
        };
        complete(&mut draft, &lines);
        assert_eq!(draft.proposals.len(), 1);
        assert_eq!(draft.proposals[0].remove, keep_first.remove);
    }

    #[test]
    fn unaligned_utterances_can_only_be_removed_whole() {
        let mut lines = timeline(&[
            "Let me show the timeline view.",
            "Let me show the timeline view and its tracks.",
        ]);
        lines[0].words.clear();
        let proposals = clear_proposals(&lines);
        assert_eq!(proposals.len(), 1);
        assert_eq!(
            proposals[0].remove,
            Range::new(lines[0].cut_start.unwrap(), lines[0].cut_end.unwrap())
        );
        // A partial cut inside an unaligned utterance is never invented.
        let mut lines = timeline(&["Intro words here. Let me show, let me show the view."]);
        lines[0].words.clear();
        assert!(clear_proposals(&lines).is_empty());
    }

    #[test]
    fn keeps_the_final_attempt_of_a_verbatim_repeat() {
        let lines = timeline(&["Hello there everyone", "Hello there everyone"]);
        let proposals = clear_proposals(&lines);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].keep, Some(lines[1].speech));
    }
}
