import { useEffect, useState } from 'react';
import { api } from '../api/invoke';
import type { HealthOverviewDto } from '../types/models';

export default function ConsolePage() {
  const [data, setData] = useState<HealthOverviewDto | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const load = async () => {
    try {
      setData(await api.healthOverview());
      setErr(null);
    } catch (e) {
      setErr(String(e));
    }
  };

  useEffect(() => {
    load();
  }, []);

  return (
    <div className="mx-auto max-w-3xl">
      <div className="mb-3 flex items-center justify-between">
        <h1 className="text-xl font-semibold">控制台（9090）</h1>
        <button className="btn-ghost" onClick={load}>
          刷新
        </button>
      </div>
      {err && <p className="text-sm text-red-600">{err}</p>}
      {data && (
        <div className="space-y-4">
          <div className="card">
            <div className="label">liveness（/healthz）</div>
            <div className="text-sm">{data.healthz || '(不可达)'}</div>
          </div>
          <div className="card">
            <div className="label">租户 / 用户</div>
            <div className="text-sm">
              {data.console.tenant_id} / {data.console.user_id}
            </div>
            <div className="mt-2 text-sm">
              权限掩码：{data.console.perm_mask}（{data.console.permissions}）
            </div>
            <div className="mt-2 text-sm text-gray-500">
              审计事件：{data.console.audit_events}
            </div>
          </div>
          <div className="card">
            <div className="label">指标</div>
            <div className="grid grid-cols-2 gap-2 text-sm text-gray-600">
              <div>请求：{data.console.metrics.requests}</div>
              <div>工具调用：{data.console.metrics.tool_calls}</div>
              <div>LLM 调用：{data.console.metrics.llm_calls}</div>
              <div>补全：{data.console.metrics.completions}</div>
              <div>拒绝：{data.console.metrics.denials}</div>
              <div>p95：{data.console.metrics.request_p95_ms} ms</div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
