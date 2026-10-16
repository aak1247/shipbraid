import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { ApiError, createApiClient, describeApiError } from "./api";
import { Icon } from "./Icon";
import { LogFeed } from "./LogFeed";
import {
  eventLabel,
  formatDate,
  phaseLabel,
  priorityLabel,
  requestLabel,
  requestTone,
  revisionLabel,
  runLabel,
  runTone,
  shortId,
} from "./display";
import type {
  AuditEvent,
  DeliveryRun,
  ExecutorLog,
  Lane,
  Priority,
  ReleaseRequest,
  RequestDetail,
  RequestStatus,
  RunDetail,
  RunState,
  SystemInfo,
} from "./types";
import type { ReactNode } from "react";
import { transitionLogTarget } from "./logFeed";
import { requestRelatedToRun, requestRunId } from "./relationships";
import {
  isLaneEnabled,
  laneEnvironments,
  laneExecutorLabel,
  laneProjects,
  lanesInScope,
  preferredEnabledLane,
  receiptIsSynthetic,
  serviceExecutorLabel,
} from "./laneFilters";

type ViewMode = "requests" | "runs";
type Selection = { type: "request" | "run"; id: string } | null;

const REQUEST_STATUSES: RequestStatus[] = [
  "accepted",
  "waiting",
  "satisfied",
  "needs_attention",
  "withdrawn",
  "expired",
];
const RUN_STATES: RunState[] = [
  "planned",
  "dispatching",
  "running",
  "cancel_requested",
  "superseded",
  "canceled",
  "unknown",
  "succeeded",
  "failed",
];

function parseSelection(value: string | null): Selection {
  if (!value) return null;
  const splitAt = value.indexOf(":");
  if (splitAt < 0) return null;
  const type = value.slice(0, splitAt);
  const id = value.slice(splitAt + 1);
  if ((type !== "request" && type !== "run") || !id) return null;
  return { type, id };
}


function mergeBySequence<T extends { sequence: number }>(current: T[], incoming: T[], cap: number): T[] {
  if (incoming.length === 0) return current;
  const merged = new Map(current.map((item) => [item.sequence, item]));
  incoming.forEach((item) => merged.set(item.sequence, item));
  return [...merged.values()].sort((a, b) => a.sequence - b.sequence).slice(-cap);
}

function StatusBadge({ label, tone }: { label: string; tone: string }) {
  return <span className={`status-badge tone-${tone}`}><span className="status-dot" aria-hidden="true" />{label}</span>;
}

function PriorityLabel({ priority }: { priority: Priority }) {
  return <span className={`priority-label ${priority === "urgent" ? "is-urgent" : ""}`}>{priorityLabel(priority)}</span>;
}

function ConnectionScreen({
  input,
  setInput,
  busy,
  error,
  onSubmit,
}: {
  input: string;
  setInput: (value: string) => void;
  busy: boolean;
  error: string | null;
  onSubmit: (event: FormEvent<HTMLFormElement>) => void;
}) {
  const [showToken, setShowToken] = useState(false);
  return (
    <main className="connect-page">
      <div className="connect-topline">
        <Brand />
        <span className="connect-top-note"><Icon name="lock" size={14} />本地协调器</span>
      </div>
      <section className="connect-frame" aria-labelledby="connect-title">
        <div className="connect-glyph" aria-hidden="true"><Icon name="layers" size={22} /></div>
        <h1 id="connect-title">连接发布协调器</h1>
        <p className="connect-description">
          连接后查看真实的发布通道、请求账本和执行记录。此页面不会创建或修改请求。
        </p>
        <form onSubmit={onSubmit} className="connect-form">
          <label htmlFor="access-token">客户端令牌</label>
          <div className="token-field">
            <input
              id="access-token"
              type={showToken ? "text" : "password"}
              value={input}
              onChange={(event) => setInput(event.currentTarget.value)}
              autoComplete="off"
              spellCheck={false}
              autoCapitalize="off"
              placeholder="粘贴本地服务配置的令牌"
              aria-describedby="token-help"
              required
            />
            <button
              type="button"
              className="token-visibility"
              onClick={() => setShowToken((visible) => !visible)}
              aria-label={showToken ? "隐藏令牌" : "显示令牌"}
            >
              {showToken ? "隐藏" : "显示"}
            </button>
          </div>
          <p id="token-help" className="field-help">
            令牌仅保存在当前页面内存中；刷新或断开后会清除。连接请求通过同源 API 发送。
          </p>
          {error && <div className="inline-alert" role="alert"><Icon name="shield" />{error}</div>}
          <button className="primary-button connect-submit" type="submit" disabled={busy}>
            {busy ? <span className="button-spinner" aria-hidden="true" /> : <Icon name="arrow" />}
            {busy ? "正在验证令牌" : "连接协调器"}
          </button>
        </form>
        <div className="connect-footnote">
          <span className="signal-mark" aria-hidden="true" />
          连接后读取当前服务中的通道、执行器与记录；模拟执行数据会明确标注。
        </div>
      </section>
      <footer className="connect-footer">SHIPBRAID <span>发布请求协调台</span></footer>
    </main>
  );
}

function Brand() {
  return (
    <div className="brand-lockup">
      <span className="brand-mark" aria-hidden="true"><i /><i /><i /></span>
      <span className="brand-word">shipbraid</span>
    </div>
  );
}

function LaneFilters({
  project,
  environment,
  projects,
  environments,
  idPrefix,
  onProjectChange,
  onEnvironmentChange,
  className,
}: {
  project: string;
  environment: string;
  projects: string[];
  environments: string[];
  idPrefix: string;
  onProjectChange: (value: string) => void;
  onEnvironmentChange: (value: string) => void;
  className: string;
}) {
  return (
    <div className={className}>
      <label className="lane-filter" htmlFor={`${idPrefix}-project`}>
        <span>项目</span>
        <select id={`${idPrefix}-project`} value={project} onChange={(event) => onProjectChange(event.currentTarget.value)}>
          <option value="">全部项目</option>
          {projects.map((item) => <option key={item} value={item}>{item}</option>)}
        </select>
      </label>
      <label className="lane-filter" htmlFor={`${idPrefix}-environment`}>
        <span>环境</span>
        <select id={`${idPrefix}-environment`} value={environment} onChange={(event) => onEnvironmentChange(event.currentTarget.value)}>
          <option value="">全部环境</option>
          {environments.map((item) => <option key={item} value={item}>{item}</option>)}
        </select>
      </label>
    </div>
  );
}

