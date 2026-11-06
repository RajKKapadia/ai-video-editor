import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { errorMessage, type CleanupProgress } from './cleanupNative';
import { ProviderSettings } from './ProviderSettings';
import { gibibytes, whisperModels, type SpeechConfig, type SpeechStatus } from './speechNative';

export function Settings({ initialTab }: { initialTab: 'speech' | 'providers' }) {
  const [tab, setTab] = useState<'speech' | 'providers'>(initialTab);
  return <>
    <div className="settings-tabs" aria-label="Settings categories">
      <button className={`button ${tab === 'speech' ? 'primary' : 'secondary'}`} aria-pressed={tab === 'speech'} onClick={() => setTab('speech')}>Transcription</button>
      <button className={`button ${tab === 'providers' ? 'primary' : 'secondary'}`} aria-pressed={tab === 'providers'} onClick={() => setTab('providers')}>AI providers</button>
    </div>
    {tab === 'speech' ? <SpeechSettings /> : <ProviderSettings />}
  </>;
}

function SpeechSettings() {
  const [status, setStatus] = useState<SpeechStatus | null>(null);
  const [config, setConfig] = useState<SpeechConfig>({ preference: 'autoBalanced', processing: 'auto' });
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<CleanupProgress | null>(null);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const mounted = useRef(true);
  const preparing = useRef(false);
  const changed = status && (status.config.preference !== config.preference || status.config.processing !== config.processing);

  useEffect(() => {
    mounted.current = true;
    void invoke<SpeechStatus>('speech_status').then((saved) => {
      if (mounted.current) { setStatus(saved); setConfig(saved.config); }
    }).catch((error) => {
      if (mounted.current) setError(errorMessage(error, 'Could not inspect transcription settings.'));
    });
    return () => {
      mounted.current = false;
      if (preparing.current) void invoke('speech_cancel').catch(() => {});
    };
  }, []);

  async function run(action: () => Promise<SpeechStatus>, message: string) {
    setBusy(true); setError(''); setNotice('');
    try {
      const saved = await action();
      if (mounted.current) { setStatus(saved); setConfig(saved.config); setNotice(message); }
    } catch (error) {
      if (mounted.current) setError(errorMessage(error, 'Could not update transcription settings.'));
    } finally {
      preparing.current = false;
      if (mounted.current) { setBusy(false); setProgress(null); }
    }
  }

  async function prepare() {
    preparing.current = true;
    const unlisten = await listen<CleanupProgress>('speech-prepare-progress', ({ payload }) => {
      if (mounted.current) setProgress(payload);
    });
    try {
      if (!mounted.current) throw new Error('Preparation cancelled.');
      return await invoke<SpeechStatus>('speech_prepare', { retest: true });
    }
    finally { unlisten(); }
  }

  const selection = status?.selection;
  const model = status?.models.find((model) => model.id === selection?.model);
  return <form className="provider-form speech-settings" onSubmit={(event) => {
    event.preventDefault();
    void run(() => invoke<SpeechStatus>('speech_save', { config }), 'Transcription preferences saved.');
  }}>
    <p className="modal-intro">Transcribe on this device. Auto recommends a model for your hardware; choosing a model keeps it fixed when processing falls back to CPU.</p>
    <fieldset className="provider-fields" disabled={!status || busy}>
      <label className="field">Transcription model
        <select autoFocus value={config.preference} onChange={(event) => setConfig({ ...config, preference: event.target.value as SpeechConfig['preference'] })}>
          <option value="autoBalanced">Auto · balanced</option>
          <option value="autoAccuracy">Auto · prioritize accuracy</option>
          {Object.entries(whisperModels).map(([id, label]) => <option key={id} value={id}>{label}</option>)}
        </select>
      </label>
      <label className="field">Processing device
        <select value={config.processing} onChange={(event) => setConfig({ ...config, processing: event.target.value as SpeechConfig['processing'] })}>
          <option value="auto">Auto · use available acceleration</option>
          <option value="cpu">CPU</option>
        </select>
      </label>
    </fieldset>
    {status && selection && <section className="speech-summary" aria-label="Transcription recommendation">
      <strong>{whisperModels[selection.model]}</strong>
      <p>{selection.device?.name ?? 'CPU processing'} · {model ? gibibytes(model.downloadBytes) + ' model download' : ''}{model?.downloaded ? ' · downloaded' : ''}</p>
      <p>{selection.reason}</p>
      {selection.benchmark ? <p>Measured on 8 seconds of speech after warm-up: {(selection.benchmark.realtimeFactor * 8).toFixed(1)} s transcription · {selection.benchmark.loadSeconds.toFixed(1)} s model loading. Longer recordings and other languages may differ.</p>
        : <p>Download and test measures loading and transcription speed using a bundled speech sample.</p>}
      <dl className="speech-hardware">
        <dt>Processor</dt><dd>{status.hardware.cpu} · {status.hardware.cores} logical CPUs</dd>
        <dt>Memory</dt><dd>{gibibytes(status.hardware.availableMemory)} available / {gibibytes(status.hardware.totalMemory)} total</dd>
        <dt>Model storage</dt><dd>{status.hardware.freeDisk === null ? 'Available space unknown' : gibibytes(status.hardware.freeDisk) + ' available'}</dd>
      </dl>
      {status.hardware.warning && <p className="provider-note">{status.hardware.warning}</p>}
      {changed && <p className="provider-note">Save your preferences to update the recommendation.</p>}
    </section>}
    {progress && <div className="cleanup-progress"><p role="status">{progress.stage}</p><progress aria-label="Speech model preparation" max={1} value={progress.fraction ?? undefined} /></div>}
    <div className="provider-actions">
      {busy && preparing.current ? <button className="button secondary" type="button" onClick={() => void invoke('speech_cancel').catch(() => {})}>Cancel preparation</button> : <>
        <button className="button secondary" type="button" disabled={busy || !!changed} onClick={() => void run(() => invoke<SpeechStatus>('speech_status', { recheck: true }), 'Hardware checked. The next test will measure current performance.')}>Refresh hardware</button>
        <button className="button secondary" type="button" disabled={!status || busy || !!changed} onClick={() => void run(prepare, 'Model verified and transcription test completed.')}>Download and test</button>
        <button className="button primary" type="submit" disabled={!status || busy || !changed}>Save and use</button>
      </>}
    </div>
    {error && <p className="cleanup-warning" role="alert">{error}</p>}
    {notice && <p className="provider-note" role="status">{notice}</p>}
    {!status && !error && <p className="provider-note" role="status">Checking hardware…</p>}
  </form>;
}
