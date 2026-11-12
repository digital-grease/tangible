// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { SharedStream, TOPICS, eventData, names, watch, type EventSourceLike } from './events';

/** A stand-in stream that a test sends events through. */
class FakeSource implements EventSourceLike {
  listeners = new Map<string, ((event: MessageEvent) => void)[]>();
  closed = false;
  constructor(public url: string) {}
  addEventListener(type: string, listener: (event: MessageEvent) => void) {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }
  close() {
    this.closed = true;
  }
  send(type: string, data: unknown) {
    for (const listener of this.listeners.get(type) ?? []) {
      listener({ data: JSON.stringify(data) } as MessageEvent);
    }
  }
}

describe('watching the event stream', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('opens the stream for its topics and refreshes once for a burst of events', async () => {
    let source: FakeSource | undefined;
    const refresh = vi.fn();
    const stop = watch(['burns', 'imports'], refresh, {
      open: (url) => (source = new FakeSource(url)),
    });
    expect(source?.url).toBe('/api/v1/events?topics=burns,imports');

    source?.send('burn.progress', { burn_job_id: 'j1', progress: 0.1 });
    source?.send('burn.progress', { burn_job_id: 'j1', progress: 0.2 });
    source?.send('import.changed', { id: 'i1', state: 'complete' });
    expect(refresh).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(300);
    expect(refresh).toHaveBeenCalledTimes(1);
    expect(refresh).toHaveBeenCalledWith('event');

    stop();
    expect(source?.closed).toBe(true);
    source?.send('burn.progress', {});
    await vi.advanceTimersByTimeAsync(120_000);
    expect(refresh).toHaveBeenCalledTimes(1);
  });

  it('ignores events the page does not show, but never a reset', async () => {
    let source: FakeSource | undefined;
    const refresh = vi.fn();
    watch(['burns'], refresh, {
      open: (url) => (source = new FakeSource(url)),
      relevant: (event) => names(event, 'j1', 'burn_job_id'),
    });
    source?.send('burn_job.changed', { id: 'j2', state: 'queued' });
    await vi.advanceTimersByTimeAsync(300);
    expect(refresh).not.toHaveBeenCalled();

    source?.send('burn_attempt.changed', { id: 'a1', burn_job_id: 'j1', state: 'writing' });
    await vi.advanceTimersByTimeAsync(300);
    expect(refresh).toHaveBeenCalledTimes(1);

    source?.send('stream.reset', { reason: 'fell behind' });
    await vi.advanceTimersByTimeAsync(300);
    expect(refresh).toHaveBeenCalledTimes(2);
  });

  it('reconciles slowly with a stream, and polls at the old rate without one', async () => {
    const withStream = vi.fn();
    watch(['rips'], withStream, { open: (url) => new FakeSource(url), fallbackMs: 3_000 });
    await vi.advanceTimersByTimeAsync(59_000);
    expect(withStream).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1_000);
    expect(withStream).toHaveBeenCalledTimes(1);
    expect(withStream).toHaveBeenCalledWith('timer');

    const without = vi.fn();
    watch(['rips'], without, { open: null, fallbackMs: 3_000 });
    await vi.advanceTimersByTimeAsync(9_000);
    expect(without).toHaveBeenCalledTimes(3);
  });
});

describe('event data', () => {
  it('reads a JSON object and nothing else', () => {
    expect(eventData('{"id":"x"}')).toEqual({ id: 'x' });
    expect(eventData('[1]')).toEqual({});
    expect(eventData('not json')).toEqual({});
    expect(eventData(undefined)).toEqual({});
  });

  it('matches an event by its own id or one it belongs to', () => {
    const event = { type: 'burn.progress', data: { attempt_id: 'a', burn_job_id: 'j' } };
    expect(names(event, 'j', 'burn_job_id')).toBe(true);
    expect(names(event, 'a', 'burn_job_id')).toBe(false);
    expect(names({ type: 'rip.changed', data: { id: 'r' } }, 'r')).toBe(true);
  });
});

describe('one stream for the tab', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('opens once for every topic, however many watch, and closes when the last stops', async () => {
    const opened: FakeSource[] = [];
    const stream = new SharedStream((url) => {
      const source = new FakeSource(url);
      opened.push(source);
      return source;
    });
    const burns = vi.fn();
    const imports = vi.fn();
    const stopBurns = watch(['burns'], burns, { stream });
    const stopImports = watch(['imports'], imports, { stream });

    expect(opened).toHaveLength(1);
    expect(opened[0]?.url).toBe(`/api/v1/events?topics=${TOPICS.join(',')}`);

    // Each watcher hears only its own topics.
    opened[0]?.send('burn.progress', { burn_job_id: 'j1', progress: 0.5 });
    await vi.advanceTimersByTimeAsync(300);
    expect(burns).toHaveBeenCalledTimes(1);
    expect(imports).not.toHaveBeenCalled();

    // A reset reaches everyone.
    opened[0]?.send('stream.reset', { reason: 'fell behind' });
    await vi.advanceTimersByTimeAsync(300);
    expect(burns).toHaveBeenCalledTimes(2);
    expect(imports).toHaveBeenCalledTimes(1);

    stopBurns();
    expect(opened[0]?.closed).toBe(false);
    expect(stream.isOpen).toBe(true);
    stopImports();
    expect(opened[0]?.closed).toBe(true);
    expect(stream.isOpen).toBe(false);

    // Watching again opens a new stream.
    const stop = watch(['rips'], vi.fn(), { stream });
    expect(opened).toHaveLength(2);
    stop();
  });

  it("keeps a watcher's filter on the shared stream", async () => {
    let source: FakeSource | undefined;
    const stream = new SharedStream((url) => (source = new FakeSource(url)));
    const refresh = vi.fn();
    watch(['burns'], refresh, {
      stream,
      relevant: (event) => names(event, 'j1', 'burn_job_id'),
    });
    source?.send('burn_job.changed', { id: 'j2', state: 'queued' });
    await vi.advanceTimersByTimeAsync(300);
    expect(refresh).not.toHaveBeenCalled();
    source?.send('burn_job.changed', { id: 'j1', state: 'queued' });
    await vi.advanceTimersByTimeAsync(300);
    expect(refresh).toHaveBeenCalledTimes(1);
  });

  it('polls at the old rate where the browser has no stream', async () => {
    const stream = new SharedStream(null);
    const refresh = vi.fn();
    watch(['burns'], refresh, { stream, fallbackMs: 3_000 });
    expect(stream.isOpen).toBe(false);
    await vi.advanceTimersByTimeAsync(3_000);
    expect(refresh).toHaveBeenCalledWith('timer');
  });
});
