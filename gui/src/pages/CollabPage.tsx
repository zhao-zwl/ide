import { useState } from 'react';
import { api } from '../api/invoke';
import type { CommentDto, LockDto } from '../types/models';

export default function CollabPage() {
  const [file, setFile] = useState('');
  const [line, setLine] = useState(1);
  const [body, setBody] = useState('');
  const [comments, setComments] = useState<CommentDto[]>([]);
  const [lock, setLock] = useState<LockDto | null>(null);
  const [secretName, setSecretName] = useState('online_api_key');
  const [secretVal, setSecretVal] = useState('');
  const [msg, setMsg] = useState<string | null>(null);

  const flash = (m: string) => {
    setMsg(m);
    setTimeout(() => setMsg(null), 2500);
  };

  const addComment = async () => {
    if (!file.trim() || !body.trim()) return;
    const c = await api.commentAdd({ file, line, body });
    setComments((prev) => [...prev, c]);
    setBody('');
    flash('评论已添加');
  };

  const listComments = async () => {
    if (!file.trim()) return;
    setComments(await api.commentList(file));
  };

  const acquireLock = async () => {
    if (!file.trim()) return;
    setLock(await api.lockAcquire(file));
  };

  const releaseLock = async () => {
    if (!file.trim()) return;
    await api.lockRelease(file);
    setLock(null);
    flash('锁已释放');
  };

  const setSecret = async () => {
    await api.secretSet({ name: secretName, value: secretVal });
    setSecretVal('');
    flash(`密钥 ${secretName} 已保存（keyring，不在前端持久化）`);
  };

  const getSecret = async () => {
    const v = await api.secretGet({ name: secretName });
    flash(v ? `读取到 ${secretName}（长度 ${v.length}）` : `${secretName} 未配置`);
  };

  return (
    <div className="mx-auto max-w-3xl space-y-4">
      <h1 className="text-xl font-semibold">协作</h1>
      {msg && (
        <div className="rounded-md bg-green-50 px-3 py-2 text-sm text-green-700">
          {msg}
        </div>
      )}

      <div className="card space-y-3">
        <label className="label">文件</label>
        <input className="input" value={file} onChange={(e) => setFile(e.target.value)} />
        <div className="flex gap-2">
          <button className="btn-ghost" onClick={listComments}>
            列出评论
          </button>
          <button className="btn-ghost" onClick={acquireLock}>
            获取编辑锁
          </button>
          <button className="btn-ghost" onClick={releaseLock}>
            释放编辑锁
          </button>
        </div>
        {lock && (
          <p className="text-xs text-gray-500">
            已持有锁：{lock.owner} @ {lock.file}
          </p>
        )}
      </div>

      <div className="card space-y-3">
        <h2 className="text-sm font-medium">新增评论</h2>
        <div className="flex gap-2">
          <input
            type="number"
            className="input max-w-[90px]"
            value={line}
            onChange={(e) => setLine(Number(e.target.value))}
          />
          <input
            className="input"
            placeholder="评论内容"
            value={body}
            onChange={(e) => setBody(e.target.value)}
          />
          <button className="btn" onClick={addComment}>
            添加
          </button>
        </div>
        {comments.length > 0 && (
          <ul className="space-y-2">
            {comments.map((c) => (
              <li
                key={c.id}
                className="rounded border border-gray-200 px-3 py-2 text-sm"
              >
                <div className="text-gray-500">
                  L{c.line_start} · {c.author}
                  {c.resolved ? ' · 已解决' : ''}
                </div>
                <div>{c.body}</div>
              </li>
            ))}
          </ul>
        )}
      </div>

      <div className="card space-y-3">
        <h2 className="text-sm font-medium">密钥（在线厂商 Key 等）</h2>
        <input
          className="input"
          value={secretName}
          onChange={(e) => setSecretName(e.target.value)}
          placeholder="密钥名"
        />
        <input
          className="input"
          type="password"
          value={secretVal}
          onChange={(e) => setSecretVal(e.target.value)}
          placeholder="密钥值（存 macOS Keychain）"
        />
        <div className="flex gap-2">
          <button className="btn" onClick={setSecret}>
            保存密钥
          </button>
          <button className="btn-ghost" onClick={getSecret}>
            读取密钥
          </button>
        </div>
      </div>
    </div>
  );
}
