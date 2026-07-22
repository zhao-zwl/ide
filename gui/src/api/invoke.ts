// 前端 ⇄ Rust 命令 / 事件桥接层。
//
// 命令参数命名与 Rust 侧 `#[tauri::command]` 形参一一对应（单结构体形参用
// `{ req: {...} }`，裸形参用 `{ file }` 等）。事件用 `listen` 订阅，Rust 侧
// 通过 `app.emit(...)` 推送。
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  AgentEventDto,
  BootstrapState,
  ChatDoneEvent,
  ChatErrorEvent,
  ChatTokenEvent,
  CommentDto,
  CommentAddRequest,
  CraftActionRequest,
  CraftProposeRequest,
  CraftProposalDto,
  HealthOverviewDto,
  LockDto,
  QuestReportDto,
  SecretGetRequest,
  SecretSetRequest,
  VendorConfig,
} from '../types/models';

export const api = {
  bootstrapStatus: () => invoke<BootstrapState>('bootstrap_status'),
  getVendorConfig: () => invoke<VendorConfig>('get_vendor_config'),
  setVendorConfig: (config: VendorConfig) =>
    invoke('set_vendor_config', { config }),
  getAutoBootstrap: () => invoke<boolean>('get_auto_bootstrap'),
  setAutoBootstrap: (enabled: boolean) =>
    invoke('set_auto_bootstrap', { enabled }),
  testVendor: (baseUrl: string, apiKey?: string) =>
    invoke<boolean>('test_vendor', {
      req: { base_url: baseUrl, api_key: apiKey ?? null },
    }),

  chatSend: (sessionId: string, message: string, attachments: string[] = []) =>
    invoke('chat_send', {
      req: { session_id: sessionId, message, attachments },
    }),
  chatStop: (sessionId: string) =>
    invoke('chat_stop', { req: { session_id: sessionId } }),

  agentRun: (sessionId: string, goal: string, projectId?: string) =>
    invoke('agent_run', {
      req: { session_id: sessionId, goal, project_id: projectId ?? null },
    }),
  agentStop: (sessionId: string) =>
    invoke('agent_stop', { req: { session_id: sessionId } }),

  questRun: (goal: string, autoCommit?: boolean) =>
    invoke<QuestReportDto>('quest_run', {
      req: { goal, auto_commit: autoCommit },
    }),

  craftPropose: (req: CraftProposeRequest) =>
    invoke<CraftProposalDto>('craft_propose', { req }),
  craftConfirm: (req: CraftActionRequest) =>
    invoke<CraftProposalDto>('craft_confirm', { req }),
  craftReject: (req: CraftActionRequest) =>
    invoke<CraftProposalDto>('craft_reject', { req }),

  commentAdd: (req: CommentAddRequest) =>
    invoke<CommentDto>('comment_add', { req }),
  commentList: (file: string) =>
    invoke<CommentDto[]>('comment_list', { file }),
  commentResolve: (id: string) => invoke<boolean>('comment_resolve', { id }),
  lockAcquire: (file: string) => invoke<LockDto>('lock_acquire', { file }),
  lockRelease: (file: string) => invoke('lock_release', { file }),
  lockShow: (file: string) => invoke<LockDto | null>('lock_show', { file }),
  secretSet: (req: SecretSetRequest) =>
    invoke('secret_set', { req }),
  secretGet: (req: SecretGetRequest) =>
    invoke<string | null>('secret_get', { req }),

  healthOverview: () => invoke<HealthOverviewDto>('health_overview'),
  modelListLocal: () => invoke<string[]>('model_list_local'),
  setLocalModel: (model: string) => invoke('set_local_model', { model }),
};

export const events = {
  onBootstrap: (cb: (s: BootstrapState) => void): Promise<UnlistenFn> =>
    listen<BootstrapState>('bootstrap', (e) => cb(e.payload)),
  onChatToken: (cb: (e: ChatTokenEvent) => void): Promise<UnlistenFn> =>
    listen<ChatTokenEvent>('chat-token', (e) => cb(e.payload)),
  onChatDone: (cb: (e: ChatDoneEvent) => void): Promise<UnlistenFn> =>
    listen<ChatDoneEvent>('chat-done', (e) => cb(e.payload)),
  onChatError: (cb: (e: ChatErrorEvent) => void): Promise<UnlistenFn> =>
    listen<ChatErrorEvent>('chat-error', (e) => cb(e.payload)),
  onAgentEvent: (cb: (e: AgentEventDto) => void): Promise<UnlistenFn> =>
    listen<AgentEventDto>('agent-event', (e) => cb(e.payload)),
  onAgentDone: (cb: (e: ChatDoneEvent) => void): Promise<UnlistenFn> =>
    listen<ChatDoneEvent>('agent-done', (e) => cb(e.payload)),
  onAgentError: (cb: (e: ChatErrorEvent) => void): Promise<UnlistenFn> =>
    listen<ChatErrorEvent>('agent-error', (e) => cb(e.payload)),
};
