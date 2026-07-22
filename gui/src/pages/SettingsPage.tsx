import { useEffect, useState } from 'react';
import { api } from '../api/invoke';
import type { VendorConfig, VendorKind } from '../types/models';

export default function SettingsPage() {
  const [cfg, setCfg] = useState<VendorConfig>({
    kind: 'local',
    base_url: null,
    local_model: 'nes-tab:latest',
    model: null,
  });
  const [autoBootstrap, setAutoBootstrap] = useState(true);
  const [apiKey, setApiKey] = useState('');
  const [localModels, setLocalModels] = useState<string[]>([]);
  const [testResult, setTestResult] = useState<string | null>(null);
  const [msg, setMsg] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api
      .getVendorConfig()
      .then(setCfg)
      .catch(() => undefined);
    api
      .getAutoBootstrap()
      .then(setAutoBootstrap)
      .catch(() => undefined);
    api
      .modelListLocal()
      .then(setLocalModels)
      .catch(() => undefined);
  }, []);

  const flash = (m: string) => {
    setMsg(m);
    setTimeout(() => setMsg(null), 2500);
  };

  const save = async () => {
    setBusy(true);
    try {
      // 在线 Key 先落 keyring（secret_set），再切 vendor（serve 启动时会从 keyring 读取）。
      if (cfg.kind === 'online' && apiKey.trim()) {
        await api.secretSet({ name: 'online_api_key', value: apiKey.trim() });
      }
      await api.setVendorConfig(cfg);
      flash('已保存并重启 serve');
    } catch (e) {
      flash(`保存失败：${String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  const test = async () => {
    if (cfg.kind !== 'online' || !cfg.base_url) return;
    setTestResult('测试中…');
    try {
      const ok = await api.testVendor(cfg.base_url, apiKey.trim() || undefined);
      setTestResult(ok ? '连通 ✓' : '不可达 ✗');
    } catch (e) {
      setTestResult(`错误：${String(e)}`);
    }
  };

  return (
    <div className="mx-auto max-w-2xl">
      <h1 className="mb-1 text-xl font-semibold">设置</h1>
      <p className="mb-4 text-sm text-gray-500">选择模型后端（本地 Ollama / 在线 OpenAI 兼容）。</p>

      {msg && (
        <div className="mb-3 rounded-md bg-green-50 px-3 py-2 text-sm text-green-700">
          {msg}
        </div>
      )}

      <div className="card space-y-4">
        <div>
          <div className="label">模型后端</div>
          <div className="flex gap-2">
            {(['local', 'online'] as VendorKind[]).map((k) => (
              <button
                key={k}
                className={`btn-ghost ${cfg.kind === k ? 'border-brand text-brand' : ''}`}
                onClick={() =>
                  setCfg((c) => ({
                    ...c,
                    kind: k,
                    base_url: k === 'online' ? c.base_url ?? 'https://api.openai.com/v1' : null,
                  }))
                }
              >
                {k === 'local' ? '本地 Ollama' : '在线厂商'}
              </button>
            ))}
          </div>
        </div>

        {cfg.kind === 'local' ? (
          <div>
            <label className="label">本地模型名</label>
            <input
              className="input"
              value={cfg.local_model}
              onChange={(e) => setCfg((c) => ({ ...c, local_model: e.target.value }))}
            />
            {localModels.length > 0 && (
              <div className="mt-1 flex flex-wrap gap-1">
                {localModels.map((m) => (
                  <button
                    key={m}
                    className="rounded bg-gray-100 px-2 py-0.5 text-xs text-gray-600"
                    onClick={() => setCfg((c) => ({ ...c, local_model: m }))}
                  >
                    {m}
                  </button>
                ))}
              </div>
            )}
          </div>
        ) : (
          <>
            <div>
              <label className="label">Base URL（含 /v1）</label>
              <input
                className="input"
                value={cfg.base_url ?? ''}
                onChange={(e) =>
                  setCfg((c) => ({ ...c, base_url: e.target.value || null }))
                }
              />
            </div>
            <div>
              <label className="label">模型名</label>
              <input
                className="input"
                value={cfg.model ?? ''}
                onChange={(e) =>
                  setCfg((c) => ({ ...c, model: e.target.value || null }))
                }
                placeholder="gpt-4o-mini"
              />
            </div>
            <div>
              <label className="label">API Key（仅瞬时写入 keyring，前端不持久化）</label>
              <input
                className="input"
                type="password"
                value={apiKey}
                onChange={(e) => setApiKey(e.target.value)}
                placeholder="sk-..."
              />
            </div>
            <button className="btn-ghost" onClick={test} disabled={!cfg.base_url}>
              {testResult ?? '测试连通'}
            </button>
          </>
        )}

        <div className="border-t border-gray-100 pt-3">
          <label className="flex items-center gap-2 text-sm text-gray-600">
            <input
              type="checkbox"
              checked={autoBootstrap}
              onChange={(e) => {
                setAutoBootstrap(e.target.checked);
                api.setAutoBootstrap(e.target.checked).catch(() => undefined);
              }}
            />
            启动时自动拉起后端栈
          </label>
        </div>

        <button className="btn" disabled={busy} onClick={save}>
          保存并应用
        </button>
      </div>
    </div>
  );
}
