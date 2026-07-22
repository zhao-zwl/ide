import { useState } from 'react';
import { api } from '../api/invoke';
import type { QuestReportDto, SubTaskStatus } from '../types/models';

const STATUS_STYLE: Record<SubTaskStatus, string> = {
  pending: 'bg-gray-100 text-gray-600',
  running: 'bg-blue-100 text-blue-700',
  success: 'bg-green-100 text-green-700',
  failed: 'bg-red-100 text-red-700',
  skipped: 'bg-amber-100 text-amber-700',
};

export default function QuestPage() {
  const [goal, setGoal] = useState('');
  const [autoCommit, setAutoCommit] = useState(false);
  const [report, setReport] = useState<QuestReportDto | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const run = async () => {
    if (!goal.trim() || busy) return;
    setBusy(true);
    setErr(null);
    setReport(null);
    try {
      const r = await api.questRun(goal.trim(), autoCommit);
      setReport(r);
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="mx-auto max-w-3xl">
      <h1 className="mb-1 text-xl font-semibold">自治任务（Quest）</h1>
      <p className="mb-4 text-sm text-gray-500">
        给定高层目标，自动分解为子任务并逐个执行（决策 #A：真实 LLM 驱动）。
      </p>

      <div className="card">
        <label className="label">目标</label>
        <textarea
          className="input h-24 resize-none"
          value={goal}
          onChange={(e) => setGoal(e.target.value)}
          placeholder="例如：为 src/foo.rs 增加单元测试"
        />
        <label className="mt-3 flex items-center gap-2 text-sm text-gray-600">
          <input
            type="checkbox"
            checked={autoCommit}
            onChange={(e) => setAutoCommit(e.target.checked)}
          />
          自动执行（否则 Execute/Commit 类动作收集为待审批）
        </label>
        <button className="btn mt-3" disabled={busy} onClick={run}>
          {busy ? '运行中…' : '运行 Quest'}
        </button>
        {err && <p className="mt-2 text-sm text-red-600">{err}</p>}
      </div>

      {report && (
        <div className="card mt-4">
          <div className="mb-2 flex items-center gap-3 text-sm">
            <span className="text-green-700">成功 {report.successes}</span>
            <span className="text-red-700">失败 {report.failures}</span>
          </div>
          <ol className="space-y-2">
            {report.subtasks.map((s) => (
              <li
                key={s.id}
                className="flex items-center gap-2 rounded border border-gray-200 px-3 py-2 text-sm"
              >
                <span
                  className={`rounded px-2 py-0.5 text-xs ${STATUS_STYLE[s.status]}`}
                >
                  {s.status}
                </span>
                <span>{s.description}</span>
              </li>
            ))}
          </ol>
          {report.pending_approvals.length > 0 && (
            <div className="mt-3 rounded bg-amber-50 p-3 text-sm text-amber-700">
              <div className="mb-1 font-medium">待审批动作</div>
              {report.pending_approvals.map((p) => (
                <div key={p.id}>
                  {p.tool} {p.argument}（子任务 {p.subtask_id}）
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
