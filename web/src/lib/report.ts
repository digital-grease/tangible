// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// Issue reports, on the browser's side.
//
// The server prepares a report's fields, already scrubbed of credentials,
// paths, serial numbers and library file names, and already in the exact
// form the issue template expects. This module turns them into a link to a
// prefilled issue form, shortening a long log to fit, and into text to copy.
// Nothing here sends anything: the operator reads the report and opens the
// link themselves.

import type { components } from './api-types';
import { apiCall, apiFetch, type Problem, type Result } from './session';

export type ReportSettings = components['schemas']['ReportSettingsView'];

/** Whether this server offers issue reports, where they go, and its build. */
export function loadReportSettings(
  fetcher: typeof fetch = apiFetch,
): Promise<Result<ReportSettings>> {
  return apiCall(fetcher, '/api/v1/report-settings');
}

let settingsOnce: Promise<ReportSettings | null> | null = null;

/**
 * The report settings, asked for once per page load and shared by every
 * error panel on it. `null` when they could not be loaded, which offers no
 * reports; a failure is not remembered, so a later panel asks again.
 */
export function reportSettingsOnce(
  fetcher: typeof fetch = apiFetch,
): Promise<ReportSettings | null> {
  if (settingsOnce === null) {
    const asking = loadReportSettings(fetcher).then((result) =>
      result.kind === 'ok' ? result.data : null,
    );
    settingsOnce = asking;
    void asking.then((settings) => {
      if (settings === null && settingsOnce === asking) settingsOnce = null;
    });
  }
  return settingsOnce;
}

/** Forget the shared settings. For tests. */
export function forgetReportSettings(): void {
  settingsOnce = null;
}

/**
 * Codes that say the request was refused for a reason of its own, not that
 * the software went wrong. Reporting one of these as a bug would only waste
 * the reader's time.
 */
const NOT_DEFECTS = new Set([
  'VALIDATION_FAILED',
  'INVALID_PARAMETER',
  'INVALID_CURSOR',
  'NOT_FOUND',
  'REFERENCE_NOT_FOUND',
  'FORBIDDEN',
  'UNAUTHENTICATED',
  'CSRF_TOKEN_INVALID',
  'RATE_LIMITED',
  'CONFLICT',
  'IDEMPOTENCY_CONFLICT',
  'WRITE_IN_PROGRESS',
  'NOT_RETRYABLE',
  'TRANSPORT_FAILURE',
]);

/** Codes that say the software itself went wrong. */
const DEFECTS = new Set([
  'INTERNAL',
  'STORAGE_UNAVAILABLE',
  'MANIFEST_INVALID',
  'UNSUPPORTED_PROTOCOL',
]);

/**
 * Whether an error with this code is worth offering as a bug report: one of
 * the codes that mean the software went wrong, or a failure code such as
 * `IMPORT_STORAGE_FAILED`. Anything about the request itself is not.
 */
export function suggestsDefect(code: string): boolean {
  if (NOT_DEFECTS.has(code)) return false;
  return DEFECTS.has(code) || code.endsWith('_FAILED');
}

/** The bug form's Area options. */
export type ReportArea =
  'import' | 'library' | 'catalog' | 'romm' | 'web' | 'api' | 'deployment' | 'other';

const UUID_SEGMENT = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * A request path as its route, with identifiers replaced by `{id}` and the
 * query dropped: `/api/v1/burn-jobs/0190…/retry?x=1` is
 * `/api/v1/burn-jobs/{id}/retry`. The identifiers say nothing to a reader
 * and are the operator's own.
 */
