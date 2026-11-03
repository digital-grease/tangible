// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * Live pages, through the event stream (API.md 12, docs/plans/event-stream.md).
 *
 * A page names the topics it shows and how to refresh itself. It refreshes
 * soon after a relevant event, a few events gathered into one refresh, and
 * reconciles on a slow timer whatever the stream says, as AGENTS.md 9 asks:
 * an event says only that something changed, and the page reads it through
 * the same REST routes as ever. Where the browser has no EventSource the page
 * polls at the rate it always did.
 */

/** The event stream's topics. */
export type Topic =
  'imports' | 'burns' | 'rips' | 'erasures' | 'workers' | 'derivations' | 'library';

/** Every event type the server sends, by topic. */
export const EVENT_TYPES: Record<Topic, readonly string[]> = {
  imports: ['import.changed'],
  burns: ['burn_job.changed', 'burn_attempt.changed', 'burn.progress'],
  rips: ['rip.changed', 'rip.progress'],
  erasures: ['erasure.changed'],
  workers: ['worker.changed'],
  derivations: ['derivation.changed'],
  library: ['artifact.changed'],
};

/** What the stream sends when a client may have missed something. */
export const RESET = 'stream.reset';

/** How often a page with a working stream reconciles anyway. */
export const RECONCILE_MS = 60_000;

/** How long events are gathered before one refresh. */
export const DEBOUNCE_MS = 300;

/** One event: its type, and its data (its topic, identifiers and state). */
export interface StreamEvent {
  type: string;
  data: Record<string, unknown>;
}

/** The part of EventSource a watcher uses, so a test can stand one in. */
export interface EventSourceLike {
  addEventListener(type: string, listener: (event: MessageEvent) => void): void;
  close(): void;
}

export interface WatchOptions {
  /** Only events this accepts cause a refresh; a reset always does. */
  relevant?: (event: StreamEvent) => boolean;
  /** How often to refresh while the stream is open. */
  reconcileMs?: number;
  /** How often to refresh where there is no stream. */
  fallbackMs?: number;
  debounceMs?: number;
  /** Opens the stream; the browser's EventSource unless given, `null` for none. */
  open?: ((url: string) => EventSourceLike) | null;
}

function browserSource(): ((url: string) => EventSourceLike) | null {
  return typeof EventSource === 'undefined' ? null : (url) => new EventSource(url);
}

/** An event's data, or an empty object if it is not the JSON object expected. */
export function eventData(raw: unknown): Record<string, unknown> {
  if (typeof raw !== 'string') return {};
  try {
    const parsed: unknown = JSON.parse(raw);
    return parsed && typeof parsed === 'object' && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : {};
  } catch {
    return {};
  }
}

/** Whether an event names `id` as itself or as what it belongs to. */
export function names(event: StreamEvent, id: string, ...keys: string[]): boolean {
  return ['id', ...keys].some((key) => event.data[key] === id);
}

/** Why a watcher refreshes: an event arrived, or the timer came round. */
export type RefreshReason = 'event' | 'timer';

/**
 * Refresh when something under `topics` changes, and on a timer regardless.
 *
 * Returns a function that stops watching.
 */
export function watch(
  topics: Topic[],
  refresh: (reason: RefreshReason) => unknown,
  options: WatchOptions = {},
): () => void {
  const open = options.open === undefined ? browserSource() : options.open;
  const debounceMs = options.debounceMs ?? DEBOUNCE_MS;
  let stopped = false;
  let soon: ReturnType<typeof setTimeout> | undefined;

  const later = () => {
    if (stopped || soon !== undefined) return;
    soon = setTimeout(() => {
      soon = undefined;
      if (!stopped) void refresh('event');
    }, debounceMs);
  };

  let source: EventSourceLike | null = null;
  if (open) {
    source = open(`/api/v1/events?topics=${topics.join(',')}`);
    for (const type of topics.flatMap((topic) => EVENT_TYPES[topic])) {
      source.addEventListener(type, (message) => {
        const event = { type, data: eventData(message.data) };
        if (!options.relevant || options.relevant(event)) later();
      });
    }
    source.addEventListener(RESET, later);
  }

  const every = source
    ? (options.reconcileMs ?? RECONCILE_MS)
    : (options.fallbackMs ?? options.reconcileMs ?? RECONCILE_MS);
  const timer = setInterval(() => {
    if (!stopped) void refresh('timer');
  }, every);

  return () => {
    stopped = true;
    clearInterval(timer);
    clearTimeout(soon);
    source?.close();
  };
}
