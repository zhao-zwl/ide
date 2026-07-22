// 前后端共享类型（字段命名与 Rust 侧 serde 输出一致，均为 snake_case）。
// 前端只消费 Rust 命令返回值 / 事件载荷，不持有任何密钥。

export type BootstrapPhase =
  | 'idle'
  | 'pg'
  | 'ollama'
  | 'model'
  | 'serve'
  | 'ready'
  | 'error';

export interface BootstrapState {
  phase: BootstrapPhase;
  progress: number; // 0.0 ~ 1.0
  detail: string | null;
}

export type ConnStatus = 'booting' | 'connected' | 'error';

export type VendorKind = 'local' | 'online';

export interface VendorConfig {
  kind: VendorKind;
  base_url: string | null;
  local_model: string;
  model: string | null;
}

export interface TestVendorRequest {
  base_url: string;
  api_key: string | null;
}

export interface ChatSendRequest {
  session_id: string;
  message: string;
  attachments: string[];
}

export interface ChatStopRequest {
  session_id: string;
}

export interface ChatTokenEvent {
  session_id: string;
  delta: string;
}

export interface ChatDoneEvent {
  session_id: string;
}

export interface ChatErrorEvent {
  session_id: string;
  message: string;
}

export interface AgentRunRequest {
  session_id: string;
  goal: string;
  project_id: string | null;
}

export interface AgentEventDto {
  kind: string;
  payload: string;
  ts_ms: number;
}

export interface QuestRunRequest {
  goal: string;
  auto_commit?: boolean;
}

export type SubTaskStatus =
  | 'pending'
  | 'running'
  | 'success'
  | 'failed'
  | 'skipped';

export interface SubTaskDto {
  id: string;
  description: string;
  status: SubTaskStatus;
}

export interface PendingApprovalDto {
  id: string;
  tool: string;
  argument: string;
  subtask_id: string;
}

export interface QuestReportDto {
  goal: string;
  subtasks: SubTaskDto[];
  successes: number;
  failures: number;
  pending_approvals: PendingApprovalDto[];
}

export type CraftStateDto =
  | 'suggestion'
  | 'pending_confirm'
  | 'applied'
  | 'rejected';

export interface CraftProposeRequest {
  document_uri: string;
  old_text: string;
  new_text: string;
  rationale: string;
  kind: string; // "FileEdit" | "RunCommand" | "Commit"
}

export interface CraftProposalDto {
  id: string;
  document_uri: string;
  old_text: string;
  new_text: string;
  rationale: string;
  kind: string;
  state: CraftStateDto;
}

export interface CraftActionRequest {
  id: string;
  document_uri: string;
  old_text: string;
  new_text: string;
  rationale: string;
  kind: string;
}

export interface CommentAddRequest {
  file: string;
  line: number;
  body: string;
}

export interface CommentDto {
  id: string;
  tenant_id: string;
  file: string;
  line_start: number;
  line_end: number;
  author: string;
  body: string;
  resolved: boolean;
  created_at: number;
}

export interface LockDto {
  tenant_id: string;
  file: string;
  owner: string;
  acquired_at: number;
}

export interface SecretSetRequest {
  name: string;
  value: string;
}

export interface SecretGetRequest {
  name: string;
}

export interface ConsoleMetricsDto {
  requests: number;
  tool_calls: number;
  llm_calls: number;
  completions: number;
  denials: number;
  request_p95_ms: number;
}

export interface ConsoleStatusDto {
  tenant_id: string;
  user_id: string;
  perm_mask: number;
  permissions: string;
  audit_events: number;
  metrics: ConsoleMetricsDto;
}

export interface HealthOverviewDto {
  healthz: string;
  console: ConsoleStatusDto;
}
