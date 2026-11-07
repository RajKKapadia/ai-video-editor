//! Keep provider requests bounded while reviewing every part of a long timeline.
use std::future::Future;
use std::{collections::BTreeSet, ops::Range as Window};

use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{
    Connection, Gap, Proposal, Range, TimelineUtterance, conflicts,
    draft::{self, Draft},
};

const WINDOW_BYTES: usize = 40 * 1024;
const CONTEXT_LINES: usize = 3;
const MAX_WINDOW_LINES: usize = 64;

pub(super) fn windows(lines: &[TimelineUtterance<'_>]) -> Vec<Window<usize>> {
    let mut result = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let mut bytes = 0;
        let mut end = start;
        while let Some(line) = lines.get(end) {
            let size = json!([line.speech.start, line.speech.end, line.text])
                .to_string()
                .len()
                + 100;
            if end > start && (bytes + size > WINDOW_BYTES || end - start >= MAX_WINDOW_LINES) {
                break;
            }
            bytes += size;
            end += 1;
        }
        result.push(start..end);
        if end == lines.len() {
            break;
        }
        start = end.saturating_sub(CONTEXT_LINES).max(start + 1);
    }
    result
}

fn context_gaps(gaps: &[Gap], window: &Window<usize>, total: usize) -> Vec<Gap> {
    gaps.iter()
        .filter(|gap| {
            (gap.before == 0 && window.start == 0
                || gap.before > window.start && gap.before <= window.end)
                && (gap.after == 0 && window.end == total
                    || gap.after > window.start && gap.after <= window.end)
        })
        .enumerate()
        .map(|(index, gap)| Gap {
            id: index + 1,
            range: gap.range,
            before: if gap.before == 0 {
                0
            } else {
                gap.before - window.start
            },
            after: if gap.after == 0 {
                0
            } else {
                gap.after - window.start
            },
        })
        .collect()
}

pub(super) async fn run(
    connection: &Connection,
    brief: &str,
    lines: &[TimelineUtterance<'_>],
    gaps: &[Gap],
    cancel: &CancellationToken,
    compare_retakes: bool,
) -> Result<Draft, String> {
    execute(
        brief,
        lines,
        gaps,
        cancel,
        compare_retakes,
        |direction, local, local_gaps| async move {
            draft::run_window(connection, &direction, &local, &local_gaps, cancel).await
        },
    )
    .await
}

async fn execute<'a, F, Fut>(
    brief: &str,
    lines: &[TimelineUtterance<'a>],
    gaps: &[Gap],
    cancel: &CancellationToken,
    compare_retakes: bool,
    mut complete: F,
) -> Result<Draft, String>
where
    F: FnMut(String, Vec<TimelineUtterance<'a>>, Vec<Gap>) -> Fut,
    Fut: Future<Output = Result<Draft, String>>,
{
    let windows = windows(lines);
    let mut proposals = Vec::new();
    let mut notes = Vec::new();
    let mut reviewed = 0;
    let mut unavailable = None;
    for (index, window) in windows.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err("Timeline analysis cancelled.".into());
        }
        let local_gaps = context_gaps(gaps, window, lines.len());
        let direction = format!(
            "{brief}\nThis is section {} of {} in a longer timeline. Review all supplied speech; neighboring sections include overlapping context. Preserve unique details.",
            index + 1,
            windows.len()
        );
        match complete(direction, lines[window.clone()].to_vec(), local_gaps).await {
            Ok(draft) => {
                reviewed += 1;
                proposals.extend(draft.proposals);
                notes.extend(draft.notes);
            }
            Err(error) if !cancel.is_cancelled() => {
                notes.push(format!("Section {} was left unchanged: {error}", index + 1));
                if provider_unavailable(&error) {
                    unavailable = Some(error);
                    break;
                }
            }
            Err(error) => return Err(error),
        }
    }
    if proposals.is_empty()
        && let Some(error) = &unavailable
    {
        return Err(error.clone());
    }
    // Compare likely repetitions even when their takes fall in distant sections.
    // This index only finds candidates; the provider decides whether they are
    // redundant takes or intentional repetitions and selects the complete take.
    let mut compared = BTreeSet::new();
    for hint in if compare_retakes && unavailable.is_none() {
        draft::repeat_hints(lines)
    } else {
        Vec::new()
    } {
        if cancel.is_cancelled() {
            return Err("Timeline analysis cancelled.".into());
        }
        let a = hint["earlier"]["first"].as_u64().unwrap() as usize - 1;
        let b = hint["earlier"]["last"].as_u64().unwrap() as usize;
        let c = hint["later"]["first"].as_u64().unwrap() as usize - 1;
        let d = hint["later"]["last"].as_u64().unwrap() as usize;
        if windows
            .iter()
            .any(|window| window.start <= a && window.end >= d)
        {
            continue;
        }
        if !compared.insert((a, b, c, d)) {
            continue;
        }
        let indices: BTreeSet<_> = (a.saturating_sub(CONTEXT_LINES)
            ..(b + CONTEXT_LINES).min(lines.len()))
            .chain(c.saturating_sub(CONTEXT_LINES)..(d + CONTEXT_LINES).min(lines.len()))
            .collect();
        let comparison: Vec<_> = indices
            .into_iter()
            .map(|index| lines[index].clone())
            .collect();
        let direction = format!(
            "{brief}\nCompare these potentially repeated passages from distant parts of the timeline. Remove only clear redundant versions, preserving intentional repetition, all unique details and the clearest complete version. Do not remove unrelated false starts or gaps in this comparison."
        );
        match complete(direction, comparison, Vec::new()).await {
            Ok(draft) => {
                proposals.extend(
                    draft
                        .proposals
                        .into_iter()
                        .filter(|proposal| proposal.kind == "retake"),
                );
                notes.extend(draft.notes);
            }
            Err(error) if !cancel.is_cancelled() => notes.push(format!(
                "A comparison between distant passages could not finish: {error}"
            )),
            Err(error) => return Err(error),
        }
    }
    if cancel.is_cancelled() {
        return Err("Timeline analysis cancelled.".into());
    }
    let proposals = combine(proposals, &mut notes);
    Ok(Draft {
        summary: format!(
            "Reviewed {} of {} sections of the timeline and prepared {} edits. Review any notes before applying.",
            reviewed,
            windows.len(),
            proposals.len()
        ),
        proposals,
        notes,
    })
}

