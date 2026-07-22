import { api } from '../api/invoke';
import type { BootstrapState } from '../types/models';

const PHASE_ORDER: BootstrapState['phase'][] = [
  'idle',
  'pg',
  'ollama',
  'model',
  'serve',
  'ready',
];

export default function BootstrapPage({
  boot,
  onJump,
}: {
  boot: BootstrapState;
  onJump: (t: 'chat' | 'settings') => void;
}) {
  const idx = Math.max(0, PHASE_ORDER.indexOf(boot.phase));
  const pct = Math.round(boot.progress * 100);
  const isError = boot.phase === 'error';

  return (
    <div className="mx-auto max-w-2xl">
      <h1 className="mb-1 text-xl font-semibold">后端栈启动</h1>
      <p className="mb-4 text-sm text-gray-500">
        一键拉起 PostgreSQL → Ollama → 端模型 nes-tab → aidea serve。
      </p>

      <div className="card">
        <div className="mb-3 flex items-center justify-between">
          <span className="text-sm text-gray-600">
            阶段：<b>{boot.phase}</b>
          </span>
          <span className="text-sm font-medium text-brand">{pct}%</span>
        </div>
        <div className="h-2 w-full overflow-hidden rounded bg-gray-100">
          <div
            className={`h-full transition-all ${isError ? 'bg-red-500' : 'bg-brand'}`}
            style={{ width: `${pct}%` }}
          />
        </div>
        {boot.detail && (
          <p className="mt-3 text-xs text-gray-500">{boot.detail}</p>
        )}
        {isError && (
          <button
            className="btn mt-4"
            onClick={() => api.bootstrapStatus().catch(() => undefined)}
          >
            重试检查
          </button>
        )}
      </div>

      {/* 阶段清单 */}
      <ol className="mt-4 space-y-2">
        {PHASE_ORDER.filter((p) => p !== 'idle' && p !== 'error').map((p, i) => {
          const done = isError ? i < idx : i < idx || boot.phase === 'ready';
          return (
            <li
              key={p}
              className="flex items-center gap-2 rounded-md border border-gray-200 bg-white px-3 py-2 text-sm"
            >
              <span
                className={`inline-block h-2 w-2 rounded-full ${
                  done ? 'bg-green-500' : i === idx && !isError ? 'bg-brand' : 'bg-gray-300'
                }`}
              />
              <span className="capitalize">{p}</span>
            </li>
          );
        })}
      </ol>

      {boot.phase === 'ready' && (
        <div className="mt-6 flex gap-3">
          <button className="btn" onClick={() => onJump('chat')}>
            去对话
          </button>
          <button className="btn-ghost" onClick={() => onJump('settings')}>
            模型设置
          </button>
        </div>
      )}
    </div>
  );
}