export function routeTemplate(path: string): string {
  const bare = path.split(/[?#]/, 1)[0] ?? '';
  return bare
    .split('/')
    .map((segment) => (UUID_SEGMENT.test(segment) ? '{id}' : segment))
    .join('/');
}

export type ErrorReportRequest = components['schemas']['ErrorReportRequest'];

/** What an error panel sends: the problem as shown, where, and in which part. */
export function errorReportRequest(
  problem: Problem,
  area: ReportArea,
  endpoint?: string,
): ErrorReportRequest {
  const route = endpoint ?? (problem.instance ? routeTemplate(problem.instance) : undefined);
  // Within the server's limits, so a long detail still makes a report.
  return {
    code: problem.code,
    title: clip(problem.title, 200),
    detail: clip(problem.detail, 4000),
    status: problem.status > 0 ? problem.status : null,
    request_id: problem.request_id ?? null,
    endpoint: route ? clip(route, 300) : null,
    area,
  };
}

/** At most `max` characters, counted as the server counts them. */
function clip(text: string, max: number): string {
  const chars = Array.from(text);
  return chars.length <= max ? text : chars.slice(0, max).join('');
}

/** An error the page showed, prepared by the server as a bug report, scrubbed. */
export function prepareErrorReport(
  request: ErrorReportRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<IssueReport>> {
  return apiCall(fetcher, '/api/v1/error-reports', 'POST', request);
}

export type IssueReport = components['schemas']['IssueReport'];

/** Which report a burn attempt becomes: a problem, or hardware that worked. */
export type BurnReportKind = 'problem' | 'worked';

/** A burn attempt, prepared by the server as a report, scrubbed. */
export function loadBurnReport(
  attemptId: string,
  kind: BurnReportKind,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<IssueReport>> {
  return apiCall(
    fetcher,
    `/api/v1/burn-attempts/${encodeURIComponent(attemptId)}/report?kind=${kind}`,
  );
}

/**
 * Below this detection confidence an image counts as read with doubts. The
 * server applies the same rule and refuses a format report otherwise.
 */
export const DOUBTFUL_CONFIDENCE = 0.9;

/** Whether to offer a format report: not recognised, doubtful, or warned. */
export function formatReportOffered(artifact: {
  format: string;
  format_confidence: number;
  validation_state: string;
}): boolean {
  return (
    artifact.format === 'unknown' ||
    artifact.format_confidence < DOUBTFUL_CONFIDENCE ||
    artifact.validation_state !== 'valid'
  );
}

/** An artifact's structure, prepared by the server as a format support request. */
export function loadFormatReport(
  artifactId: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<IssueReport>> {
  return apiCall(fetcher, `/api/v1/artifacts/${encodeURIComponent(artifactId)}/format-report`);
}

/** The server's report as the review dialog takes it. */
export function draftFromReport(report: IssueReport): ReportDraft {
  return {
    template: report.template,
    title: report.title ?? undefined,
    fields: report.fields.map((field) => ({ ...field })),
    longField: report.long_field ?? undefined,
    removed: { ...report.removed },
    truncated: report.truncated,
  };
}

/** One field of an issue form, as it will be filled in. */
export interface ReportField {
  /** The template's field id, which the prefilled link names. */
  id: string;
  /** What the form calls it, for the review screen. */
  label: string;
  /** Its text, already scrubbed. Empty fields are left out of the link. */
  value: string;
}

/** A report ready for the operator to review. */
export interface ReportDraft {
  /** The issue template's file name, such as `burn_report.yml`. */
  template: string;
  /** A title to suggest, if any; the template supplies a prefix. */
  title?: string;
  fields: ReportField[];
  /** The field that may be shortened to fit the link: the log. */
  longField?: string;
  /** What the scrubber replaced, by kind, such as `{ path: 2 }`. */
  removed: Record<string, number>;
  /** Whether the scrubber cut the text short. */
  truncated: boolean;
}

/**
 * The longest link to open. GitHub refuses much longer ones, and browsers
 * differ; fauxx settled on the same figure.
 */
export const MAX_ISSUE_URL_LENGTH = 7000;

/** What replaces the start of a log that was shortened to fit the link. */
export const SHORTENED_MARKER =
  '[Earlier lines were removed to fit the link. The full report was copied to your clipboard: paste the log here in place of this line.]\n';

export type IssueLink =
  | { kind: 'embedded'; url: string }
  | {
      kind: 'shortened';
      url: string;
      /** The field that was shortened, with its full text. */
      field: ReportField;
    };

function param(name: string, value: string): string {
  return `&${encodeURIComponent(name)}=${encodeURIComponent(value)}`;
}

/**
 * A link to the issue form, filled in with the draft.
 *
 * Every field goes in whole except the long one, which keeps its end (a
 * failure is usually last), starting on a line boundary where it can, behind
 * [`SHORTENED_MARKER`], so the link stays within `maxLength`.
 */
export function issueLink(
  issuesUrl: string,
  draft: ReportDraft,
  maxLength = MAX_ISSUE_URL_LENGTH,
): IssueLink {
  let base = `${issuesUrl}?template=${encodeURIComponent(draft.template)}`;
  if (draft.title) base += param('title', draft.title);
  const long = draft.fields.find((field) => field.id === draft.longField && field.value);
  for (const field of draft.fields) {
    if (field !== long && field.value) base += param(field.id, field.value);
  }
  if (!long) return { kind: 'embedded', url: base };

  const whole = param(long.id, long.value);
  if (base.length + whole.length <= maxLength) {
    return { kind: 'embedded', url: base + whole };
  }
  const tail = fittingTail(long.value, maxLength - base.length - param(long.id, '').length);
  const url = tail === null ? base : base + param(long.id, SHORTENED_MARKER + tail);
  return { kind: 'shortened', url, field: long };
}

/** The encoded length of a shortened value starting at `start`. */
function shortenedLength(value: string, start: number): number {
  return encodeURIComponent(SHORTENED_MARKER + value.slice(start)).length;
}

/**
 * The longest end of `value` that fits `budget` encoded characters behind
 * the marker: from a line start if any fits, else from mid-line. `null` when
 * not even the marker fits.
 */
function fittingTail(value: string, budget: number): string | null {
  if (encodeURIComponent(SHORTENED_MARKER).length > budget) return null;

  // Each start is later than the last, so the encoded length only falls:
  // binary search for the earliest that fits.
  const earliest = (starts: number[]): number | null => {
    let low = 0;
    let high = starts.length - 1;
    let found: number | null = null;
    while (low <= high) {
      const middle = (low + high) >> 1;
      const start = starts[middle];
      if (start === undefined) break;
      if (shortenedLength(value, start) <= budget) {
        found = start;
        high = middle - 1;
      } else {
        low = middle + 1;
      }
    }
    return found;
  };

  const lineStarts: number[] = [];
  for (let i = 1; i < value.length; i++) {
    if (value[i - 1] === '\n') lineStarts.push(i);
  }
  const fromLine = earliest(lineStarts);
  if (fromLine !== null) return value.slice(fromLine);

  const charStarts: number[] = [];
  for (let i = 1; i <= value.length; i++) {
    // Never start inside a surrogate pair.
    const code = value.charCodeAt(i);
    if (!(code >= 0xdc00 && code <= 0xdfff)) charStarts.push(i);
  }
  const fromChar = earliest(charStarts);
  return fromChar === null ? '' : value.slice(fromChar);
}

/** A fence that no run of backticks in `text` can close early. */
function fenceFor(text: string): string {
  const longest = Math.max(2, ...(text.match(/`+/g) ?? []).map((run) => run.length));
  return '`'.repeat(longest + 1);
}

/**
 * The whole report as Markdown, for the clipboard: the full log, unshortened,
 * so it can be pasted into the form, or into an issue written by hand.
 */
export function reportMarkdown(draft: ReportDraft): string {
  const sections = draft.fields
    .filter((field) => field.value)
    .map((field) => {
      const multiline = field.id === draft.longField || field.value.includes('\n');
      if (!multiline) return `### ${field.label}\n\n${field.value}`;
      const fence = fenceFor(field.value);
      return `### ${field.label}\n\n${fence}text\n${field.value.replace(/\n$/, '')}\n${fence}`;
    });
  return `${sections.join('\n\n')}\n`;
}

const NOUNS: Record<string, [string, string]> = {
  credential: ['credential', 'credentials'],
  header_value: ['token or header value', 'tokens or header values'],
  url_credentials: ['password in an address', 'passwords in addresses'],
  path: ['path', 'paths'],
  name: ['file name', 'file names'],
  serial: ['serial number', 'serial numbers'],
  address: ['IP address', 'IP addresses'],
  host: ['host name', 'host names'],
};

/** What was removed, in words: "2 paths, 1 credential and 1 serial number". */
export function removedSummary(removed: Record<string, number>): string {
  const parts = Object.entries(removed)
    .filter(([, count]) => count > 0)
    .map(([kind, count]) => {
      const [one, many] = NOUNS[kind] ?? [kind.replace(/_/g, ' '), `${kind.replace(/_/g, ' ')}s`];
      return `${count} ${count === 1 ? one : many}`;
    });
  if (parts.length === 0) return 'Nothing needed removing.';
  const list =
    parts.length === 1
      ? parts[0]
      : `${parts.slice(0, -1).join(', ')} and ${parts[parts.length - 1]}`;
  return `Replaced before you saw it: ${list}.`;
}

/** The host a link opens, for a button that says where it goes. */
export function issueHost(issuesUrl: string): string {
  try {
    return new URL(issuesUrl).host;
  } catch {
    return issuesUrl;
  }
}

/** What copying needs from the browser, injectable for tests. */
export interface CopyEnvironment {
  clipboard?: { writeText(text: string): Promise<void> };
  isSecureContext: boolean;
  document?: Document;
}

function browserEnvironment(): CopyEnvironment {
  return {
    clipboard: typeof navigator === 'undefined' ? undefined : navigator.clipboard,
    isSecureContext: typeof window !== 'undefined' && window.isSecureContext,
    document: typeof document === 'undefined' ? undefined : document,
  };
}

/**
 * Copy `text` to the clipboard. Returns whether it worked.
 *
 * The clipboard API needs a secure context, and many Tangible servers are
 * reached over plain HTTP on a home network, so without one this falls back
 * to selecting the text in a hidden field and asking the browser to copy it.
 */
export async function copyText(
  text: string,
  environment: CopyEnvironment = browserEnvironment(),
): Promise<boolean> {
  if (environment.clipboard && environment.isSecureContext) {
    try {
      await environment.clipboard.writeText(text);
      return true;
    } catch {
      // Refused (no permission, not focused): try the fallback.
    }
  }
  const doc = environment.document;
  if (!doc?.body) return false;
  const field = doc.createElement('textarea');
  field.value = text;
  field.setAttribute('readonly', '');
  field.style.position = 'fixed';
  field.style.opacity = '0';
  doc.body.appendChild(field);
  try {
    field.select();
    return doc.execCommand('copy');
  } catch {
    return false;
  } finally {
    field.remove();
  }
}
