// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// Integrations, for administrators (ADR 0016): download clients (qBittorrent,
// SABnzbd, NZBGet) and the settings they need. A password or API key is sent
// and never comes back; a view says only whether one is set.

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

/** What a client's reported version is a version of. */
export function versionLabel(kind: string): string {
  return kind === 'qbittorrent' ? 'WebUI API' : 'version';
}

/** How trying an integration went, in words. */
export function testOutcome(name: string, test: IntegrationTestView, kind = 'qbittorrent'): string {
  if (test.ok) {
    return `${name} signed in${test.api_version ? `; ${versionLabel(kind)} ${test.api_version}` : ''}.`;
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

/** The integrations an administrator can set up here: download clients, and S3. */
export type ClientKind = 'qbittorrent' | 'sabnzbd' | 'nzbget' | 's3';

/** The kinds, in the order offered. */
export const CLIENT_KINDS: ClientKind[] = ['qbittorrent', 'sabnzbd', 'nzbget', 's3'];

/** Whether an integration's kind has a form here. */
export function isClientKind(kind: string): kind is ClientKind {
  return (CLIENT_KINDS as string[]).includes(kind);
}

/** What differs between the kinds' forms. */
export type ClientWording = {
  /** The secret's field on the server. */
  secretField: 'password' | 'api_key' | 'secret_access_key';
  /** Whether it is a download client, with a category and folders. */
  downloads: boolean;
  /** The secret's label. */
  secretLabel: string;
  /** Whether it takes a user name, and the label. */
  usernameLabel: string | null;
  /** Its address's label. */
  addressLabel: string;
  /** An example address. */
  addressExample: string;
  /** An example finished-download folder, as the client sees it. */
  folderExample: string;
  /** What a job is handed over as. */
  takes: string;
};

/** How a kind's form reads. */
export function clientWording(kind: ClientKind): ClientWording {
  switch (kind) {
    case 'qbittorrent':
      return {
        downloads: true,
        secretField: 'password',
        secretLabel: 'WebUI password',
        usernameLabel: 'WebUI user',
        addressLabel: 'WebUI address',
        addressExample: 'http://192.168.1.13:8080/',
        folderExample: '/downloads/tangible',
        takes: 'a .torrent file or a magnet link',
      };
    case 'sabnzbd':
      return {
        downloads: true,
        secretField: 'api_key',
        secretLabel: 'API key',
        usernameLabel: null,
        addressLabel: 'SABnzbd address',
        addressExample: 'http://192.168.1.13:8080/',
        folderExample: '/downloads/complete/tangible',
        takes: 'an NZB',
      };
    case 'nzbget':
      return {
        downloads: true,
        secretField: 'password',
        secretLabel: 'Control password',
        usernameLabel: 'Control user',
        addressLabel: 'NZBGet address',
        addressExample: 'http://192.168.1.13:6789/',
        folderExample: '/downloads/completed/tangible',
        takes: 'an NZB',
      };
    case 's3':
      return {
        downloads: false,
        secretField: 'secret_access_key',
        secretLabel: 'Secret access key',
        usernameLabel: null,
        addressLabel: 'Endpoint',
        addressExample: 'https://s3.eu-west-1.amazonaws.com/',
        folderExample: '',
        takes: 'objects from the buckets it allows',
      };
  }
}

/** What the form holds. The secret is never filled in from the server. */
export type ClientForm = {
  kind: ClientKind;
  name: string;
  enabled: boolean;
  baseUrl: string;
  username: string;
  secret: string;
  /** Editing only: remove the stored secret. */
  clearSecret: boolean;
  category: string;
  clientPath: string;
  localRoot: string;
  /** S3 only: the access key id, a secret too, never filled in. */
  accessKey: string;
  /** S3 only. */
  region: string;
  /** S3 only: buckets in the path rather than as a subdomain. */
  pathStyle: boolean;
  /** S3 only: the buckets allowed, one a line or comma-separated. */
  buckets: string;
};

/** An empty form for `kind`, with the server's defaults. */
export function emptyClientForm(kind: ClientKind, roots: string[]): ClientForm {
  return {
    kind,
    name: '',
    enabled: true,
    baseUrl: '',
    username: '',
    secret: '',
    clearSecret: false,
    category: 'tangible',
    clientPath: '',
    localRoot: roots.length === 1 ? (roots[0] ?? '') : '',
    accessKey: '',
    region: 'us-east-1',
    pathStyle: true,
    buckets: '',
  };
}

function text(settings: Record<string, unknown>, key: string): string {
  const value = settings[key];
  return typeof value === 'string' ? value : '';
}

/** A form filled from an integration, to edit it. The secret stays empty. */
export function clientFormFrom(view: IntegrationView & { kind: ClientKind }): ClientForm {
  const buckets = view.settings.buckets;
  return {
    kind: view.kind,
    name: view.name,
    enabled: view.enabled,
    baseUrl: text(view.settings, view.kind === 's3' ? 'endpoint' : 'base_url'),
    username: text(view.settings, 'username'),
    secret: '',
    clearSecret: false,
    category: text(view.settings, 'category') || 'tangible',
    clientPath: text(view.settings, 'client_path'),
    localRoot: text(view.settings, 'local_root'),
    accessKey: '',
    region: text(view.settings, 'region') || 'us-east-1',
    pathStyle: view.settings.path_style !== false,
    buckets: Array.isArray(buckets)
      ? buckets.filter((bucket) => typeof bucket === 'string').join('\n')
      : '',
  };
}

/** The buckets typed, one a line or comma-separated, without blanks. */
export function parseBuckets(text: string): string[] {
  return text
    .split(/[\n,]/)
    .map((bucket) => bucket.trim())
    .filter((bucket) => bucket !== '');
}

function clientSettings(form: ClientForm): Record<string, unknown> {
  if (form.kind === 's3') {
    return {
      endpoint: form.baseUrl.trim(),
      region: form.region.trim() || 'us-east-1',
      path_style: form.pathStyle,
      buckets: parseBuckets(form.buckets),
    };
  }
  const settings: Record<string, unknown> = {
    base_url: form.baseUrl.trim(),
    category: form.category.trim() || 'tangible',
    client_path: form.clientPath.trim(),
    local_root: form.localRoot,
  };
  // SABnzbd takes no user, and refuses a settings field it does not know.
  if (clientWording(form.kind).usernameLabel !== null) settings.username = form.username.trim();
  return settings;
}

/** The request that makes an integration from the form. */
export function createClientRequest(form: ClientForm): CreateIntegrationRequest {
  const request: CreateIntegrationRequest = {
    kind: form.kind,
    name: form.name.trim(),
    enabled: form.enabled,
    settings: clientSettings(form),
  };
  const secrets = typedSecrets(form);
  if (Object.keys(secrets).length > 0) request.secrets = secrets;
  return request;
}

/** The secrets typed into the form, by their fields on the server. */
function typedSecrets(form: ClientForm): Record<string, string> {
  const secrets: Record<string, string> = {};
  if (form.secret !== '') secrets[clientWording(form.kind).secretField] = form.secret;
  if (form.kind === 's3' && form.accessKey !== '') secrets.access_key_id = form.accessKey;
  return secrets;
}

/**
 * The request that changes an integration. An empty secret keeps the stored
 * one; asking to clear it sends `null`.
 */
export function updateClientRequest(form: ClientForm): UpdateIntegrationRequest {
  const field = clientWording(form.kind).secretField;
  const request: UpdateIntegrationRequest = {
    name: form.name.trim(),
    enabled: form.enabled,
    settings: clientSettings(form),
  };
  if (form.clearSecret) {
    request.secrets =
      form.kind === 's3' ? { access_key_id: null, [field]: null } : { [field]: null };
  } else {
    const secrets = typedSecrets(form);
    if (Object.keys(secrets).length > 0) request.secrets = secrets;
  }
  return request;
}

/** Why a change was refused, in words an administrator can act on. */
export function integrationRefusal(problem: Problem): string {
  if (problem.code === 'SECRET_KEY_MISSING') {
    return (
      'This server holds no secret key, so it cannot store a password or API key. Set ' +
      'TANGIBLE_SECRET_KEY_FILE and restart it (deploy/README.md, "Integration secrets"), ' +
      'or save this without one for now.'
    );
  }
  return problem.detail;
}
