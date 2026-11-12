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
 *
 * A tab holds one stream, shared by everything watching
 * (docs/plans/ui-overhaul.md, decision 6): the drive bay and the sidebar's
 * counts are on every page, and a stream each would soon reach the server's
 * limit of 16 for one account. It opens for every topic when the first
 * watcher starts and closes when the last one stops.
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

/** Every topic: the shared stream carries them all. */
export const TOPICS = Object.keys(EVENT_TYPES) as Topic[];

type Listener = (event: StreamEvent) => void;

/**
 * One stream for a tab, shared by every watcher in it.
 *
 * Each subscriber names the event types it wants; a reset goes to everyone.
 */
export class SharedStream {
  private source: EventSourceLike | null = null;
  private readonly subscribers = new Map<
    symbol,
    { types: ReadonlySet<string>; listener: Listener }
  >();

  /**
   * `open` makes the stream; left out, it is the browser's EventSource, looked
   * for each time the stream connects rather than once, so a page that
   * gains one later (a test's stand-in) is not left polling.
   */
  constructor(private readonly given?: ((url: string) => EventSourceLike) | null) {}

  private get open(): ((url: string) => EventSourceLike) | null {
    return this.given === undefined ? browserSource() : this.given;
  }

  /** Whether there is a stream at all, or only polling. */
  get available(): boolean {
    return this.open !== null;
  }

  /** Whether the stream is open now: while anyone is subscribed. */
  get isOpen(): boolean {
    return this.source !== null;
  }

  /** Receive events of `types`, and every reset. Returns the way to stop. */
  subscribe(types: Iterable<string>, listener: Listener): () => void {
    const key = Symbol('subscriber');
    this.subscribers.set(key, { types: new Set(types), listener });
    this.connect();
    return () => {
      this.subscribers.delete(key);
      if (this.subscribers.size === 0) {
        this.source?.close();
        this.source = null;
      }
    };
  }

  private connect() {
    const open = this.open;
    if (this.source || !open) return;
    const source = open(`/api/v1/events?topics=${TOPICS.join(',')}`);
    for (const type of [...TOPICS.flatMap((topic) => EVENT_TYPES[topic]), RESET]) {
      source.addEventListener(type, (message) => {
        const event = { type, data: eventData(message.data) };
        for (const { types, listener } of [...this.subscribers.values()]) {
          if (type === RESET || types.has(type)) listener(event);
        }
      });
    }
    this.source = source;
  }
}

let tabStream: SharedStream | null = null;

/** This tab's stream, made when first asked for. */
export function sharedStream(): SharedStream {
  tabStream ??= new SharedStream();
  return tabStream;
}

export interface WatchOptions {
  /** Only events this accepts cause a refresh; a reset always does. */
  relevant?: (event: StreamEvent) => boolean;
  /** How often to refresh while the stream is open. */
  reconcileMs?: number;
  /** How often to refresh where there is no stream. */
  fallbackMs?: number;
  debounceMs?: number;
  /**
   * Opens a stream of the watcher's own, rather than sharing the tab's: for
   * a test, or `null` for none.
   */
  open?: ((url: string) => EventSourceLike) | null;
  /** The shared stream to use; the tab's unless given. */
  stream?: SharedStream;
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

  const types = topics.flatMap((topic) => EVENT_TYPES[topic]);
  const onEvent: Listener = (event) => {
    if (event.type === RESET || !options.relevant || options.relevant(event)) later();
  };

  let streaming = false;
  let unsubscribe: (() => void) | undefined;
  if (options.open !== undefined) {
    // A stream of the watcher's own, as tests ask for.
    if (options.open) {
      const source = options.open(`/api/v1/events?topics=${topics.join(',')}`);
      for (const type of types) {
        source.addEventListener(type, (message) =>
          onEvent({ type, data: eventData(message.data) }),
        );
      }
      source.addEventListener(RESET, later);
      streaming = true;
      unsubscribe = () => source.close();
    }
  } else {
    const stream = options.stream ?? sharedStream();
    if (stream.available) {
      unsubscribe = stream.subscribe(types, onEvent);
      streaming = true;
    }
  }

  const every = streaming
    ? (options.reconcileMs ?? RECONCILE_MS)
    : (options.fallbackMs ?? options.reconcileMs ?? RECONCILE_MS);
  const timer = setInterval(() => {
    if (!stopped) void refresh('timer');
  }, every);

  return () => {
    stopped = true;
    clearInterval(timer);
    clearTimeout(soon);
    unsubscribe?.();
  };
}
