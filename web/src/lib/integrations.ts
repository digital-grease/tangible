// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// Integrations, for administrators (ADR 0016): download clients (qBittorrent,
// SABnzbd, NZBGet), S3-compatible storage and RomM's API, and the settings
// they need. A password, API key or token is sent and never comes back; a
// view says only whether one is set.

import type { components } from './api-types';
import type { Tone } from './components/ui/tone';
import { apiCall, apiFetch, type Problem, type Result } from './session';

export type IntegrationView = components['schemas']['IntegrationView'];
export type IntegrationList = components['schemas']['IntegrationList'];
export type CreateIntegrationRequest = components['schemas']['CreateIntegrationRequest'];
export type UpdateIntegrationRequest = components['schemas']['UpdateIntegrationRequest'];
export type IntegrationTestView = components['schemas']['IntegrationTestView'];
export type IntegrationCheckView = components['schemas']['IntegrationCheckView'];

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
  switch (kind) {
    case 'qbittorrent':
      return 'WebUI API';
    case 'romm_api':
      return 'RomM';
    default:
      return 'version';
  }
}

/** How trying an integration went, in words. */
export function testOutcome(name: string, test: IntegrationTestView, kind = 'qbittorrent'): string {
  if (kind === 'romm_api') {
    const version = test.api_version ? ` ${test.api_version}` : '';
    return test.ok
      ? `${name} answered as RomM${version}; exported games can be found.`
      : `${name} will not do: ${test.detail ?? test.error_code ?? 'no reason given'}`;
  }
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
    case 'romm_api':
      return "RomM's API";
    case 'sabnzbd':
      return 'SABnzbd';
    case 'nzbget':
      return 'NZBGet';
    case 's3':
      return 'S3-compatible storage';
    case 'musicbrainz':
      return 'MusicBrainz';
    case 'tmdb':
      return 'TMDB';
    case 'igdb':
      return 'IGDB';
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

/**
 * A status as a tone, beside its words. One turned off, or not tried yet,
 * is neither well nor ill.
 */
export function integrationStatusTone(view: Pick<IntegrationView, 'status' | 'enabled'>): Tone {
  if (!view.enabled) return 'idle';
  switch (view.status) {
    case 'ok':
      return 'ok';
    case 'degraded':
      return 'warn';
    case 'failing':
      return 'bad';
    default:
      return 'idle';
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

/** The integrations an administrator can set up here: download clients, S3, and RomM's API. */
export type ClientKind = 'qbittorrent' | 'sabnzbd' | 'nzbget' | 's3' | 'romm_api';

/** The kinds, in the order offered. */
export const CLIENT_KINDS: ClientKind[] = ['qbittorrent', 'sabnzbd', 'nzbget', 's3', 'romm_api'];

/** Whether an integration's kind has a form here. */
export function isClientKind(kind: string): kind is ClientKind {
  return (CLIENT_KINDS as string[]).includes(kind);
}

/** What differs between the kinds' forms. */
export type ClientWording = {
  /** The secret's field on the server. */
  secretField: 'password' | 'api_key' | 'secret_access_key' | 'token';
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
  /** What it is for, in a sentence. */
  purpose: string;
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
        purpose: 'Hands over a .torrent file or a magnet link.',
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
        purpose: 'Hands over an NZB.',
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
        purpose: 'Hands over an NZB.',
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
        purpose: 'Imports objects from the buckets it allows.',
      };
    case 'romm_api':
      return {
        downloads: false,
        secretField: 'token',
        secretLabel: 'Client token',
        usernameLabel: null,
        addressLabel: 'RomM address, as this server reaches it',
        addressExample: 'http://romm:8080/',
        folderExample: '',
        takes: '',
        purpose:
          'Finds exported games in RomM, links to their pages, and asks RomM to scan what the ' +
          'export changed. It never changes anything in RomM. There is one, for the one export.',
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
  /** RomM's API only: RomM's address as a browser reaches it, for links. */
  webUrl: string;
  /** RomM's API only: ask RomM to scan what the export changed. */
  scanAfterExport: boolean;
  /** Download clients and S3: what becomes of a source once its import is in the library. */
  afterImport: AfterImport;
};

/** What becomes of an import's source once the library holds a checked copy. */
export type AfterImport = 'keep' | 'remove';

/** Whether a kind can remove its sources after import. */
export function removesAfterImport(kind: ClientKind): boolean {
  return kind !== 'romm_api';
}

/** How the two choices read for a kind, and what removing takes. */
export function afterImportWording(kind: ClientKind): {
  keep: string;
  remove: string;
  needs: string;
} {
  const client = kindLabel(kind);
  switch (kind) {
    case 'qbittorrent':
      return {
        keep: 'Keep the torrent in qBittorrent, seeding as qBittorrent is set to',
        remove: 'Remove the torrent and its files from qBittorrent',
        needs: 'qBittorrent deletes the files itself; the folder stays read-only here.',
      };
    case 'sabnzbd':
    case 'nzbget':
      return {
        keep: `Keep the download in ${client}'s folder`,
        remove: `Delete the downloaded files, and remove the job from ${client}'s history`,
        needs:
          `${client} cannot delete them, so Tangible does: only the files whose bytes the ` +
          'library holds, and only in a watched root listed in TANGIBLE_REMOVABLE_ROOTS and ' +
          'mounted read-write.',
      };
    case 's3':
      return {
        keep: 'Keep the object in its bucket',
        remove: 'Delete the version of the object that was imported',
        needs:
          'Only while it is unchanged; a newer version is left. The access key then needs ' +
          's3:DeleteObject, and s3:DeleteObjectVersion for a versioned bucket.',
      };
    case 'romm_api':
      return { keep: '', remove: '', needs: '' };
  }
}

/**
 * Why removing is not offered for this form, or `null` when it is: a Usenet
 * client deletes only in a watched root Tangible may delete in.
 */
export function removalBlocked(form: ClientForm, removableRoots: string[]): string | null {
  if (form.kind !== 'sabnzbd' && form.kind !== 'nzbget') return null;
  if (form.localRoot === '') return 'Choose the watched root first.';
  if (removableRoots.includes(form.localRoot)) return null;
  return (
    `The watched root ${form.localRoot} is not one Tangible may delete in. List it in ` +
    'TANGIBLE_REMOVABLE_ROOTS, mount it read-write and restart the server to offer this.'
  );
}

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
    webUrl: '',
    scanAfterExport: true,
    afterImport: 'keep',
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
    webUrl: text(view.settings, 'web_url'),
    scanAfterExport: view.settings.scan_after_export !== false,
    afterImport: view.settings.after_import === 'remove' ? 'remove' : 'keep',
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
  if (form.kind === 'romm_api') {
    const settings: Record<string, unknown> = {
      base_url: form.baseUrl.trim(),
      scan_after_export: form.scanAfterExport,
    };
    // Left out, links use the address the server reaches RomM at.
    if (form.webUrl.trim() !== '') settings.web_url = form.webUrl.trim();
    return settings;
  }
  // Always sent: settings are saved whole, so leaving it out would turn
  // removal off on every edit.
  if (form.kind === 's3') {
    return {
      endpoint: form.baseUrl.trim(),
      region: form.region.trim() || 'us-east-1',
      path_style: form.pathStyle,
      buckets: parseBuckets(form.buckets),
      after_import: form.afterImport,
    };
  }
  const settings: Record<string, unknown> = {
    base_url: form.baseUrl.trim(),
    category: form.category.trim() || 'tangible',
    client_path: form.clientPath.trim(),
    local_root: form.localRoot,
    after_import: form.afterImport,
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

/**
 * The metadata providers people search to find an item to ask for (ADR
 * 0021). Each is one service at its own public address, so it takes no
 * settings, only its key; there is at most one of each.
 */
export type ProviderKind = 'musicbrainz' | 'tmdb' | 'igdb';

/** The providers, in the order offered. */
export const PROVIDER_KINDS: ProviderKind[] = ['musicbrainz', 'tmdb', 'igdb'];

/** Whether an integration is a metadata provider. */
export function isProviderKind(kind: string): kind is ProviderKind {
  return (PROVIDER_KINDS as string[]).includes(kind);
}

/** A secret a provider takes, as its form asks for it. */
export interface ProviderSecret {
  field: string;
  label: string;
  hint: string;
}

/** What the form says about a provider. */
export interface ProviderWording {
  label: string;
  /** What it is searched for. */
  covers: string;
  /** Where its key comes from, in words; empty for none. */
  getting: string;
  secrets: ProviderSecret[];
}

/** The words for a provider. */
export function providerWording(kind: ProviderKind): ProviderWording {
  switch (kind) {
    case 'musicbrainz':
      return {
        label: 'MusicBrainz',
        covers:
          'Music: CD releases, with their artist, year, country, catalogue number and barcode.',
        getting:
          'No key. Searches are kept to one a second, as MusicBrainz asks, and name Tangible to it.',
        secrets: [],
      };
    case 'tmdb':
      return {
        label: 'TMDB',
        covers: 'Films and television, and IMDb IDs, which TMDB looks up.',
        getting:
          'An API Read Access Token, the long one, from your TMDB account settings (API). Not the short API key.',
        secrets: [
          {
            field: 'read_access_token',
            label: 'API Read Access Token',
            hint: 'Sent as a bearer token, never in an address.',
          },
        ],
      };
    case 'igdb':
      return {
        label: 'IGDB',
        covers: 'Games, with their platforms and year.',
        getting:
          'A Twitch application: its client ID and a client secret, from the Twitch developer console. Tangible asks Twitch for a token with them.',
        secrets: [
          { field: 'client_id', label: 'Client ID', hint: '' },
          {
            field: 'client_secret',
            label: 'Client secret',
            hint: 'Sent to Twitch only, in the body of a token request.',
          },
        ],
      };
  }
}

/** The secrets typed, without the empty ones. */
function typed(secrets: Record<string, string>): Record<string, string> {
  return Object.fromEntries(
    Object.entries(secrets)
      .map(([field, value]) => [field, value.trim()] as const)
      .filter(([, value]) => value !== ''),
  );
}

/** What adds a provider, named after itself. */
export function createProviderRequest(
  kind: ProviderKind,
  secrets: Record<string, string>,
): CreateIntegrationRequest {
  return {
    kind,
    name: providerWording(kind).label,
    enabled: true,
    settings: {},
    secrets: typed(secrets),
  };
}

/** What replaces a provider's key: only the fields typed. */
export function updateProviderRequest(secrets: Record<string, string>): UpdateIntegrationRequest {
  return { secrets: typed(secrets) };
}

/** Whether every secret a provider takes is set. */
export function providerReady(view: Pick<IntegrationView, 'secrets'>): boolean {
  return view.secrets.every((secret) => secret.set);
}

/** How trying a provider went, in words. */
export function providerTestOutcome(name: string, test: IntegrationTestView): string {
  return test.ok
    ? `${name} answered a search.`
    : `${name} did not work: ${test.detail ?? test.error_code ?? 'no reason given'}`;
}
