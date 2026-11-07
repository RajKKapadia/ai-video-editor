import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { CutReview, CutReviewActions, type CutReviewProps } from './CleanupPanel';
import { Icon } from './Icon';
import {
  errorMessage, providers,
  type AssistantRequest, type CleanupProgress, type ProviderStatus,
} from './cleanupNative';
import type { AssistantMessage, CleanupReport } from './cleanupTimeline';

type Props = {
  projectId: string;
  providerRevision: number;
  hasAudio: boolean;
  progress: CleanupProgress | null;
  error: string;
  report: CleanupReport | null;
  reviewingPauses: boolean;
  needsCuts: boolean;
  messages: AssistantMessage[];
  review: CutReviewProps;
  onRun: (request: AssistantRequest) => void;
  onApplyPauses: (request: AssistantRequest) => void;
  onCancel: () => void;
  onClose: () => void;
  onSettings: (tab: 'speech' | 'providers') => void;
};

export function AssistantPanel({
  projectId, providerRevision, hasAudio, progress, error, report, reviewingPauses, needsCuts,
  messages, review, onRun, onApplyPauses, onCancel, onClose, onSettings,
}: Props) {
  const [providerStatus, setProviderStatus] = useState<ProviderStatus | null>(null);
  const [providerError, setProviderError] = useState('');
  const [saved, setSaved] = useState<AssistantMessage[]>([]);
  const [historyError, setHistoryError] = useState('');
  const [prompt, setPrompt] = useState('');
  const chat = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let current = true;
    void invoke<ProviderStatus>('ai_provider_status').then((status) => {
      if (current) {
        setProviderStatus(status);
        setProviderError('');
      }
    }).catch((err) => {
      if (current) setProviderError(errorMessage(err, 'Could not load AI provider settings.'));
    });
    return () => { current = false; };
  }, [providerRevision]);

  useEffect(() => {
    let current = true;
    void invoke<AssistantMessage[]>('assistant_history', { projectId }).then((history) => {
      if (current) setSaved(history);
    }).catch((err) => {
      if (current) setHistoryError(errorMessage(err, 'Could not load the assistant conversation.'));
    });
    return () => { current = false; };
  }, [projectId]);

  const conversation = [...new Map([...saved, ...messages].map((message) => [message.id, message])).values()]
    .sort((left, right) => left.id - right.id);
  useEffect(() => {
    if (chat.current) chat.current.scrollTop = chat.current.scrollHeight;
  }, [conversation.length]);

  const active = providerStatus?.active;
  const profile = active ? providerStatus?.profiles[active] : undefined;
  const ready = !!(active && profile?.ready && !providerError);
  const canRun = hasAudio && ready && !progress;
  function submit(stage: AssistantRequest['stage'], fallback: string, apply = false) {
    if (!canRun || !active || !profile) return;
    const message = prompt.trim() || fallback;
    if (!message) return;
    const request = {
      stage, message, provider: { provider: active, config: { model: profile.model, baseUrl: profile.baseUrl } },
    };
    if (apply) onApplyPauses(request);
    else onRun(request);
    setPrompt('');
  }

  return (
    <aside className="ai-panel cleanup-panel assistant-panel" aria-label="Timeline assistant">
      <div className="panel-heading">
        <h2><Icon name="sparkles" size={16} /> Timeline assistant</h2>
        <button className="icon-button" aria-label="Close assistant" onClick={onClose}><Icon name="close" size={15} /></button>
      </div>
      <div className="cleanup-body">
        <div className="assistant-controls">
          <p className="provider-note">Draft cleanup for the entire sequence: false starts, repeated words and takes, and quiet gaps. Preview and apply the cuts you choose.</p>
          <p className="provider-note">
            {ready && active && profile
              ? <>Whisper runs locally. Your brief, transcript, and timing go to {providers[active]} · {profile.model}{profile.baseUrl ? ' · ' + profile.baseUrl : ''}. Transcript questions and Advanced analysis also send recent conversation.</>
              : providerError || providerStatus?.warning || 'Set up an AI provider to use the timeline assistant.'}
            {' '}<button className="text-link" type="button" onClick={() => onSettings('providers')}>AI providers</button>
            {' · '}<button className="text-link" type="button" onClick={() => onSettings('speech')}>Transcription</button>
          </p>
          {!hasAudio && <p className="provider-note">Add footage with audio to your timeline to start.</p>}
          <form className="assistant-controls" onSubmit={(event) => {
            event.preventDefault();
            submit('chat', '');
          }}>
            <label className="sr-only" htmlFor="timeline-assistant-prompt">Editing direction or transcript question</label>
            <textarea
              id="timeline-assistant-prompt"
              value={prompt}
              onChange={(event) => setPrompt(event.target.value)}
              placeholder="Give editing direction or ask about the transcript"
              rows={2}
              maxLength={1500}
              disabled={!!progress}
            />
            <div className="assistant-actions">
              <button type="button" className="button primary" disabled={!canRun} onClick={() => submit('draft', 'Remove clear false starts, repeated words or passages, recording count-ins, and dispensable quiet gaps. Keep the clearest complete take. Preserve unique details, intentional pauses, and time used for demonstrations.')}>
                Draft cleanup
              </button>
              <button type="submit" className="button secondary" disabled={!canRun || !prompt.trim()}>Ask about transcript</button>
            </div>
            <details className="assistant-advanced">
              <summary>Advanced</summary>
              <div className="assistant-actions">
                {!reviewingPauses && !needsCuts && (
                  <button type="button" className="button secondary" disabled={!canRun} onClick={() => submit('pauses', 'Analyze the transcript and suggest quiet gaps to shorten.')}>
                    {report && !report.draftSummary ? 'Review pauses again' : 'Analyze transcript and pauses'}
                  </button>
                )}
                {needsCuts && (
                  <button type="button" className="button secondary" disabled={!canRun} onClick={() => submit('cuts', 'Suggest cuts for the reviewed timeline.')}>
                    Find cuts
                  </button>
                )}
                {reviewingPauses && (
                  <button type="button" className="button secondary" disabled={!canRun} onClick={() => submit('cuts', 'Keep the pauses as they are and suggest cuts for the reviewed timeline.')}>
                    Find cuts without removing pauses
                  </button>
                )}
              </div>
            </details>
          </form>
        </div>
        {progress && (
          <div className="cleanup-progress">
            <p role="status">{progress.stage}</p>
            <progress aria-label="Assistant progress" max={1} value={progress.fraction ?? undefined} />
            <button className="button secondary" onClick={onCancel}>Cancel analysis</button>
          </div>
        )}
        {error && <p className="cleanup-warning" role="alert">{error}</p>}
        {!progress && report && (
          <>
            {report.draftSummary && <p className="assistant-draft-summary">{report.draftSummary}</p>}
            <CutReview
              report={report}
              {...review}
              assistant
              reviewingPauses={reviewingPauses}
            />
            <CutReviewActions
              {...review}
              reviewingPauses={reviewingPauses}
              canApply={!reviewingPauses || canRun}
              onApply={reviewingPauses
                ? () => submit('cuts', 'I applied the selected pause removals. Suggest cuts for the edited timeline.', true)
                : review.onApply}
            />
          </>
        )}
        {historyError && <p className="cleanup-warning" role="alert">{historyError}</p>}
        {conversation.length > 0 && (
          <section className="assistant-conversation" aria-label="Project conversation">
            <h3>Project conversation</h3>
            <div className="assistant-history" ref={chat}>
              {conversation.map((message) => (
                <div className={`chat-message ${message.role}`} key={message.id}>
                  <div className="message-role">{message.role === 'assistant' ? 'Cutline' : 'You'}</div>
                  <p>{message.text}</p>
                </div>
              ))}
            </div>
          </section>
        )}
      </div>
    </aside>
  );
}
