import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { errorMessage, providers, type Provider, type ProviderStatus } from './cleanupNative';

export function ProviderSettings() {
  const [status, setStatus] = useState<ProviderStatus | null>(null);
  const [provider, setProvider] = useState<Provider>('openAi');
  const [model, setModel] = useState('');
  const [baseUrl, setBaseUrl] = useState('');
  const [contextTokens, setContextTokens] = useState(32768);
  const [key, setKey] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const profile = status?.profiles[provider];
  const changed = model.trim() !== profile?.model ||
    (provider === 'compatible' && baseUrl.trim() !== profile?.baseUrl) ||
    contextTokens !== (profile?.contextWindowTokens ?? 32768) || !!key.trim();

  function select(next: Provider, saved: ProviderStatus) {
    setProvider(next);
    setModel(saved.profiles[next]?.model ?? '');
    setBaseUrl(saved.profiles[next]?.baseUrl ?? '');
    setContextTokens(saved.profiles[next]?.contextWindowTokens ?? 32768);
    setKey('');
    setError('');
    setNotice('');
  }

  useEffect(() => {
    let current = true;
    void invoke<ProviderStatus>('ai_provider_status').then((saved) => {
      if (!current) return;
      setStatus(saved);
      select(saved.active ?? 'openAi', saved);
    }).catch((error) => {
      if (current) setError(errorMessage(error, 'Could not load AI provider settings.'));
    });
    return () => { current = false; };
  }, []);

  async function run(action: () => Promise<void>) {
    setBusy(true);
    setError('');
    setNotice('');
    try {
      await action();
    } catch (error) {
      setError(errorMessage(error, 'The provider operation failed.'));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form
      className="provider-form"
      onSubmit={(event) => {
        event.preventDefault();
        void run(async () => {
          const saved = await invoke<ProviderStatus>('ai_provider_save', {
            provider,
            config: { model, baseUrl: provider === 'compatible' ? baseUrl : null, contextWindowTokens: contextTokens },
            key: key.trim() || null,
          });
          setStatus(saved);
          select(provider, saved);
          setNotice('Provider saved and active.');
        });
      }}
    >
      <p className="modal-intro">
        Cutline sends transcript text to the active provider when you analyze retakes. OpenAI transcription also uses the saved OpenAI key when selected in Transcription settings.
        Your API key is stored in your system credential store.
      </p>
      <fieldset className="provider-fields" disabled={!status || busy}>
        <label className="field">
          Provider
          <select autoFocus value={provider} onChange={(event) => status && select(event.target.value as Provider, status)}>
            {Object.entries(providers).map(([id, label]) => (
              <option key={id} value={id}>{label}{status?.active === id ? ' · active' : ''}</option>
            ))}
          </select>
        </label>
        {provider === 'compatible' && (
          <label className="field">
            API base URL
            <input
              value={baseUrl}
              onChange={(event) => setBaseUrl(event.target.value)}
              placeholder="https://api.example.com/v1"
              type="url"
              required
            />
          </label>
        )}
        <label className="field">
          Model ID
          <input value={model} onChange={(event) => setModel(event.target.value)} required maxLength={200} spellCheck={false} />
        </label>
        <label className="field">
          Planning context budget (tokens)
          <input type="number" min={8192} max={2000000} step={1} value={contextTokens}
            onChange={(event) => setContextTokens(Number(event.target.value))} required />
        </label>
        <p className="provider-note">Use a budget within your model’s documented context window. Cutline estimates the full request size, reserves room for the response, and splits only when needed. This value is not detected automatically.</p>
        <label className="field">
          API key
          <input
            type="password"
            autoComplete="new-password"
            value={key}
            onChange={(event) => setKey(event.target.value)}
            placeholder={profile?.hasKey ? 'Saved key · enter a new key to replace it' : 'Paste API key'}
            maxLength={4096}
          />
        </label>
        {provider === 'compatible' && <p className="provider-note">Localhost endpoints can use an empty key. Re-enter the key if you change the base URL.</p>}
        <div className="provider-actions">
          <button className="button secondary" type="button" disabled={!profile?.hasKey || changed} onClick={() => {
            void run(async () => {
              setStatus(await invoke<ProviderStatus>('ai_provider_remove_key', { provider }));
              setNotice('API key removed.');
            });
          }}>Remove key</button>
          <button className="button secondary" type="button" disabled={!profile?.ready || changed} title="Sends synthetic text to the saved model" onClick={() => {
            void run(async () => {
              await invoke('ai_provider_test', { provider, config: { model: profile?.model, baseUrl: profile?.baseUrl } });
              setNotice('The model returned the expected response.');
            });
          }}>Test connection</button>
          <button className="button primary" type="submit" disabled={!model.trim() || (!changed && status?.active === provider)}>
            Save and use
          </button>
        </div>
      </fieldset>
      {status?.warning && !error && <p className="cleanup-warning" role="status">{status.warning}</p>}
      {busy && <p className="provider-note" role="status">Working…</p>}
      {notice && <p className="provider-note" role="status">{notice}</p>}
      {error && <p className="cleanup-warning" role="alert">{error}</p>}
      {!status && !error && <p className="provider-note" role="status">Loading settings…</p>}
    </form>
  );
}
