import { useState } from 'react';
import { api } from '../api/invoke';
import type { CraftProposalDto } from '../types/models';

export default function CraftPage() {
  const [uri, setUri] = useState('');
  const [oldText, setOldText] = useState('');
  const [newText, setNewText] = useState('');
  const [rationale, setRationale] = useState('');
  const [kind, setKind] = useState('FileEdit');
  const [proposal, setProposal] = useState<CraftProposalDto | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const buildReq = () => ({
    document_uri: uri,
    old_text: oldText,
    new_text: newText,
    rationale: rationale,
    kind,
  });

  const propose = async () => {
    setBusy(true);
    setErr(null);
    try {
      setProposal(await api.craftPropose(buildReq()));
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const confirm = async () => {
    if (!proposal) return;
    setBusy(true);
    setErr(null);
    try {
      setProposal(
        await api.craftConfirm({
          id: proposal.id,
          document_uri: proposal.document_uri,
          old_text: proposal.old_text,
          new_text: proposal.new_text,
          rationale: proposal.rationale,
          kind: proposal.kind,
        }),
      );
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const reject = async () => {
    if (!proposal) return;
    setBusy(true);
    setErr(null);
    try {
      setProposal(
        await api.craftReject({
          id: proposal.id,
          document_uri: proposal.document_uri,
          old_text: proposal.old_text,
          new_text: proposal.new_text,
          rationale: proposal.rationale,
          kind: proposal.kind,
        }),
      );
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="mx-auto max-w-3xl">
      <h1 className="mb-1 text-xl font-semibold">编辑（Craft）</h1>
      <p className="mb-4 text-sm text-gray-500">
        提出编辑提案并人工确认后真实落盘（决策 #B：走 FsHost，confirm 即写磁盘）。
      </p>

      <div className="card space-y-3">
        <div>
          <label className="label">文档路径</label>
          <input
            className="input"
            value={uri}
            onChange={(e) => setUri(e.target.value)}
            placeholder="/abs/path/to/file.rs"
          />
        </div>
        <div className="grid grid-cols-2 gap-3">
          <div>
            <label className="label">旧文本</label>
            <textarea
              className="input h-24 resize-none font-mono text-xs"
              value={oldText}
              onChange={(e) => setOldText(e.target.value)}
            />
          </div>
          <div>
            <label className="label">新文本</label>
            <textarea
              className="input h-24 resize-none font-mono text-xs"
              value={newText}
              onChange={(e) => setNewText(e.target.value)}
            />
          </div>
        </div>
        <div>
          <label className="label">理由</label>
          <input
            className="input"
            value={rationale}
            onChange={(e) => setRationale(e.target.value)}
          />
        </div>
        <div>
          <label className="label">类型</label>
          <select
            className="input"
            value={kind}
            onChange={(e) => setKind(e.target.value)}
          >
            <option value="FileEdit">FileEdit</option>
            <option value="RunCommand">RunCommand</option>
            <option value="Commit">Commit</option>
          </select>
        </div>
        <button className="btn" disabled={busy} onClick={propose}>
          提出提案
        </button>
        {err && <p className="text-sm text-red-600">{err}</p>}
      </div>

      {proposal && (
        <div className="card mt-4">
          <div className="mb-2 flex items-center justify-between">
            <span className="text-sm font-medium">提案状态：{proposal.state}</span>
            <span className="text-xs text-gray-400">{proposal.id}</span>
          </div>
          {proposal.state !== 'applied' && proposal.state !== 'rejected' && (
            <div className="flex gap-2">
              <button className="btn" disabled={busy} onClick={confirm}>
                确认并应用
              </button>
              <button className="btn-ghost" disabled={busy} onClick={reject}>
                拒绝
              </button>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
