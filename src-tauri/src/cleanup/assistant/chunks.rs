//! Review a whole transcript first, with budgeted overlapping sections as fallback.
use std::future::Future;
use std::{
    collections::{BTreeSet, VecDeque},
    ops::Range as Window,
};

use tokio_util::sync::CancellationToken;

use super::{
    Connection, Gap, Proposal, Range, TimelineUtterance, conflicts,
    draft::{self, Draft},
    planning::Budget,
};

const CONTEXT_LINES: usize = 3;

pub(super) fn windows(
    brief: &str,
    lines: &[TimelineUtterance<'_>],
    gaps: &[Gap],
    budget: Budget,
) -> Vec<Window<usize>> {
    let mut result = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        // Binary search the largest complete section that fits the full request,
        // including its local gap evidence, instructions, schema and headroom.
        let mut low = start + 1;
        let mut high = lines.len();
        let mut end = low;
        while low <= high {
            let mid = low + (high - low) / 2;
            let window = start..mid;
            if draft::fits(
                brief,
                &lines[window.clone()],
                &context_gaps(gaps, &window, lines.len()),
                budget,
            ) {
                end = mid;
                low = mid + 1;
            } else {
                high = mid - 1;
            }
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
    let budget = connection.budget();
    let full = draft::fits(brief, lines, gaps, budget);
    {
        let mut diagnostics = connection.planning.lock().unwrap();
        diagnostics.strategy = if full { "fullTimeline" } else { "sections" }.into();
        diagnostics.context_budget_tokens = budget.context_tokens;
        diagnostics.output_budget_tokens = budget.output_tokens;
        diagnostics.fallback_reason =
            (!full).then(|| "The complete request exceeds the configured planning budget.".into());
    }
    let result = execute(
        brief,
        lines,
        gaps,
        cancel,
        compare_retakes,
        budget,
        |scope, direction, local, local_gaps| async move {
            draft::run_window(connection, &direction, &local, &local_gaps, cancel, scope).await
        },
    )
    .await;
    let mut diagnostics = connection.planning.lock().unwrap();
    diagnostics.total_utterances = lines.len();
    diagnostics.reviewed_utterances = lines
        .iter()
        .filter(|line| {
            diagnostics.reviews.iter().any(|review| {
                review.completed
                    && review.scope != "comparison"
                    && review.start <= line.speech.start + 0.000_001
                    && review.end >= line.speech.end - 0.000_001
            })
        })
        .count();
    if diagnostics
        .reviews
        .iter()
        .any(|review| review.scope == "section")
    {
        diagnostics.strategy = "sections".into();
        if diagnostics.fallback_reason.is_none() {
            diagnostics.fallback_reason = Some("The provider could not complete the full request; smaller overlapping sections were used.".into());
        }
    }
    result
}

fn capacity_error(error: &str) -> bool {
    [
        "cannot fit",
        "input limit",
        "context budget",
        "output limit",
        "planning timed out",
        "planning limit",
    ]
    .iter()
    .any(|marker| error.contains(marker))
}

#[allow(clippy::too_many_arguments)]
async fn execute<'a, F, Fut>(
    brief: &str,
    lines: &[TimelineUtterance<'a>],
    gaps: &[Gap],
    cancel: &CancellationToken,
    compare_retakes: bool,
    budget: Budget,
    mut complete: F,
) -> Result<Draft, String>
where
    F: FnMut(&'static str, String, Vec<TimelineUtterance<'a>>, Vec<Gap>) -> Fut,
    Fut: Future<Output = Result<Draft, String>>,
{
    let mut fallback = None;
    if cancel.is_cancelled() {
        return Err("Timeline analysis cancelled.".into());
    }
    if draft::fits(brief, lines, gaps, budget) {
        match complete("fullTimeline", brief.into(), lines.to_vec(), gaps.to_vec()).await {
            Ok(draft) => {
                cancel_check(cancel)?;
                return Ok(draft);
            }
            Err(error) if capacity_error(&error) && lines.len() > 1 && !cancel.is_cancelled() => {
                fallback = Some(error)
            }
            Err(error) => return Err(error),
        }
    }
    let direction = format!(
        "{brief}\nThis is an overlapping section of a longer timeline. Review all supplied speech and boundary context. Preserve every unique detail; other sections are reviewed separately."
    );
    let section_budget = if fallback.is_some() {
        Budget::new(budget.context_tokens / 2)
    } else {
        budget
    };
    let mut windows = windows(&direction, lines, gaps, section_budget);
    if windows.len() == 1 && fallback.is_some() {
        windows = split_window(0..lines.len());
    }
    let mut queue: VecDeque<_> = windows.into();
    let mut proposals = Vec::new();
    let mut notes = Vec::new();
    let mut covered = BTreeSet::new();
    let mut reviewed_gaps = BTreeSet::new();
    let mut successful_windows = Vec::new();
    let mut unavailable = None;
    if let Some(error) = fallback {
        notes.push(format!("The full-timeline request could not finish; reviewed smaller overlapping sections instead. {error}"));
    }
    while let Some(window) = queue.pop_front() {
        cancel_check(cancel)?;
        let local_gaps = context_gaps(gaps, &window, lines.len());
        let gap_ranges: Vec<_> = local_gaps
            .iter()
            .map(|gap| (gap.range.start.to_bits(), gap.range.end.to_bits()))
            .collect();
        match complete(
            "section",
            direction.clone(),
            lines[window.clone()].to_vec(),
            local_gaps,
        )
        .await
        {
            Ok(draft) => {
                covered.extend(window.clone());
                reviewed_gaps.extend(gap_ranges);
                successful_windows.push(window);
                proposals.extend(draft.proposals);
                notes.extend(draft.notes);
            }
            Err(error) if !cancel.is_cancelled() && capacity_error(&error) && window.len() > 1 => {
                for smaller in split_window(window).into_iter().rev() {
                    queue.push_front(smaller);
                }
            }
            Err(error) if !cancel.is_cancelled() => {
                notes.push(format!(
                    "Transcript entries {}–{} were left unchanged: {error}",
                    window.start + 1,
                    window.end
                ));
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
    // Overlap alone cannot compare a restart many minutes away. Compare likely
    // repeated passages across the original, immutable transcript as well.
    let mut compared = BTreeSet::new();
    let hints = if compare_retakes && unavailable.is_none() {
        draft::repeat_hints(lines)
    } else {
        Vec::new()
    };
    if hints.len() == 64 {
        notes.push("Distant-passage discovery reached its 64-candidate limit; additional rephrased repetitions may need review.".into());
    }
    for hint in hints {
        cancel_check(cancel)?;
        let a = hint["earlier"]["first"].as_u64().unwrap() as usize - 1;
        let b = hint["earlier"]["last"].as_u64().unwrap() as usize;
        let c = hint["later"]["first"].as_u64().unwrap() as usize - 1;
        let d = hint["later"]["last"].as_u64().unwrap() as usize;
        if successful_windows
            .iter()
            .any(|window| window.start <= a && window.end >= d)
            || !compared.insert((a, b, c, d))
        {
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
            "{brief}\nCompare potentially repeated passages from distant parts of the timeline. Remove only redundant versions and identify the complete kept version. Preserve intentional repetition and every unique fact, number, setting and qualification. Do not remove unrelated false starts or gaps in this comparison."
        );
        match complete("comparison", direction, comparison, Vec::new()).await {
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
    cancel_check(cancel)?;
    if covered.len() < lines.len() {
        notes.push(format!("Review is incomplete: {} of {} transcript entries were reviewed successfully. Unreviewed material was left unchanged.", covered.len(), lines.len()));
    }
    let missing_gaps = gaps
        .iter()
        .filter(|gap| {
            !reviewed_gaps.contains(&(gap.range.start.to_bits(), gap.range.end.to_bits()))
        })
        .count();
    if missing_gaps > 0 {
        notes.push(format!("{missing_gaps} quiet gaps could not be reviewed with complete neighboring context and were left unchanged."));
    }
    let mut proposals = combine(proposals, &mut notes);
    if super::removes_all_speech(&proposals, lines) {
        proposals.clear();
        notes.push(
            "The combined section drafts would remove all speech and were left unchanged.".into(),
        );
    }
    Ok(Draft {
        summary: format!(
            "Reviewed {} of {} transcript entries across {} sections and prepared {} edits. Review any notes before applying.",
            covered.len(),
            lines.len(),
            successful_windows.len(),
            proposals.len()
        ),
        proposals,
        notes,
    })
}

fn cancel_check(cancel: &CancellationToken) -> Result<(), String> {
    if cancel.is_cancelled() {
        Err("Timeline analysis cancelled.".into())
    } else {
        Ok(())
    }
}

fn split_window(window: Window<usize>) -> Vec<Window<usize>> {
    let mid = window.start + window.len() / 2;
    let overlap = CONTEXT_LINES.min(window.len().saturating_sub(2) / 4);
    vec![window.start..mid + overlap, mid - overlap..window.end]
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

    fn compact_lines(count: usize) -> Vec<TimelineUtterance<'static>> {
        (0..count)
            .map(|ordinal| TimelineUtterance {
                run: 0,
                ordinal,
                last: ordinal + 1 == count,
                speech: Range::new(ordinal as f64 * 3.0 + 0.2, ordinal as f64 * 3.0 + 2.0),
                cut_start: Some(ordinal as f64 * 3.0),
                cut_end: Some(ordinal as f64 * 3.0 + 2.15),
                text: "Review the export settings and choose MP4, then retain every unique detail in the complete explanation.",
                words: Vec::new(),
            })
            .collect()
    }

    fn empty_draft() -> Draft {
        Draft {
            summary: "Preserve the speech".into(),
            proposals: Vec::new(),
            notes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn sends_all_159_utterances_together_when_the_request_fits() {
        let lines = compact_lines(159);
        let mut calls = 0;
        let gaps = super::super::transcript_gaps([(0, Range::new(2.15, 3.05))], &lines);
        execute(
            "Review the entire recording",
            &lines,
            &gaps,
            &CancellationToken::new(),
            true,
            Budget::default(),
            |scope, _, local, local_gaps| {
                calls += 1;
                assert_eq!(scope, "fullTimeline");
                assert_eq!(local.len(), 159);
                assert_eq!(local_gaps.len(), 1);
                assert_eq!(local[158].ordinal, 158);
                std::future::ready(Ok(empty_draft()))
            },
        )
        .await
        .unwrap();
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn provider_capacity_rejections_shrink_sections_without_losing_coverage() {
        let lines = compact_lines(159);
        let mut covered = BTreeSet::new();
        let mut whole_attempts = 0;
        let result = execute(
            "Keep unique information",
            &lines,
            &[],
            &CancellationToken::new(),
            false,
            Budget::default(),
            |scope, _, local, _| {
                if scope == "fullTimeline" {
                    whole_attempts += 1;
                }
                let result = if local.len() > 24 {
                    Err("The selected model cannot fit this timeline transcript.".into())
                } else {
                    covered.extend(local.iter().map(|line| line.ordinal));
                    Ok(empty_draft())
                };
                std::future::ready(result)
            },
        )
        .await
        .unwrap();
        assert_eq!(whole_attempts, 1);
        assert_eq!(covered.len(), 159);
        assert!(result.summary.contains("159 of 159"));
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("full-timeline"))
        );
    }

    #[tokio::test]
    async fn an_unrecoverable_section_is_reported_as_incomplete() {
        let text = "unique ".repeat(1000);
        let mut lines = compact_lines(10);
        for line in &mut lines {
            line.text = &text;
        }
        let mut calls = 0;
        let result = execute(
            "Review",
            &lines,
            &[],
            &CancellationToken::new(),
            false,
            Budget::new(8192),
            |_, _, _, _| {
                calls += 1;
                std::future::ready(if calls == 1 {
                    Err("Invalid draft".into())
                } else {
                    Ok(empty_draft())
                })
            },
        )
        .await
        .unwrap();
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("Review is incomplete"))
        );
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("left unchanged"))
        );
    }

    #[test]
    fn rejects_rushed_micro_fragments_but_allows_a_single_adjacent_join() {
        let pause = |start, end| Proposal {
            kind: "pause",
            remove: Range::new(start, end),
            keep: None,
            removed_text: None,
            kept_text: None,
            reason: None,
        };
        let mut notes = Vec::new();
        assert!(combine(vec![pause(1.0, 2.0), pause(2.1, 3.0)], &mut notes).is_empty());
        assert!(!notes.is_empty());
        assert_eq!(
            combine(vec![pause(1.0, 2.0), pause(2.0, 3.0)], &mut Vec::new()).len(),
            2
        );
        assert_eq!(
            combine(vec![pause(1.0, 2.0), pause(2.15, 3.0)], &mut Vec::new()).len(),
            2
        );
    }

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
        let windows = windows("Brief", &lines, &[], Budget::default());
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
            .map(|index| {
                format!(
                    "Passage{index} content{index} value{index} topic{index} {}",
                    "sample ".repeat(250)
                )
            })
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
            Budget::default(),
            |_, direction, local, _| {
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
        let result = execute(
            "Brief",
            &lines,
            &[],
            &cancel,
            true,
            Budget::default(),
            |_, _, _, _| {
                calls += 1;
                cancel.cancel();
                std::future::ready(Ok(Draft {
                    summary: "Reviewed".into(),
                    proposals: Vec::new(),
                    notes: Vec::new(),
                }))
            },
        )
        .await;
        assert!(result.err().unwrap().contains("cancelled"));
        assert_eq!(calls, 1);
    }
}