function recordSearchText(record: ReleaseRequest | DeliveryRun): string {
  const common = [record.id, record.lane_id, record.revision, ...(record.components ?? [])];
  if ("current_run_id" in record) {
    const request = record as ReleaseRequest;
    common.push(request.status, request.priority, request.reason, requestRunId(request) ?? "");
  } else {
    const run = record as DeliveryRun;
    common.push(run.state, run.phase, run.effective_priority, run.replacement_run_id ?? "");
  }
  return common.join(" ").toLowerCase();
}

function App() {
  const [tokenInput, setTokenInput] = useState("");
  const [token, setToken] = useState<string | null>(null);
  const [connectBusy, setConnectBusy] = useState(false);
  const [connectError, setConnectError] = useState<string | null>(null);
  const [system, setSystem] = useState<SystemInfo | null>(null);
  const [lanes, setLanes] = useState<Lane[]>([]);
  const [laneId, setLaneId] = useState<string | null>(null);
  const [projectFilter, setProjectFilter] = useState("");
  const [environmentFilter, setEnvironmentFilter] = useState("");
  const [requests, setRequests] = useState<ReleaseRequest[]>([]);
  const [runs, setRuns] = useState<DeliveryRun[]>([]);
  const [view, setView] = useState<ViewMode>("requests");
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [statusFilter, setStatusFilter] = useState("all");
  const [priorityFilter, setPriorityFilter] = useState("all");
  const [search, setSearch] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  const [requestDetail, setRequestDetail] = useState<RequestDetail | null>(null);
  const [runDetail, setRunDetail] = useState<RunDetail | null>(null);
  const [linkedRun, setLinkedRun] = useState<RunDetail | null>(null);
  const [events, setEvents] = useState<AuditEvent[]>([]);
  const [logs, setLogs] = useState<ExecutorLog[]>([]);
  const [logCursor, setLogCursor] = useState(0);
  const [detailLoading, setDetailLoading] = useState(false);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [detailUpdated, setDetailUpdated] = useState<Date | null>(null);
  const requestsRef = useRef(requests);
  requestsRef.current = requests;

  const selection = parseSelection(selectedKey);
  const visibleLanes = lanesInScope(lanes, { project: projectFilter, environment: environmentFilter });
  const projectOptions = laneProjects(lanes);
  const environmentOptions = laneEnvironments(lanes, projectFilter);
  const activeLane = lanes.find((lane) => lane.id === laneId && isLaneEnabled(lane)) ?? null;
  const selectedExecutorKind = activeLane?.executor_kind
    ?? (system?.executor === "scripted" || system?.executor === "hooks" ? system.executor : null);
  const executorBadgeLabel = activeLane
    ? laneExecutorLabel(activeLane, system?.executor ?? null)
    : system?.executor
      ? serviceExecutorLabel(system.executor)
      : null;

  function clearProtectedData() {
    setSystem(null);
    setLanes([]);
    setLaneId(null);
    setProjectFilter("");
    setEnvironmentFilter("");
    setRequests([]);
    setRuns([]);
    setView("requests");
    setSelectedKey(null);
    setStatusFilter("all");
    setPriorityFilter("all");
    setSearch("");
    setRequestDetail(null);
    setRunDetail(null);
    setLinkedRun(null);
    setEvents([]);
    setLogs([]);
    setLogCursor(0);
    setDetailError(null);
    setDetailLoading(false);
    setError(null);
    setLastUpdated(null);
    setDetailUpdated(null);
  }

  const filteredRequests = useMemo(() => {
    const needle = search.trim().toLowerCase();
    return requests.filter((request) => {
      if (statusFilter !== "all" && request.status !== statusFilter) return false;
      if (priorityFilter !== "all" && request.priority !== priorityFilter) return false;
      return !needle || recordSearchText(request).includes(needle);
    });
  }, [priorityFilter, requests, search, statusFilter]);

  const filteredRuns = useMemo(() => {
    const needle = search.trim().toLowerCase();
    return runs.filter((run) => {
      if (statusFilter !== "all" && run.state !== statusFilter) return false;
      if (priorityFilter !== "all" && run.effective_priority !== priorityFilter) return false;
      return !needle || recordSearchText(run).includes(needle);
    });
  }, [priorityFilter, runs, search, statusFilter]);

  async function handleConnect(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const candidate = tokenInput.trim();
    if (!candidate) return;
    setConnectBusy(true);
    setConnectError(null);
    try {
      const nextSystem = await createApiClient(candidate).getSystem();
      clearProtectedData();
      setSystem(nextSystem);
      setToken(candidate);
      setTokenInput("");
    } catch (connectFailure) {
      setConnectError(describeApiError(connectFailure));
    } finally {
      setConnectBusy(false);
    }
  }

  function disconnect() {
    setToken(null);
    setTokenInput("");
    clearProtectedData();
    setConnectError(null);
  }

  useEffect(() => {
    if (!token) return;
    const api = createApiClient(token);
    let alive = true;
    let timer: number | undefined;
    let controller: AbortController | null = null;
    setLoading(true);

    const load = async () => {
      if (!alive || document.visibilityState !== "visible") return;
      controller?.abort();
      const thisController = new AbortController();
      controller = thisController;
      const signal = thisController.signal;
      try {
        const [nextSystem, nextLanes] = await Promise.all([
          api.getSystem(signal),
          api.getLanes(signal),
        ]);
        if (!alive || signal.aborted) return;
        const nextProjectFilter = projectFilter && nextLanes.some((lane) => lane.project === projectFilter)
          ? projectFilter
          : "";
        const nextEnvironmentFilter = environmentFilter && nextLanes.some((lane) =>
          (!nextProjectFilter || lane.project === nextProjectFilter) && lane.environment === environmentFilter,
        ) ? environmentFilter : "";
        if (nextProjectFilter !== projectFilter) setProjectFilter(nextProjectFilter);
        if (nextEnvironmentFilter !== environmentFilter) setEnvironmentFilter(nextEnvironmentFilter);
        const lanesInCurrentScope = lanesInScope(nextLanes, {
          project: nextProjectFilter,
          environment: nextEnvironmentFilter,
        });
        const requestedLane = preferredEnabledLane(lanesInCurrentScope, laneId)?.id ?? null;
        let nextRequests: ReleaseRequest[] = [];
        let nextRuns: DeliveryRun[] = [];
        if (requestedLane) {
          [nextRequests, nextRuns] = await Promise.all([
            api.getRequests(requestedLane, signal),
            api.getRuns(requestedLane, signal),
          ]);
        }
        if (!alive || signal.aborted) return;
        setSystem(nextSystem);
        setLanes(nextLanes);
        setLaneId(requestedLane);
        setRequests(nextRequests);
        setRuns(nextRuns);
        setError(null);
        setLastUpdated(new Date());
        setLoading(false);
      } catch (loadFailure) {
        if (!alive || signal.aborted) return;
        if (loadFailure instanceof ApiError && loadFailure.status === 401) {
          setConnectError("访问令牌已失效，请重新输入令牌连接。");
          setToken(null);
          clearProtectedData();
        } else {
          setError(describeApiError(loadFailure));
        }
        setLoading(false);
      } finally {
        if (alive && controller === thisController && document.visibilityState === "visible") {
          timer = window.setTimeout(load, 2000);
        }
      }
    };

    const onVisibilityChange = () => {
      if (document.visibilityState === "hidden") {
        window.clearTimeout(timer);
        controller?.abort();
      } else {
        window.clearTimeout(timer);
        void load();
      }
    };
    document.addEventListener("visibilitychange", onVisibilityChange);
    void load();
    return () => {
      alive = false;
      window.clearTimeout(timer);
      controller?.abort();
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, [environmentFilter, laneId, projectFilter, refreshKey, token]);

  useEffect(() => {
    if (!token || !laneId || !selection) {
      setRequestDetail(null);
      setRunDetail(null);
      setLinkedRun(null);
      setEvents([]);
      setLogs([]);
      setLogCursor(0);
      setDetailError(null);
      setDetailLoading(false);
      return;
    }

    const api = createApiClient(token);
    let alive = true;
    let timer: number | undefined;
    let controller: AbortController | null = null;
    let eventCursor = 0;
    let currentLogCursor = 0;
    let currentRunId: string | null = selection.type === "run" ? selection.id : null;
    let eventRecords: AuditEvent[] = [];
    let logRecords: ExecutorLog[] = [];
    setRequestDetail(null);
    setRunDetail(null);
    setLinkedRun(null);
    setEvents([]);
    setLogs([]);
    setLogCursor(0);
    setDetailError(null);
    setDetailLoading(true);

    const loadDetail = async () => {
      if (!alive || document.visibilityState !== "visible") return;
      controller?.abort();
      const thisController = new AbortController();
      controller = thisController;
      const signal = thisController.signal;
      let catchUpLogs = false;
      try {
        const detail = selection.type === "request"
          ? await api.getRequest(selection.id, signal)
          : await api.getRun(selection.id, signal);
        if (!alive || signal.aborted) return;
        if (selection.type === "request") {
          const request = detail as RequestDetail;
          setRequestDetail(request);
          const nextRunId = requestRunId(request);
          if (currentRunId !== nextRunId) {
            const reset = transitionLogTarget(currentRunId, nextRunId, {
              cursor: currentLogCursor,
              items: logRecords,
            });
            currentLogCursor = reset.cursor;
            logRecords = reset.items;
            setLogs(reset.items);
            setLogCursor(reset.cursor);
            setLinkedRun(null);
          }
          currentRunId = nextRunId;
        } else {
          setRunDetail(detail as RunDetail);
          currentRunId = selection.id;
        }

        const [nextLinkedRun, nextEvents, nextLogs] = await Promise.all([
          selection.type === "request" && currentRunId
            ? api.getRun(currentRunId, signal)
            : Promise.resolve(null),
          api.getEvents(laneId, eventCursor, signal),
          currentRunId
            ? api.getLogs(currentRunId, currentLogCursor, signal)
            : Promise.resolve(null),
        ]);
        if (!alive || signal.aborted) return;
        if (selection.type === "request") setLinkedRun(nextLinkedRun);

        const relatedRequestIds = selection.type === "run"
          ? requestsRef.current.filter((request) => requestRelatedToRun(request, selection.id, [...eventRecords, ...nextEvents])).map((request) => request.id)
          : [selection.id];
        const relatedEvents = nextEvents.filter((item) => {
          if (selection.type === "request") {
            return item.request_id === selection.id || (currentRunId !== null && item.run_id === currentRunId);
          }
          return item.run_id === selection.id || (item.request_id !== null && relatedRequestIds.includes(item.request_id));
        });
        if (nextEvents.length > 0) eventCursor = Math.max(eventCursor, ...nextEvents.map((item) => item.sequence));
        eventRecords = mergeBySequence(eventRecords, relatedEvents, 400);
        setEvents(eventRecords);

        if (nextLogs) {
          const previousLogCursor = currentLogCursor;
          const cursorFromPayload = Number.isFinite(nextLogs.next_cursor) ? nextLogs.next_cursor : currentLogCursor;
          if (nextLogs.items.length > 0) {
            currentLogCursor = Math.max(cursorFromPayload, nextLogs.items[nextLogs.items.length - 1]!.sequence);
          } else {
            currentLogCursor = Math.max(currentLogCursor, cursorFromPayload);
          }
          catchUpLogs = nextLogs.items.length === 100 && currentLogCursor > previousLogCursor;
          logRecords = mergeBySequence(logRecords, nextLogs.items, 300);
          setLogs(logRecords);
          setLogCursor(currentLogCursor);
        } else {
          currentLogCursor = 0;
          logRecords = [];
          setLogs([]);
          setLogCursor(0);
        }
        setDetailError(null);
        setDetailUpdated(new Date());
        setDetailLoading(false);
      } catch (detailFailure) {
        if (!alive || signal.aborted) return;
        setDetailError(describeApiError(detailFailure));
        setDetailLoading(false);
        if (detailFailure instanceof ApiError && detailFailure.status === 401) {
          setConnectError("访问令牌已失效，请重新输入令牌连接。");
          setToken(null);
          clearProtectedData();
        }
      } finally {
        if (alive && controller === thisController && document.visibilityState === "visible") {
          timer = window.setTimeout(loadDetail, catchUpLogs ? 100 : 2000);
        }
      }
    };

    const onVisibilityChange = () => {
      if (document.visibilityState === "hidden") {
        window.clearTimeout(timer);
        controller?.abort();
      } else {
        window.clearTimeout(timer);
        void loadDetail();
      }
    };
    document.addEventListener("visibilitychange", onVisibilityChange);
    void loadDetail();
    return () => {
      alive = false;
      window.clearTimeout(timer);
      controller?.abort();
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, [laneId, selectedKey, token]);

  function activateLane(nextLane: string | null) {
    if (nextLane === laneId) return;
    setLaneId(nextLane);
    setRequests([]);
    setRuns([]);
    setSelectedKey(null);
    setStatusFilter("all");
    setPriorityFilter("all");
    setSearch("");
    setRequestDetail(null);
    setRunDetail(null);
    setLinkedRun(null);
    setEvents([]);
    setLogs([]);
    setLogCursor(0);
    setError(null);
    setLastUpdated(null);
    setDetailError(null);
    setDetailUpdated(null);
    setLoading(true);
  }

  function chooseLane(nextLane: string) {
    const candidate = visibleLanes.find((lane) => lane.id === nextLane);
    if (!candidate || !isLaneEnabled(candidate)) return;
    activateLane(candidate.id);
  }

  function chooseProjectFilter(nextProject: string) {
    setProjectFilter(nextProject);
    setEnvironmentFilter("");
    const scoped = lanesInScope(lanes, { project: nextProject, environment: "" });
    activateLane(preferredEnabledLane(scoped, laneId)?.id ?? null);
  }

  function chooseEnvironmentFilter(nextEnvironment: string) {
    setEnvironmentFilter(nextEnvironment);
    const scoped = lanesInScope(lanes, { project: projectFilter, environment: nextEnvironment });
    activateLane(preferredEnabledLane(scoped, laneId)?.id ?? null);
  }

  function chooseView(nextView: ViewMode) {
    if (nextView === view) return;
    setView(nextView);
    setStatusFilter("all");
    const first = nextView === "requests" ? filteredRequests[0] : filteredRuns[0];
    setSelectedKey(first ? `${nextView === "requests" ? "request" : "run"}:${first.id}` : null);
  }

  const closeDetails = useCallback(() => setSelectedKey(null), []);

  function inspectRelatedRecord(type: "request" | "run", id: string) {
    setView(type === "run" ? "runs" : "requests");
    setStatusFilter("all");
    setPriorityFilter("all");
    setSearch("");
    selectRecord(type, id);
  }

  function selectRecord(type: "request" | "run", id: string) {
    setSelectedKey(`${type}:${id}`);
  }

  if (!token) {
    return (
      <ConnectionScreen
        input={tokenInput}
        setInput={setTokenInput}
        busy={connectBusy}
        error={connectError}
        onSubmit={handleConnect}
      />
    );
  }

  const selectedRequestFromList = selection?.type === "request"
    ? requests.find((request) => request.id === selection.id) ?? null
    : null;
  const selectedRunFromList = selection?.type === "run"
    ? runs.find((run) => run.id === selection.id) ?? null
    : null;
  const visibleRequest = requestDetail ?? selectedRequestFromList;
  const visibleRun = runDetail ?? selectedRunFromList;
  const currentRows = view === "requests" ? filteredRequests : filteredRuns;
  const currentStatuses = view === "requests" ? REQUEST_STATUSES : RUN_STATES;

  return (
    <div className="app-shell min-h-screen bg-canvas text-ink">
      <header className="topbar">
        <div className="topbar-brand"><Brand /><span className="topbar-divider" /><span className="topbar-context">发布协调台</span></div>
        <div className="topbar-right">
          {executorBadgeLabel && (
            <span className={`executor-badge ${selectedExecutorKind === "scripted" ? "is-scripted" : ""}`}>
              <span className="executor-dot" />{executorBadgeLabel}
            </span>
          )}
          <span className={`connection-label ${error ? "connection-stale" : ""}`}>
            <span className="connection-dot" />
            {error ? "数据更新异常" : "已连接"}
          </span>
          <span className="sync-time" title={lastUpdated?.toISOString()}>
            {lastUpdated ? `更新于 ${formatDate(lastUpdated.toISOString())}` : "等待首次同步"}
          </span>
          <button
            className="icon-button refresh-button"
            onClick={() => setRefreshKey((key) => key + 1)}
            aria-label="立即刷新"
            title="立即刷新"
          ><Icon name="refresh" /></button>
          <button className="disconnect-button" onClick={disconnect}>断开连接</button>
        </div>
      </header>

      <div className="mobile-lane-control">
        <LaneFilters
          className="mobile-lane-filters"
          idPrefix="mobile-lane"
          project={projectFilter}
          environment={environmentFilter}
          projects={projectOptions}
          environments={environmentOptions}
          onProjectChange={chooseProjectFilter}
          onEnvironmentChange={chooseEnvironmentFilter}
        />
        <label className="mobile-lane-picker" htmlFor="mobile-lane-select">
          <span>发布通道</span>
          <select id="mobile-lane-select" value={laneId ?? ""} onChange={(event) => chooseLane(event.currentTarget.value)}>
            <option value="">{visibleLanes.some(isLaneEnabled)
              ? "选择通道"
              : visibleLanes.length
                ? "无已启用通道"
                : lanes.length === 0
                  ? "暂无通道"
                  : "无匹配通道"}</option>
            {visibleLanes.map((lane) => <option key={lane.id} value={lane.id} disabled={!isLaneEnabled(lane)}>
              {lane.project} / {lane.environment}{!isLaneEnabled(lane) ? "（已禁用）" : ""}
            </option>)}
          </select>
        </label>
      </div>

      <div className="console-grid">
        <aside className="lane-rail" aria-label="发布通道">
          <div className="rail-heading">
            <div><h2>发布通道</h2><p>LANES <span>{visibleLanes.length}{visibleLanes.length !== lanes.length ? ` / ${lanes.length}` : ""}</span></p></div>
            <span className="rail-rule" />
          </div>
          <LaneFilters
            className="lane-filters"
            idPrefix="rail-lane"
            project={projectFilter}
            environment={environmentFilter}
            projects={projectOptions}
            environments={environmentOptions}
            onProjectChange={chooseProjectFilter}
            onEnvironmentChange={chooseEnvironmentFilter}
          />
          {loading && lanes.length === 0 ? (
            <div className="lane-loading" aria-label="正在读取发布通道"><span /><span /><span /></div>
          ) : lanes.length === 0 ? (
            <div className="rail-empty">服务尚未配置通道。</div>
          ) : visibleLanes.length === 0 ? (
            <div className="rail-empty">筛选条件下没有发布通道。</div>
          ) : (
            <nav className="lane-list">
              {visibleLanes.map((lane) => (
                <button
                  type="button"
                  className={`lane-item ${lane.id === laneId ? "is-current" : ""} ${!isLaneEnabled(lane) ? "is-disabled" : ""}`}
                  key={lane.id}
                  onClick={() => chooseLane(lane.id)}
                  disabled={!isLaneEnabled(lane)}
                  aria-current={lane.id === laneId ? "page" : undefined}
                  title={`${lane.project} / ${lane.environment} · ${lane.id}${!isLaneEnabled(lane) ? " · 已禁用，不可选择" : ""}`}
                >
                  <span className="lane-item-top"><span className={`lane-state ${lane.active_run_id ? "has-run" : ""}`} />
                    <span className="lane-project">{lane.project}</span></span>
                  <span className="lane-environment">{lane.environment}</span>
                  <span className="lane-id">{shortId(lane.id)}</span>
                  <span className="lane-item-meta">
                    <span className="lane-executor">{laneExecutorLabel(lane, system?.executor ?? null)}</span>
                    {!isLaneEnabled(lane) && <span className="lane-disabled">已禁用</span>}
                  </span>
                  {lane.blocked_reason && <span className="lane-blocked">需核对</span>}
                </button>
              ))}
            </nav>
          )}
          <div className="rail-footnote">
            <Icon name="shield" size={15} />
            <span>只读视图<br /><small>来源：协调器 API</small></span>
          </div>
        </aside>

        <main className="ledger-pane" aria-labelledby="page-title">
          <div className="page-heading">
            <div className="heading-copy">
              <h1 id="page-title">{view === "requests" ? "发布请求" : "执行记录"}</h1>
              <p>
                {activeLane ? <><span>{activeLane.project}</span><span className="crumb-slash">/</span><span>{activeLane.environment}</span></> : "等待发布通道"}
                {activeLane?.active_run_id && <span className="active-run-caption"><i />存在活动执行</span>}
              </p>
            </div>
            {activeLane && <div className="generation-tag">通道代次 <strong>{activeLane.generation}</strong></div>}
          </div>

          {error && (
            <div className="data-alert" role="status">
              <Icon name="activity" />
              <span><strong>数据暂未刷新。</strong> {error}{lastUpdated && <small> 当前显示截至 {formatDate(lastUpdated.toISOString())} 的数据。</small>}</span>
              <button type="button" onClick={() => setRefreshKey((key) => key + 1)}>重试</button>
            </div>
          )}

          {activeLane?.blocked_reason && (
            <div className="blocked-alert" role="status">
              <Icon name="shield" />
              <span><strong>此通道需要核对</strong><small>{activeLane.blocked_reason}</small><small>未知执行保留环境写入所有权；确认前不会释放通道。</small></span>
            </div>
          )}

          {system?.draining && (
            <div className="draining-alert" role="status"><Icon name="clock" />服务正在排空；现有执行仍会继续记录状态。</div>
          )}

          <div className="ledger-toolbar">
            <div className="view-switch" role="group" aria-label="记录类型">
              <button type="button" aria-pressed={view === "requests"} className={view === "requests" ? "is-selected" : ""} onClick={() => chooseView("requests")}>
                <Icon name="layers" size={15} />请求 <span>{requests.length}</span>
              </button>
              <button type="button" aria-pressed={view === "runs"} className={view === "runs" ? "is-selected" : ""} onClick={() => chooseView("runs")}>
                <Icon name="activity" size={15} />执行 <span>{runs.length}</span>
              </button>
            </div>
            <div className="filter-controls">
              <label className="search-field">
                <Icon name="search" size={15} />
                <span className="visually-hidden">搜索请求或执行</span>
                <input value={search} onChange={(event) => setSearch(event.currentTarget.value)} placeholder="搜索 ID、版本或组件" />
                {search && <button type="button" aria-label="清除搜索" onClick={() => setSearch("")}><Icon name="x" size={14} /></button>}
              </label>
              <label className="select-field">
                <span className="visually-hidden">按状态筛选</span>
                <select value={statusFilter} onChange={(event) => setStatusFilter(event.currentTarget.value)}>
                  <option value="all">全部状态</option>
                  {currentStatuses.map((status) => <option key={status} value={status}>{view === "requests" ? requestLabel(status as RequestStatus) : runLabel(status as RunState)}</option>)}
                </select><Icon name="chevron" size={14} />
              </label>
              <label className="select-field priority-select">
                <span className="visually-hidden">按优先级筛选</span>
                <select value={priorityFilter} onChange={(event) => setPriorityFilter(event.currentTarget.value)}>
                  <option value="all">全部优先级</option><option value="normal">普通</option><option value="urgent">紧急</option>
                </select><Icon name="chevron" size={14} />
              </label>
            </div>
          </div>

          <div className="table-heading-row">
            <div><span className="table-title">{view === "requests" ? "请求账本" : "执行队列"}</span>
              <span className="record-count">{currentRows.length}{currentRows.length !== (view === "requests" ? requests.length : runs.length) ? ` / ${view === "requests" ? requests.length : runs.length}` : ""} 条</span></div>
            <span className="sort-note">按创建时间 · 最新优先</span>
          </div>

          <div className="table-wrap">
            {loading && activeLane && currentRows.length === 0 ? (
              <div className="table-loading" role="status" aria-label="正在读取记录"><span /><span /><span /><span /></div>
            ) : !activeLane ? (
              <div className="empty-state">
                <span className="empty-line" />
                <h2>{lanes.length === 0
                  ? "还没有发布通道"
                  : visibleLanes.length === 0
                    ? "没有匹配的发布通道"
                    : visibleLanes.some(isLaneEnabled)
                      ? "请选择可用发布通道"
                      : "此范围内没有已启用通道"}</h2>
                <p>{lanes.length === 0
                  ? "协调器 API 当前没有返回通道记录。"
                  : visibleLanes.length === 0
                    ? "调整项目或环境筛选后查看可用通道。"
                    : visibleLanes.some(isLaneEnabled)
                      ? "请选择一个已启用的发布通道查看请求与执行记录。"
                      : "已禁用通道不会读取执行记录；可在通道列表中查看其禁用状态。"}</p>
                {visibleLanes.length > 0 && !visibleLanes.some(isLaneEnabled) && <span className="disabled-scope-note">此筛选范围内的通道均已禁用。</span>}
              </div>
            ) : currentRows.length === 0 ? (
              <div className="empty-state">
                <span className="empty-line" />
                <h2>{search || statusFilter !== "all" || priorityFilter !== "all" ? "没有匹配的记录" : view === "requests" ? "此通道暂无请求" : "此通道暂无执行"}</h2>
                <p>{search || statusFilter !== "all" || priorityFilter !== "all" ? "调整搜索词或筛选条件后查看。" : "新记录出现后会在此处显示。"}</p>
                {(search || statusFilter !== "all" || priorityFilter !== "all") && <button className="text-button" onClick={() => { setSearch(""); setStatusFilter("all"); setPriorityFilter("all"); }}>清除筛选</button>}
              </div>
            ) : view === "requests" ? (
              <RequestTable
                rows={filteredRequests}
                selectedId={selection?.type === "request" ? selection.id : null}
                onSelect={(id) => selectRecord("request", id)}
              />
            ) : (
              <RunTable
                rows={filteredRuns}
                selectedId={selection?.type === "run" ? selection.id : null}
                onSelect={(id) => selectRecord("run", id)}
              />
            )}
          </div>
          <div className="ledger-footline">
            <span><i className="live-tick" />自动刷新 · 页面可见时每 2 秒</span>
            <span>最近 200 条 · 筛选当前列表</span>
          </div>
        </main>

        {selection && <DetailPanel
          key={selectedKey}
          onClose={closeDetails}
          selection={selection}
          request={visibleRequest}
          run={visibleRun}
          linkedRun={linkedRun}
          relatedRequests={visibleRun ? requests.filter((request) => requestRelatedToRun(request, visibleRun.id, events)) : []}
          events={events}
          logs={logs}
          logCursor={logCursor}
          detailLoading={detailLoading}
          detailError={detailError}
          detailUpdated={detailUpdated}
          executorKind={selectedExecutorKind}
          onSelectRequest={(id) => inspectRelatedRecord("request", id)}
          onSelectRun={(id) => inspectRelatedRecord("run", id)}
        />}
      </div>
    </div>
  );
}

function RequestTable({ rows, selectedId, onSelect }: { rows: ReleaseRequest[]; selectedId: string | null; onSelect: (id: string) => void }) {
  return (
    <table className="record-table request-table">
      <thead><tr><th scope="col">状态 / ID</th><th scope="col">版本</th><th scope="col">优先级</th><th scope="col">关联执行</th><th scope="col">创建时间</th></tr></thead>
      <tbody>
        {rows.map((request) => (
          <tr key={request.id} className={selectedId === request.id ? "is-active" : ""} onClick={() => onSelect(request.id)}>
            <td className="record-primary-cell">
              <div className="record-main-line"><StatusBadge label={requestLabel(request.status)} tone={requestTone(request.status)} /></div>
              <button type="button" className="record-id" onClick={() => onSelect(request.id)} title={request.id} aria-label={`查看请求 ${request.id}`}>
                <span className="type-marker request-marker" aria-hidden="true">R</span>{shortId(request.id)}
              </button>
            </td>
            <td><code className="revision-code" title={request.revision}>{revisionLabel(request.revision)}</code>
              <small className="cell-subline">{request.coverage_mode === "descendant" ? "允许后续版本覆盖" : "精确版本"}</small></td>
            <td><PriorityLabel priority={request.priority} /></td>
            <td>{requestRunId(request) ? <><span className="related-id"><Icon name="link" size={13} />{shortId(requestRunId(request)!)}</span>{request.satisfied_by_run && <small className="cell-subline">验收完成时的执行</small>}</> : <span className="muted-dash">尚未分配</span>}</td>
            <td className="date-cell"><time dateTime={request.created_at}>{formatDate(request.created_at)}</time></td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function RunTable({ rows, selectedId, onSelect }: { rows: DeliveryRun[]; selectedId: string | null; onSelect: (id: string) => void }) {
  return (
    <table className="record-table run-table">
      <thead><tr><th scope="col">状态 / ID</th><th scope="col">版本</th><th scope="col">当前阶段</th><th scope="col">优先级</th><th scope="col">替代执行</th><th scope="col">更新时间</th></tr></thead>
      <tbody>
        {rows.map((run) => (
          <tr key={run.id} className={selectedId === run.id ? "is-active" : ""} onClick={() => onSelect(run.id)}>
            <td className="record-primary-cell">
              <div className="record-main-line"><StatusBadge label={runLabel(run.state)} tone={runTone(run.state)} /></div>
              <button type="button" className="record-id" onClick={() => onSelect(run.id)} title={run.id} aria-label={`查看执行 ${run.id}`}>
                <span className="type-marker run-marker" aria-hidden="true">E</span>{shortId(run.id)}
              </button>
            </td>
            <td><code className="revision-code" title={run.revision}>{revisionLabel(run.revision)}</code>
              <small className="cell-subline">通道代次 {run.generation}</small></td>
            <td><span className="phase-cell"><span className={`phase-marker phase-${run.phase}`} />{phaseLabel(run.phase)}</span></td>
            <td><PriorityLabel priority={run.effective_priority} /></td>
            <td>{run.replacement_run_id ? <span className="related-id"><Icon name="arrow" size={13} />{shortId(run.replacement_run_id)}</span> : <span className="muted-dash">—</span>}</td>
            <td className="date-cell"><time dateTime={run.updated_at}>{formatDate(run.updated_at)}</time></td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function DetailPanel({
  onClose,
  selection,
  request,
  run,
  linkedRun,
  relatedRequests,
  events,
  logs,
  logCursor,
  detailLoading,
  detailError,
  detailUpdated,
  executorKind,
  onSelectRequest,
  onSelectRun,
}: {
  onClose: () => void;
  selection: Selection;
  request: ReleaseRequest | null;
  run: DeliveryRun | null;
  linkedRun: RunDetail | null;
  relatedRequests: ReleaseRequest[];
  events: AuditEvent[];
  logs: ExecutorLog[];
  logCursor: number;
  detailLoading: boolean;
  detailError: string | null;
  detailUpdated: Date | null;
  executorKind: Lane["executor_kind"] | null;
  onSelectRequest: (id: string) => void;
  onSelectRun: (id: string) => void;
}) {
  const [tab, setTab] = useState<"timeline" | "logs">(selection?.type === "run" ? "logs" : "timeline");
  const closeButton = useRef<HTMLButtonElement>(null);
  const opener = useRef<HTMLElement | null>(null);
  const selectedType = selection?.type ?? null;
  const isRequest = selectedType === "request";
  const currentRequest = isRequest ? request : null;
  const currentRun = selectedType === "run" ? run : null;
  const id = currentRequest?.id ?? currentRun?.id ?? selection?.id ?? null;
  const currentStatus = currentRequest
    ? <StatusBadge label={requestLabel(currentRequest.status)} tone={requestTone(currentRequest.status)} />
    : currentRun
      ? <StatusBadge label={runLabel(currentRun.state)} tone={runTone(currentRun.state)} />
      : null;

  const dismiss = useCallback(() => {
    onClose();
    if (opener.current?.isConnected) opener.current.focus({ preventScroll: true });
  }, [onClose]);

  useEffect(() => {
    const label = `查看${selectedType === "run" ? "执行" : "请求"} ${selection?.id}`;
    opener.current = Array.from(document.querySelectorAll<HTMLButtonElement>("button[aria-label]"))
      .find((button) => button.getAttribute("aria-label") === label)
      ?? document.querySelector<HTMLButtonElement>('.view-switch button[aria-pressed="true"]')
      ?? (document.activeElement instanceof HTMLElement ? document.activeElement : null);
    closeButton.current?.focus({ preventScroll: true });
    const onEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !event.defaultPrevented) { event.preventDefault(); dismiss(); }
    };
    document.addEventListener("keydown", onEscape);
    return () => document.removeEventListener("keydown", onEscape);
  }, [dismiss, selectedType, selection?.id]);

  async function copyId() {
    if (!id || !navigator.clipboard) return;
    try { await navigator.clipboard.writeText(id); } catch { /* Clipboard access can be unavailable in non-secure local origins. */ }
  }

  return (
    <aside className="detail-pane" aria-label="记录详情">
      <div className="detail-topline">
        <div><span className="detail-overline">记录详情</span><span className="detail-freshness">{detailUpdated ? `同步于 ${formatDate(detailUpdated.toISOString())}` : "等待读取"}</span></div>
        {detailLoading && <span className="mini-spinner" role="status" aria-label="正在更新详情" />}
        <button ref={closeButton} className="icon-button detail-close" type="button" onClick={dismiss} aria-label="关闭记录详情" title="关闭 (Esc)"><Icon name="x" size={19} /></button>
      </div>
      <div className={`detail-body${tab === "logs" ? " is-logs" : ""}`}>
      {!selection ? (
        <div className="detail-placeholder">
          <div className="placeholder-mark"><Icon name="arrow" size={18} /></div>
          <h2>选择一条记录</h2>
          <p>查看请求归属、执行阶段、审计事件与受限日志。</p>
          <div className="placeholder-rule" />
          <small>请求与执行保持独立状态。</small>
        </div>
      ) : (
        <>
          <div className="detail-record-heading">
            <span className={`detail-type ${isRequest ? "detail-request" : "detail-run"}`}>
              <span className="type-marker" aria-hidden="true">{isRequest ? "R" : "E"}</span>{isRequest ? "发布请求" : "执行记录"}
            </span>
            {currentStatus}
            <h2 title={id ?? undefined}>{id ? shortId(id) : "读取记录…"}</h2>
            {id && <button type="button" className="copy-id" onClick={copyId} aria-label="复制完整记录 ID" title="复制完整 ID"><Icon name="copy" size={14} />复制 ID</button>}
          </div>
          {detailError && <div className="detail-error" role="status">{detailError}</div>}
          {selection && !request && !run && detailLoading && <div className="detail-skeleton"><span /><span /><span /></div>}
          <details className="inspector-disclosure" open={tab === "timeline" || undefined}>
            <summary>{isRequest ? "请求信息与关联执行" : "执行信息与验收"}<Icon name="chevron" size={15} /></summary>
          {currentRequest && (
            <RequestInspector
              request={currentRequest}
              linkedRun={linkedRun}
              executorKind={executorKind}
              onSelectRun={onSelectRun}
            />
          )}
          {currentRun && (
            <RunInspector
              run={currentRun}
              relatedRequests={relatedRequests}
              executorKind={executorKind}
              onSelectRequest={onSelectRequest}
              onSelectRun={onSelectRun}
            />
          )}
          </details>

          <div className="detail-subnav" role="group" aria-label="详情内容">
            <button type="button" aria-pressed={tab === "timeline"} className={tab === "timeline" ? "is-selected" : ""} onClick={() => setTab("timeline")}>
              <Icon name="activity" size={14} />事件时间线<span>{events.length}</span>
            </button>
            <button type="button" aria-pressed={tab === "logs"} className={tab === "logs" ? "is-selected" : ""} onClick={() => setTab("logs")}>
              <Icon name="terminal" size={14} />运行日志<span>{logs.length}</span>
            </button>
          </div>
          {tab === "timeline" ? (
            <EventTimeline events={events} loading={detailLoading} />
          ) : (
            <LogFeed key={currentRun?.id ?? linkedRun?.id ?? "pending"} logs={logs} cursor={logCursor} runId={currentRun?.id ?? linkedRun?.id ?? null} loading={detailLoading} executorKind={executorKind} />
          )}
          <div className="detail-footline">
            <span><Icon name="lock" size={13} />日志来源：{executorKind === "scripted" ? "模拟执行器" : executorKind === "hooks" ? "Hooks 执行器" : "当前通道执行器"}</span>
            <span>最多展示最近 {Math.max(logs.length, 100)} 条已读取记录</span>
          </div>
        </>
      )}
      </div>
    </aside>
  );
}

function RequestInspector({ request, linkedRun, executorKind, onSelectRun }: { request: ReleaseRequest; linkedRun: RunDetail | null; executorKind: Lane["executor_kind"] | null; onSelectRun: (id: string) => void }) {
  const detail = request as RequestDetail;
  const linkedRunId = requestRunId(request);
  return (
    <div className="inspector-content">
      <div className="detail-data-grid">
        <DetailField label="目标版本"><code>{request.revision}</code></DetailField>
        <DetailField label="优先级"><PriorityLabel priority={request.priority} /></DetailField>
        <DetailField label="覆盖规则">{request.coverage_mode === "descendant" ? "允许后续版本覆盖" : "精确版本"}</DetailField>
        <DetailField label="创建时间">{formatDate(request.created_at)}</DetailField>
        {request.deadline_at && <DetailField label="截止时间">{formatDate(request.deadline_at)}</DetailField>}
      </div>
      {request.components.length > 0 && <div className="detail-section"><h3>目标组件</h3><ComponentList components={request.components} /></div>}
      {request.reason && <div className="reason-block"><span>请求原因</span><p>{request.reason}</p></div>}
      {request.status === "satisfied" && detail.receipt && <ReceiptPanel receipt={detail.receipt} executorKind={executorKind} />}
      <div className="relation-block">
        <div className="relation-heading"><h3>{request.satisfied_by_run ? "满足此请求的执行" : "当前执行归属"}</h3><span>请求 → 执行</span></div>
        {linkedRunId ? (
          <button className="relation-link" type="button" onClick={() => onSelectRun(linkedRunId)}>
            <span className="relation-id"><span className="type-marker run-marker">E</span><code>{shortId(linkedRunId)}</code></span>
            {linkedRun ? <StatusBadge label={runLabel(linkedRun.state)} tone={runTone(linkedRun.state)} /> : <span className="relation-loading">读取执行状态…</span>}
            <Icon name="arrow" size={15} />
          </button>
        ) : (
          <div className="relation-empty">此请求当前没有关联的执行。</div>
        )}
        {linkedRun?.replacement_run_id && (
          <div className="replacement-line"><span>此执行后被</span><button type="button" onClick={() => onSelectRun(linkedRun.replacement_run_id!)}>{shortId(linkedRun.replacement_run_id)}</button><span>替代</span></div>
        )}
        {detail.coverage_reason && <p className="coverage-reason">覆盖依据：{detail.coverage_reason}</p>}
      </div>
    </div>
  );
}

function RunInspector({ run, relatedRequests, executorKind, onSelectRequest, onSelectRun }: { run: DeliveryRun; relatedRequests: ReleaseRequest[]; executorKind: Lane["executor_kind"] | null; onSelectRequest: (id: string) => void; onSelectRun: (id: string) => void }) {
  const detail = run as RunDetail;
  return (
    <div className="inspector-content">
      <div className="run-phase-strip">
        <div><span className="phase-pulse" /><span>当前阶段</span></div>
        <strong>{phaseLabel(run.phase)}</strong>
        <small>通道代次 {run.generation}</small>
      </div>
      <div className="detail-data-grid">
        <DetailField label="执行版本"><code>{run.revision}</code></DetailField>
        <DetailField label="有效优先级"><PriorityLabel priority={run.effective_priority} /></DetailField>
        <DetailField label="开始记录">{formatDate(run.created_at)}</DetailField>
        <DetailField label="最近更新">{formatDate(run.updated_at)}</DetailField>
      </div>
      {run.components.length > 0 && <div className="detail-section"><h3>执行组件</h3><ComponentList components={run.components} /></div>}
      {detail.receipt && <ReceiptPanel receipt={detail.receipt} executorKind={executorKind} />}
      {run.state === "superseded" && (
        <div className="superseded-callout">
          <strong>执行已被替代</strong>
          <p>这是一次执行的终态记录，不代表关联请求失败。请求状态独立保留，并可追踪后继执行的验收结果。</p>
        </div>
      )}
      {run.state === "unknown" && (
        <div className="unknown-callout"><Icon name="shield" size={16} /><span><strong>外部执行状态未知</strong><small>环境写入所有权仍由此执行保留。</small></span></div>
      )}
      {run.replacement_run_id && (
        <div className="relation-block replacement-block">
          <div className="relation-heading"><h3>后继执行</h3><span>本执行 → 后继</span></div>
          <button className="relation-link" type="button" onClick={() => onSelectRun(run.replacement_run_id!)}>
            <span className="relation-id"><span className="type-marker run-marker">E</span><code>{shortId(run.replacement_run_id)}</code></span>
            <span className="relation-loading">查看后继记录</span><Icon name="arrow" size={15} />
          </button>
        </div>
      )}
      <div className="relation-block request-relations">
        <div className="relation-heading"><h3>关联请求</h3><span>{relatedRequests.length} 条 · 含已读取历史</span></div>
        {relatedRequests.length > 0 ? (
          <div className="related-request-list">
            {relatedRequests.map((request) => <button type="button" className="related-request" key={request.id} onClick={() => onSelectRequest(request.id)}>
              <span className="relation-id"><span className="type-marker request-marker">R</span><code>{shortId(request.id)}</code></span>
              <StatusBadge label={requestLabel(request.status)} tone={requestTone(request.status)} />
              <Icon name="arrow" size={14} />
            </button>)}
          </div>
        ) : <div className="relation-empty">当前已读取的请求中没有关联项。</div>}
        {detail.coverage_reason && <p className="coverage-reason">覆盖依据：{detail.coverage_reason}</p>}
      </div>
    </div>
  );
}

function ReceiptPanel({ receipt, executorKind }: { receipt: NonNullable<RequestDetail["receipt"]>; executorKind: Lane["executor_kind"] | null }) {
  const digests = Object.entries(receipt.component_digests ?? {});
  let report = "";
  try {
    report = JSON.stringify(receipt.verification_report, null, 2) ?? "";
  } catch {
    report = String(receipt.verification_report);
  }
  const simulated = receiptIsSynthetic(receipt, executorKind);
  const resultLabel = (simulated ? "模拟" : "") + (receipt.result === "success" ? "验收通过" : receipt.result === "failure" ? "验收失败" : receipt.result);
  return (
    <section className="receipt-panel" aria-label="部署验收回执">
      <div className="receipt-heading">
        <span><Icon name="check" size={14} />{simulated ? "模拟验收回执" : "环境验收回执"}</span>
        <span className={`receipt-result ${receipt.result === "success" ? "is-success" : receipt.result === "failure" ? "is-failure" : ""}`}>{resultLabel}</span>
      </div>
      <div className="receipt-grid">
        <DetailField label="回执 ID"><code>{shortId(receipt.id)}</code></DetailField>
        <DetailField label="来源 run"><code>{shortId(receipt.run_id)}</code></DetailField>
        <DetailField label="验收来源 SHA"><code>{receipt.source_revision || "—"}</code></DetailField>
        <DetailField label="观察时间">{formatDate(receipt.observed_at)}</DetailField>
        <DetailField label="通道代次">{receipt.generation}</DetailField>
        <DetailField label="Profile 摘要"><code>{receipt.profile_digest || "—"}</code></DetailField>
      </div>
      {digests.length > 0 && <details className="receipt-digests">
        <summary>组件摘要 <span>{digests.length}</span></summary>
        <dl>{digests.map(([name, value]) => <div key={name}><dt>{name}</dt><dd><code>{typeof value === "string" ? value : JSON.stringify(value)}</code></dd></div>)}</dl>
      </details>}
      {report && <details className="verification-report">
        <summary>验收报告 <span>{simulated ? "模拟证据" : "服务返回的验收证据"}</span></summary>
        <pre>{report}</pre>
      </details>}
    </section>
  );
}

function DetailField({ label, children }: { label: string; children: ReactNode }) {
  return <div className="detail-field"><span>{label}</span><strong>{children}</strong></div>;
}

function ComponentList({ components }: { components: string[] }) {
  return <div className="component-list">{components.map((component) => <code key={component}>{component}</code>)}</div>;
}

function EventTimeline({ events, loading }: { events: AuditEvent[]; loading: boolean }) {
  if (loading && events.length === 0) return <div className="feed-loading"><span /><span /><span /></div>;
  if (events.length === 0) return <div className="feed-empty"><span className="empty-line" /><p>当前读取范围内没有相关事件。</p><small>事件按服务端顺序游标读取。</small></div>;
  return (
    <ol className="event-timeline" aria-label="请求与执行事件">
      {events.map((event) => <li key={event.sequence}>
        <span className="timeline-stem"><i /></span>
        <div className="event-copy"><div className="event-heading"><strong>{eventLabel(event.kind)}</strong><time dateTime={event.timestamp}>{formatDate(event.timestamp)}</time></div>
          <p>{event.message}</p><small>序号 {event.sequence}{event.run_id ? ` · 执行 ${shortId(event.run_id)}` : ""}{event.request_id ? ` · 请求 ${shortId(event.request_id)}` : ""}</small></div>
      </li>)}
    </ol>
  );
}

export default App;
