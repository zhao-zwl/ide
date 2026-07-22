import { useEffect, useState } from 'react';
import { api, events } from './api/invoke';
import type { BootstrapState } from './types/models';
import BootstrapPage from './pages/BootstrapPage';
import ChatPage from './pages/ChatPage';
import QuestPage from './pages/QuestPage';
import CraftPage from './pages/CraftPage';
import CollabPage from './pages/CollabPage';
import ConsolePage from './pages/ConsolePage';
import SettingsPage from './pages/SettingsPage';

type Tab = 'bootstrap' | 'chat' | 'quest' | 'craft' | 'collab' | 'console' | 'settings';

const TABS: { id: Tab; label: string }[] = [
  { id: 'bootstrap', label: '启动' },
  { id: 'chat', label: '对话' },
  { id: 'quest', label: '自治任务' },
  { id: 'craft', label: '编辑' },
  { id: 'collab', label: '协作' },
  { id: 'console', label: '控制台' },
  { id: 'settings', label: '设置' },
];

export default function App() {
  const [tab, setTab] = useState<Tab>('bootstrap');
  const [boot, setBoot] = useState<BootstrapState>({
    phase: 'idle',
    progress: 0,
    detail: null,
  });

  useEffect(() => {
    api.bootstrapStatus().then(setBoot).catch(() => undefined);
    const p = events.onBootstrap(setBoot);
    return () => {
      p.then((u) => u()).catch(() => undefined);
    };
  }, []);

  const ready = boot.phase === 'ready';
  const phaseLabel: Record<string, string> = {
    idle: '未启动',
    pg: 'PostgreSQL',
    ollama: 'Ollama',
    model: '端模型',
    serve: 'aidea serve',
    ready: '就绪',
    error: '错误',
  };

  return (
    <div className="flex h-full">
      {/* 侧边导航 */}
      <aside className="flex w-44 flex-col border-r border-gray-200 bg-white">
        <div className="px-4 py-4 text-lg font-semibold text-brand">aidea</div>
        <nav className="flex-1">
          {TABS.map((t) => (
            <button
              key={t.id}
              onClick={() => setTab(t.id)}
              className={`block w-full px-4 py-2 text-left text-sm ${
                tab === t.id
                  ? 'bg-blue-50 font-medium text-brand'
                  : 'text-gray-600 hover:bg-gray-50'
              }`}
            >
              {t.label}
            </button>
          ))}
        </nav>
        <div className="border-t border-gray-200 px-4 py-2 text-xs text-gray-400">
          后端：{phaseLabel[boot.phase] ?? boot.phase}
        </div>
      </aside>

      {/* 主区域 */}
      <main className="flex-1 overflow-auto p-6">
        {!ready && tab !== 'bootstrap' && tab !== 'settings' && (
          <div className="mb-4 rounded-md bg-amber-50 px-3 py-2 text-sm text-amber-700">
            后端栈尚未就绪，请先在「启动」页等待 bootstrap 完成。
          </div>
        )}
        {tab === 'bootstrap' && <BootstrapPage boot={boot} onJump={setTab} />}
        {tab === 'chat' && <ChatPage />}
        {tab === 'quest' && <QuestPage />}
        {tab === 'craft' && <CraftPage />}
        {tab === 'collab' && <CollabPage />}
        {tab === 'console' && <ConsolePage />}
        {tab === 'settings' && <SettingsPage />}
      </main>
    </div>
  );
}
