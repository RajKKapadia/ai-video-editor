import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Icon } from './Icon';
import { timecode } from './types';
import { errorMessage, providers, type CleanupProgress, type ProviderSelection, type ProviderStatus } from './cleanupNative';
import type { CleanupReport, TimeRange } from './cleanupTimeline';
import { whisperModels, type SpeechStatus } from './speechNative';

type Props = {
  providerRevision: number;
  progress: CleanupProgress | null;
  error: string;
  report: CleanupReport | null;
  checked: ReadonlySet<number>;
  saving: number;
  invalidSelection: boolean;
  hasAudio: boolean;
  previewing: boolean;
  onClose: () => void;
  onSettings: (tab?: 'speech' | 'providers') => void;
  onAnalyze: (options: { pauses: boolean; retakes: ProviderSelection | null }) => void;
  onCancel: () => void;
  onToggle: (index: number) => void;
  onSelectPauses: () => void;
  onAudition: (range: TimeRange) => void;
  onPreview: (range?: TimeRange) => void;
  onApply: () => void;
};

export function CleanupPanel({
  providerRevision, progress, error, report, checked, saving, invalidSelection,
  hasAudio, previewing, onClose, onSettings, onAnalyze, onCancel,
  onToggle, onSelectPauses, onAudition, onPreview, onApply,
}: Props) {
  const [pauses, setPauses] = useState(true);
  const [retakes, setRetakes] = useState(false);
  const [providerStatus, setProviderStatus] = useState<ProviderStatus | null>(null);
  const [settingsError, setSettingsError] = useState('');
  const [speech, setSpeech] = useState<SpeechStatus | null>(null);
  const active = providerStatus?.active;
  const profile = active ? providerStatus?.profiles[active] : undefined;

  useEffect(() => {
    let current = true;
    void invoke<SpeechStatus>('speech_status').then((saved) => {
      if (current) setSpeech(saved);
    }).catch(() => {});
    void invoke<ProviderStatus>('ai_provider_status').then((status) => {
      if (current) {
        setProviderStatus(status);
        setSettingsError('');
      }
    }).catch((error) => {
      if (current) setSettingsError(errorMessage(error, 'Could not load AI provider settings.'));
    });
    return () => { current = false; };
  }, [providerRevision]);

  return (
    <aside className="ai-panel cleanup-panel" aria-label="Clean up timeline">
      <div className="panel-heading">
        <h2><Icon name="scissors" size={16} /> Clean up timeline</h2>
        <button className="icon-button" aria-label="Close timeline cleanup" onClick={onClose}><Icon name="close" size={15} /></button>
      </div>
      <div className="cleanup-body">
        {progress ? (
          <div className="cleanup-progress">
            <p role="status">{progress.stage}</p>
            <progress aria-label="Timeline cleanup progress" max={1} value={progress.fraction ?? undefined} />
            <button className="button secondary" onClick={onCancel}>Cancel analysis</button>
          </div>
        ) : (
          <>
            <fieldset className="cleanup-options">
              <legend>Analyze the entire sequence</legend>
              <label><input type="checkbox" checked={pauses} onChange={(event) => setPauses(event.target.checked)} /> Shorten pauses</label>
              <label><input type="checkbox" checked={retakes} onChange={(event) => setRetakes(event.target.checked)} /> Remove repeated takes</label>
              {retakes && speech && <p className="provider-note">{whisperModels[speech.selection.model]} · {speech.selection.device?.name ?? 'CPU'} · <button className="text-link" type="button" onClick={() => onSettings('speech')}>Transcription settings</button></p>}
              {retakes ? (
                <p className="provider-note">
                  {active && profile?.ready
                    ? <>Transcript text will go to {providers[active]} · {profile.model}{profile.baseUrl ? ' · ' + profile.baseUrl : ''}.</>
                    : settingsError || providerStatus?.warning || 'Set up an AI provider to compare repeated takes.'}
                  {' '}<button className="text-link" type="button" onClick={() => onSettings('providers')}>AI providers</button>
                </p>
              ) : (
                <p className="provider-note">Speech analysis runs on this device. First use downloads the required speech models.</p>
              )}
              {retakes && active && profile?.ready && (
                <p className="provider-note">First use downloads the local speech models. Comparing takes may use your provider quota.</p>
              )}
              {!hasAudio && <p className="provider-note">Add footage with audio to your timeline to start.</p>}
              <button className="button secondary" disabled={!hasAudio || (!pauses && !retakes) || (retakes && !profile?.ready)} onClick={() => {
                if (retakes && (!active || !profile?.ready)) return;
                onAnalyze({
                  pauses,
                  retakes: retakes && active && profile
                    ? { provider: active, config: { model: profile.model, baseUrl: profile.baseUrl } }
                    : null,
                });
              }}>{report ? 'Analyze again' : 'Analyze timeline'}</button>
            </fieldset>
            {error && <p className="cleanup-warning" role="alert">{error}</p>}
            {report?.retakeIssue && <p className="cleanup-warning" role="status">{report.retakeIssue}</p>}
            {report?.transcriptCacheIssue && <p className="cleanup-warning" role="status">{report.transcriptCacheIssue}</p>}
            {report?.runtimeNotice && <p className="cleanup-warning" role="status">{report.runtimeNotice}</p>}
            {report?.speech && <p className="provider-note">Transcribed with {whisperModels[report.speech.model]} · {report.speech.device?.name ?? 'CPU'}</p>}
            {!!report?.failedSources && (
              <p className="cleanup-warning" role="status">
                {report.failedSources} {report.failedSources === 1 ? 'source could' : 'sources could'} not be analyzed. Review the available results.
              </p>
            )}
            {report && (
              <section aria-label="Suggested cuts">
                <div className="cleanup-results-heading">
                  <h3>{report.proposals.length ? report.proposals.length + ' suggested cuts' : 'No cuts suggested'}</h3>
                  {report.proposals.some((proposal) => proposal.kind === 'pause') && (
                    <button type="button" className="text-link" onClick={onSelectPauses}>Select all pauses</button>
                  )}
                </div>
                <ul className="cleanup-results">
                  {report.proposals.map((proposal, index) => (
                    <li key={index}>
                      <label>
                        <input type="checkbox" checked={checked.has(index)} onChange={() => onToggle(index)} />
                        <span>{proposal.kind === 'pause' ? 'Quiet pause' : 'Repeated take'}</span>
                      </label>
                      <p className="cleanup-range">
                        {timecode(proposal.remove.start)} – {timecode(proposal.remove.end)} · {(proposal.remove.end - proposal.remove.start).toFixed(2)} s
                      </p>
                      {proposal.keep && (
                        <div className="cleanup-excerpts">
                          <p><strong>Remove</strong> {proposal.removedText}</p>
                          <p><strong>Keep</strong> {proposal.keptText}</p>
                          <div>
                            <button className="text-link" onClick={() => onAudition(proposal.remove)}>Play removed take</button>
                            <button className="text-link" onClick={() => proposal.keep && onAudition(proposal.keep)}>Play kept take</button>
                          </div>
                        </div>
                      )}
                      <button className="text-link" disabled={!checked.has(index) || invalidSelection} onClick={() => onPreview(proposal.remove)}>Preview join</button>
                    </li>
                  ))}
                </ul>
              </section>
            )}
          </>
        )}
      </div>
      {report && checked.size > 0 && (
        <div className="cleanup-footer">
          {invalidSelection ? (
            <p className="cleanup-warning" role="status">This selection would leave an empty sequence or a fragment shorter than one frame. Deselect a cut to continue.</p>
          ) : (
            <p role="status">{checked.size} selected · {saving.toFixed(2)} s saved</p>
          )}
          <div>
            <button className="button secondary" disabled={invalidSelection} onClick={() => onPreview()}>
              {previewing ? 'Show original' : 'Preview selected'}
            </button>
            <button className="button primary" disabled={invalidSelection} onClick={onApply}>Apply selected</button>
          </div>
        </div>
      )}
    </aside>
  );
}
