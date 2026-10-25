// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// Integrations, for administrators (ADR 0016): download clients and the
// settings they need. A password is sent and never comes back; a view says
// only whether one is set.

import type { components } from './api-types';
import { apiCall, apiFetch, type Problem, type Result } from './session';

export type IntegrationView = components['schemas']['IntegrationView'];
export type IntegrationList = components['schemas']['IntegrationList'];
export type CreateIntegrationRequest = components['schemas']['CreateIntegrationRequest'];
export type UpdateIntegrationRequest = components['schemas']['UpdateIntegrationRequest'];
export type IntegrationTestView = components['schemas']['IntegrationTestView'];

/** Every integration, and whether the server can store a password. */
export function loadIntegrations(
  fetcher: typeof fetch = apiFetch,
): Promise<Result<IntegrationList>> {
  return apiCall(fetcher, '/api/v1/integrations');
}

/** Make an integration. */
export function createIntegration(
  request: CreateIntegrationRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<IntegrationView>> {
  return apiCall(fetcher, '/api/v1/integrations', 'POST', request);
}

/** Change an integration; what is left out stays as it is. */
export function updateIntegration(
  id: string,
  request: UpdateIntegrationRequest,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<IntegrationView>> {
  return apiCall(fetcher, `/api/v1/integrations/${encodeURIComponent(id)}`, 'PATCH', request);
}

/** Remove an integration and its password. */
export function deleteIntegration(
  id: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<undefined>> {
  return apiCall(fetcher, `/api/v1/integrations/${encodeURIComponent(id)}`, 'DELETE');
}

/** Try an integration: sign in and ask the client's API version. */
export function testIntegration(
  id: string,
  fetcher: typeof fetch = apiFetch,
): Promise<Result<IntegrationTestView>> {
  return apiCall(fetcher, `/api/v1/integrations/${encodeURIComponent(id)}/test`, 'POST');
}

/** How trying an integration went, in words. */
export function testOutcome(name: string, test: IntegrationTestView): string {
  if (test.ok) {
    return `${name} signed in${test.api_version ? `; WebUI API ${test.api_version}` : ''}.`;
  }
  return `${name} did not work: ${test.detail ?? test.error_code ?? 'no reason given'}`;
}

/** A kind in words. */
export function kindLabel(kind: string): string {
  switch (kind) {
    case 'qbittorrent':
      return 'qBittorrent';
    case 'romm':
      return 'RomM export';
    case 'sabnzbd':
      return 'SABnzbd';
    case 'nzbget':
      return 'NZBGet';
    case 's3':
      return 'S3-compatible storage';
    default:
      return kind;
  }
}

/** A status in words: never colour alone. */
export function integrationStatusLabel(view: Pick<IntegrationView, 'status' | 'enabled'>): string {
  if (!view.enabled) return 'Turned off';
  switch (view.status) {
    case 'ok':
      return 'Working';
    case 'degraded':
      return 'Working, with problems';
    case 'failing':
      return 'Failing';
    case 'disabled':
      return 'Turned off';
    case 'unconfigured':
      return 'Not tried yet';
    default:
      return view.status;
  }
}

/** Whether the password, or each secret, is set, in words. */
export function secretsLabel(view: Pick<IntegrationView, 'secrets'>): string {
  if (view.secrets.length === 0) return 'None needed';
  return view.secrets
    .map(({ field, set }) => `${fieldLabel(field)} ${set ? 'set' : 'not set'}`)
    .join(', ');
}

function fieldLabel(field: string): string {
  const words = field.replace(/_/g, ' ');
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** What the qBittorrent form holds. The password is never filled in from the server. */
export type QbittorrentForm = {
  name: string;
  enabled: boolean;
  baseUrl: string;
  username: string;
  password: string;
  /** Editing only: remove the stored password. */
  clearPassword: boolean;
  category: string;
  clientPath: string;
  localRoot: string;
};

/** An empty form, with the server's defaults. */
export function emptyQbittorrentForm(roots: string[]): QbittorrentForm {
  return {
    name: '',
    enabled: true,
    baseUrl: '',
    username: '',
    password: '',
    clearPassword: false,
    category: 'tangible',
    clientPath: '',
    localRoot: roots.length === 1 ? (roots[0] ?? '') : '',
  };
}

function text(settings: Record<string, unknown>, key: string): string {
  const value = settings[key];
  return typeof value === 'string' ? value : '';
}

/** A form filled from an integration, to edit it. The password stays empty. */
export function qbittorrentFormFrom(view: IntegrationView): QbittorrentForm {
  return {
    name: view.name,
    enabled: view.enabled,
    baseUrl: text(view.settings, 'base_url'),
    username: text(view.settings, 'username'),
    password: '',
    clearPassword: false,
    category: text(view.settings, 'category') || 'tangible',
    clientPath: text(view.settings, 'client_path'),
    localRoot: text(view.settings, 'local_root'),
  };
}

function qbittorrentSettings(form: QbittorrentForm): Record<string, unknown> {
  return {
    base_url: form.baseUrl.trim(),
    username: form.username.trim(),
    category: form.category.trim() || 'tangible',
    client_path: form.clientPath.trim(),
    local_root: form.localRoot,
  };
}

/** The request that makes a qBittorrent integration from the form. */
export function createQbittorrentRequest(form: QbittorrentForm): CreateIntegrationRequest {
  const request: CreateIntegrationRequest = {
    kind: 'qbittorrent',
    name: form.name.trim(),
    enabled: form.enabled,
    settings: qbittorrentSettings(form),
  };
  if (form.password !== '') request.secrets = { password: form.password };
  return request;
}

/**
 * The request that changes a qBittorrent integration. An empty password
 * keeps the stored one; asking to clear it sends `null`.
 */
export function updateQbittorrentRequest(form: QbittorrentForm): UpdateIntegrationRequest {
  const request: UpdateIntegrationRequest = {
    name: form.name.trim(),
    enabled: form.enabled,
    settings: qbittorrentSettings(form),
  };
  if (form.clearPassword) request.secrets = { password: null };
  else if (form.password !== '') request.secrets = { password: form.password };
  return request;
}

/** Why a change was refused, in words an administrator can act on. */
export function integrationRefusal(problem: Problem): string {
  if (problem.code === 'SECRET_KEY_MISSING') {
    return (
      'This server holds no secret key, so it cannot store a password. Set ' +
      'TANGIBLE_SECRET_KEY_FILE and restart it (deploy/README.md, "Integration secrets"), ' +
      'or save this without a password for now.'
    );
  }
  return problem.detail;
}
