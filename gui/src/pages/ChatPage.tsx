import { useEffect, useRef, useState } from 'react';
import { api, events } from '../api/invoke';

interface Msg {
  role: 'user' | 'assistant' | 'system' | 'error';
  text: string;
}

export default function ChatPage() {
  const [session, setSession] = useState('sess-1');
  const [input, setInput] = useState('');
  const [msgs, setMsgs] = useState<Msg[]>([]);
  const [busy, setBusy] = useState(false);
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const unsubs = [
      events.onChatToken((e) => {
        if (e.session_id !== session) return;
        setMsgs((prev) => {
          const next = [...prev];
          const last = next[next.length - 1];
          if (last && last.role === 'assistant') {
            next[next.length - 1] = { ...last, text: last.text + e.delta };
          } else {
            next.push({ role: 'assistant', text: e.delta });
          }
          return next;
        });
      }),
      events.onChatDone((e) => {
        if (e.session_id === session) setBusy(false);
      }),
      events.onChatError((e) => {
        if (e.session_id !== session) return;
        setMsgs((prev) => [...prev, { role: 'error', text: e.message }]);
        setBusy(false);
      }),
    ];
    return () => {
      unsubs.forEach((p) => p.then((u) => u()).catch(() => undefined));
    };
  }, [session]);

  useEffect(() => {
    listRef.current?.scrollTo({ top: listRef.current.scrollHeight });
  }, [msgs]);

  const send = async () => {
    const text = input.trim();
    if (!text || busy) return;
    setMsgs((prev) => [...prev, { role: 'user', text }]);
    setInput('');
    setBusy(true);
    try {
      await api.chatSend(session, text, []);
    } catch (e) {
      setMsgs((prev) => [...prev, { role: 'error', text: String(e) }]);
      setBusy(false);
    }
  };

  return (
    <div className="mx-auto flex h-full max-w-3xl flex-col">
      <div className="mb-3 flex items-center gap-2">
        <span className="text-sm text-gray-500">会话</span>
        <input
          className="input max-w-[200px]"
          value={session}
          onChange={(e) => setSession(e.target.value)}
        />
      </div>

      <div ref={listRef} className="flex-1 space-y-3 overflow-auto pr-2">
        {msgs.length === 0 && (
          <p className="text-sm text-gray-400">开始和 nes-tab 对话吧。</p>
        )}
        {msgs.map((m, i) => (
          <div
            key={i}
            className={`rounded-lg px-3 py-2 text-sm ${
              m.role === 'user'
                ? 'ml-auto max-w-[80%] bg-brand text-white'
                : m.role === 'error'
                  ? 'border border-red-200 bg-red-50 text-red-600'
                  : m.role === 'system'
                    ? 'bg-gray-100 text-gray-600'
                    : 'mr-auto max-w-[80%] bg-white text-gray-800 shadow-sm'
            }`}
          >
            <span className="whitespace-pre-wrap">{m.text}</span>
          </div>
        ))}
      </div>

      <div className="mt-3 flex gap-2">
        <input
          className="input flex-1"
          placeholder="输入消息，回车发送"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') send();
          }}
        />
        {busy ? (
          <button className="btn-ghost" onClick={() => api.chatStop(session)}>
            停止
          </button>
        ) : (
          <button className="btn" onClick={send}>
            发送
          </button>
        )}
      </div>
    </div>
  );
}
