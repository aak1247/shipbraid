import { useLayoutEffect, useRef, useState } from "react";
import { Icon } from "./Icon";
import { formatDate, shortId } from "./display";
import type { ExecutorLog, Lane } from "./types";

export function LogFeed({ logs, cursor, runId, loading, executorKind }: {
  logs: ExecutorLog[];
  cursor: number;
  runId: string | null;
  loading: boolean;
  executorKind: Lane["executor_kind"] | null;
}) {
  const viewport = useRef<HTMLDivElement>(null);
  // Freeze the visible window when paused so the rolling cap cannot remove
  // the lines being read. Network polling continues in the parent.
  const [pausedWindow, setPausedWindow] = useState<ExecutorLog[] | null>(null);
  const [wrap, setWrap] = useState(() => window.matchMedia("(max-width: 740px)").matches);
  const following = pausedWindow === null;
  const displayed = pausedWindow ?? logs;
  const pausedCursor = pausedWindow?.at(-1)?.sequence ?? cursor;
  const pending = following ? 0 : logs.filter((log) => log.sequence > pausedCursor).length;

  useLayoutEffect(() => {
    if (following && viewport.current) viewport.current.scrollTop = viewport.current.scrollHeight;
  }, [logs, following, wrap]);

  function handleScroll() {
    const element = viewport.current;
    if (!element) return;
    const atBottom = element.scrollHeight - element.scrollTop - element.clientHeight <= 24;
    if (!atBottom && following) setPausedWindow(logs);
    else if (atBottom && !following) setPausedWindow(null);
  }

  return (
    <section className="log-section" aria-label="执行日志">
      <div className="terminal-toolbar">
        <span className="terminal-title"><Icon name="terminal" size={16} />tail -f <span>{runId ? shortId(runId) : "尚无执行"}</span></span>
        <div className="terminal-controls">
          <button type="button" aria-pressed={wrap} onClick={() => setWrap(!wrap)}><Icon name="wrap" size={15} />自动换行</button>
          <button type="button" aria-pressed={following} disabled={!runId} onClick={() => setPausedWindow(following ? logs : null)}>
            <Icon name={following ? "pause" : "follow"} size={15} />{following ? "暂停跟随" : "继续跟随"}
          </button>
        </div>
      </div>
      <div ref={viewport} className={`terminal-viewport${wrap ? " is-wrapped" : ""}`} onScroll={handleScroll} tabIndex={0} role="region" aria-label="执行器日志，可滚动查看">
        {!runId ? <p className="terminal-empty">此请求尚未关联执行。</p>
          : loading && logs.length === 0 ? <p className="terminal-empty" role="status">正在读取执行日志…</p>
          : displayed.length === 0 ? <p className="terminal-empty">等待{executorKind === "scripted" ? "模拟执行器" : "执行器"}返回日志…</p>
          : <ol className="terminal-lines" aria-label="执行器日志">
            {displayed.map((log) => <li key={log.sequence} className={`terminal-line log-level-${log.level.toLowerCase()}`}>
              <time dateTime={log.timestamp} title={formatDate(log.timestamp)}>{formatDate(log.timestamp).split(" ").at(-1)}</time>
              <span className="log-sequence" title={`序号 ${log.sequence}`}>{log.sequence}</span>
              <span className="log-level">{log.level.toUpperCase()}</span>
              <span className="log-message">{log.message}</span>
            </li>)}
          </ol>}
      </div>
      <div className="terminal-statusbar">
        <span className={following ? "tail-following" : "tail-paused"}><i />{following ? "自动跟随" : "已暂停跟随"}</span>
        {!following && <button type="button" onClick={() => setPausedWindow(null)}><Icon name="follow" size={14} />{pending ? `${pending} 条新日志 · ` : ""}回到底部</button>}
        <span className="terminal-position">{displayed.length} 行 · 序号 {displayed.at(-1)?.sequence ?? 0}</span>
      </div>
      <p className="log-cursor-note">{following ? "向上滚动暂停跟随" : "正在查看暂停时的日志窗口，后台继续接收"} · 保留最近 300 条</p>
    </section>
  );
}
