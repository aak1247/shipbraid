export interface CursorFeed<T> {
  cursor: number;
  items: T[];
}

export function transitionLogTarget<T>(
  currentRunId: string | null,
  nextRunId: string | null,
  feed: CursorFeed<T>,
): CursorFeed<T> {
  if (currentRunId === nextRunId) return feed;
  return { cursor: 0, items: [] };
}