fn provider_unavailable(error: &str) -> bool {
    [
        "temporarily unavailable",
        "Could not connect to the AI provider",
        "The AI provider timed out",
        "rejected the API key",
        "rate or quota limit",
        "model or API endpoint was not found",
    ]
    .iter()
    .any(|message| error.contains(message))
}

fn contains(outer: Range, inner: Range) -> bool {
    outer.start <= inner.start + 0.000_001 && outer.end >= inner.end - 0.000_001
}

fn combine(mut proposals: Vec<Proposal>, notes: &mut Vec<String>) -> Vec<Proposal> {
    proposals.sort_by(|left, right| {
        left.remove
            .start
            .total_cmp(&right.remove.start)
            .then(left.remove.end.total_cmp(&right.remove.end))
    });
    proposals.dedup_by(|left, right| {
        left.kind == right.kind && left.remove == right.remove && left.keep == right.keep
    });
    let original = proposals.clone();
    for proposal in &mut proposals {
        let mut visited = Vec::new();
        while let Some(keep) = proposal.keep {
            if visited.contains(&keep) {
                break;
            }
            visited.push(keep);
            let Some(replacement) = original.iter().find(|other| {
                other.kind == "retake" && contains(other.remove, keep) && other.keep.is_some()
            }) else {
                break;
            };
            proposal.keep = replacement.keep;
            proposal.kept_text = replacement.kept_text.clone();
        }
    }
    proposals.dedup_by(|left, right| {
        left.kind == right.kind && left.remove == right.remove && left.keep == right.keep
    });
    let valid: Vec<_> = proposals
        .iter()
        .enumerate()
        .filter(|(index, proposal)| {
            !proposal
                .keep
                .is_some_and(|keep| proposal.remove.overlaps(keep))
                && !proposals
                    .iter()
                    .enumerate()
                    .any(|(other, candidate)| *index != other && conflicts(proposal, candidate))
        })
        .map(|(_, proposal)| proposal.clone())
        .collect();
    if valid.len() < proposals.len() {
        notes.push(format!("{} edits conflicted between sections and were left unchanged to preserve retained speech.", proposals.len() - valid.len()));
    }
    valid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_sequences_are_fully_covered_by_overlapping_bounded_windows() {
        let text = "word ".repeat(400);
        let lines: Vec<_> = (0..100)
            .map(|index| TimelineUtterance {
                run: 0,
                ordinal: index,
                last: index == 99,
                speech: Range::new(index as f64, index as f64 + 0.7),
                cut_start: Some(index as f64),
                cut_end: Some(index as f64 + 0.8),
                text: &text,
                words: Vec::new(),
            })
            .collect();
        let windows = windows(&lines);
        assert!(windows.len() > 1);
        assert_eq!(windows[0].start, 0);
        assert_eq!(windows.last().unwrap().end, lines.len());
        assert!(
            windows
                .windows(2)
                .all(|pair| pair[0].end - pair[1].start == CONTEXT_LINES)
        );
    }

    #[test]
    fn a_kept_take_can_be_replaced_by_a_more_complete_distant_take_without_conflicts() {
        let take = |remove, keep| Proposal {
            kind: "retake",
            remove,
            keep: Some(keep),
            removed_text: None,
            kept_text: Some("Complete take".into()),
            reason: None,
        };
        let proposals = vec![
            take(Range::new(0.0, 2.0), Range::new(10.2, 11.8)),
            take(Range::new(10.0, 12.0), Range::new(20.2, 21.8)),
        ];
        let mut notes = Vec::new();
        let combined = combine(proposals, &mut notes);
        assert_eq!(combined.len(), 2);
        assert!(
            combined
                .iter()
                .all(|proposal| proposal.keep == Some(Range::new(20.2, 21.8)))
        );
        assert!(notes.is_empty());
    }

    #[tokio::test]
    async fn reviews_every_section_and_compares_a_distant_duplicate() {
        let mut texts: Vec<_> = (0..100)
            .map(|index| format!("Unique passage number {index}: {}", "sample ".repeat(250)))
            .collect();
        texts[95] = texts[4].clone();
        let lines: Vec<_> = texts
            .iter()
            .enumerate()
            .map(|(index, text)| TimelineUtterance {
                run: 0,
                ordinal: index,
                last: index == 99,
                speech: Range::new(index as f64 * 10.0 + 0.2, index as f64 * 10.0 + 8.0),
                cut_start: Some(index as f64 * 10.0),
                cut_end: Some(index as f64 * 10.0 + 8.15),
                text,
                words: Vec::new(),
            })
            .collect();
        let mut covered = BTreeSet::new();
        let mut comparisons = 0;
        let result = execute(
            "Keep the clearest complete take",
            &lines,
            &[],
            &CancellationToken::new(),
            true,
            |direction, local, _| {
                covered.extend(local.iter().map(|line| line.ordinal));
                let proposals = if direction.contains("distant parts") {
                    comparisons += 1;
                    let early = local.iter().find(|line| line.ordinal == 4).unwrap();
                    let late = local.iter().find(|line| line.ordinal == 95).unwrap();
                    vec![Proposal {
                        kind: "retake",
                        remove: Range::new(early.cut_start.unwrap(), early.cut_end.unwrap()),
                        keep: Some(late.speech),
                        removed_text: Some(early.text.into()),
                        kept_text: Some(late.text.into()),
                        reason: Some("Complete later take".into()),
                    }]
                } else {
                    Vec::new()
                };
                std::future::ready(Ok(Draft {
                    summary: "Reviewed".into(),
                    proposals,
                    notes: Vec::new(),
                }))
            },
        )
        .await
        .unwrap();
        assert_eq!(covered.len(), 100);
        assert_eq!(comparisons, 1);
        assert_eq!(result.proposals.len(), 1);
        assert_eq!(result.proposals[0].remove, Range::new(40.0, 48.15));
        assert_eq!(result.proposals[0].keep, Some(lines[95].speech));
    }

    #[tokio::test]
    async fn long_review_cancellation_stops_before_another_provider_call() {
        let text = "sample ".repeat(600);
        let lines: Vec<_> = (0..100)
            .map(|index| TimelineUtterance {
                run: 0,
                ordinal: index,
                last: index == 99,
                speech: Range::new(index as f64, index as f64 + 0.8),
                cut_start: Some(index as f64),
                cut_end: Some(index as f64 + 0.9),
                text: &text,
                words: Vec::new(),
            })
            .collect();
        let cancel = CancellationToken::new();
        let mut calls = 0;
        let result = execute("Brief", &lines, &[], &cancel, true, |_, _, _| {
            calls += 1;
            cancel.cancel();
            std::future::ready(Ok(Draft {
                summary: "Reviewed".into(),
                proposals: Vec::new(),
                notes: Vec::new(),
            }))
        })
        .await;
        assert!(result.err().unwrap().contains("cancelled"));
        assert_eq!(calls, 1);
    }
}
