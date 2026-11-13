// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

/**
 * Tangible's icons (docs/plans/ui-overhaul.md, decision 5): strokes on a
 * 24-pixel grid, one path each, drawn in the text's colour by Icon.svelte.
 * Kept to what the UI uses; add one here when a page needs it.
 */
export const ICONS = {
  overview: 'M3 10.5 12 3l9 7.5V21h-6v-6H9v6H3z',
  library:
    'M4 4h16a1 1 0 0 1 1 1v3a1 1 0 0 1 -1 1h-16a1 1 0 0 1 -1 -1v-3a1 1 0 0 1 1 -1zM4 11h16a1 1 0 0 1 1 1v3a1 1 0 0 1 -1 1h-16a1 1 0 0 1 -1 -1v-3a1 1 0 0 1 1 -1zM4 20h16',
  catalog: 'M5 4h11a3 3 0 0 1 3 3v13H8a3 3 0 0 1-3-3zM5 17a3 3 0 0 1 3-3h11',
  disc: 'M3 12a9 9 0 1 0 18 0a9 9 0 1 0 -18 0M9.5 12a2.5 2.5 0 1 0 5 0a2.5 2.5 0 1 0 -5 0',
  import: 'M12 3v12m-5-5 5 5 5-5M4 17v3h16v-3',
  request: 'M4 13h4.5l1.5 3h4l1.5-3H20M4 13l2.5-8h11L20 13v6H4z',
  rip: 'M3 12a7 7 0 1 0 14 0a7 7 0 1 0 -14 0M8.2 12a1.8 1.8 0 1 0 3.6 0a1.8 1.8 0 1 0 -3.6 0M20 7v10m-3-3 3 3 3-3',
  burn: 'M12 3c1 3 5.5 5 5.5 10.5a5.5 5.5 0 0 1-11 0c0-2.6 1.4-4.3 2.6-5.3 0 2 1 3.3 2.2 3.3 0-3.2-1.2-5.8.7-8.5z',
  worker:
    'M4.5 4h15a1.5 1.5 0 0 1 1.5 1.5v4a1.5 1.5 0 0 1 -1.5 1.5h-15a1.5 1.5 0 0 1 -1.5 -1.5v-4a1.5 1.5 0 0 1 1.5 -1.5zM4.5 13h15a1.5 1.5 0 0 1 1.5 1.5v4a1.5 1.5 0 0 1 -1.5 1.5h-15a1.5 1.5 0 0 1 -1.5 -1.5v-4a1.5 1.5 0 0 1 1.5 -1.5zM7 7.5h.01M7 16.5h.01',
  user: 'M8 8a4 4 0 1 0 8 0a4 4 0 1 0 -8 0M4 21a8 8 0 0 1 16 0',
  integration: 'M9 3v5M15 3v5M6 8h12v3a6 6 0 0 1-12 0zM12 17v4',
  storage:
    'M4 5.5a8 2.5 0 1 0 16 0a8 2.5 0 1 0 -16 0M4 5.5v13c0 1.4 3.6 2.5 8 2.5s8-1.1 8-2.5v-13M4 12c0 1.4 3.6 2.5 8 2.5s8-1.1 8-2.5',
  dat: 'm4 6 1.5 1.5L8 5m-4 7 1.5 1.5L8 11m-4 7 1.5 1.5L8 17M11 6.5h9M11 12.5h9M11 18.5h9',
  audit: 'M3 12a9 9 0 1 0 18 0a9 9 0 1 0 -18 0M12 7v5l3 2',
  diagnostics: 'M3 12h4l2-6 4 12 2-6h6',
  search: 'M4 11a7 7 0 1 0 14 0a7 7 0 1 0 -14 0M20 20l-4-4',
  plus: 'M12 5v14M5 12h14',
  lock: 'M7 11h10a2 2 0 0 1 2 2v6a2 2 0 0 1 -2 2h-10a2 2 0 0 1 -2 -2v-6a2 2 0 0 1 2 -2zM8 11V8a4 4 0 0 1 8 0v3',
  filter: 'M4 5h16l-6 7.5V19l-4 2v-8.5z',
  'arrow-right': 'M5 12h14m-5-5 5 5-5 5',
  'chevron-down': 'm6 9 6 6 6-6',
  'chevron-right': 'm9 6 6 6-6 6',
  close: 'M6 6l12 12M18 6 6 18',
  menu: 'M4 7h16M4 12h16M4 17h16',
  'sidebar-collapse':
    'M4 4h16a1 1 0 0 1 1 1v14a1 1 0 0 1 -1 1h-16a1 1 0 0 1 -1 -1v-14a1 1 0 0 1 1 -1zM9 4v16M16 9l-3 3 3 3',
  'sidebar-expand':
    'M4 4h16a1 1 0 0 1 1 1v14a1 1 0 0 1 -1 1h-16a1 1 0 0 1 -1 -1v-14a1 1 0 0 1 1 -1zM9 4v16M13 9l3 3-3 3',
  check: 'm5 12.5 4.5 4.5L19 7',
  alert: 'M12 4 2.8 19.5h18.4zM12 10v4.5M12 17.2h.01',
  info: 'M3 12a9 9 0 1 0 18 0a9 9 0 1 0 -18 0M12 11v5M12 8h.01',
  external: 'M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5',
  sun: 'M8 12a4 4 0 1 0 8 0a4 4 0 1 0 -8 0M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4',
  moon: 'M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z',
  monitor:
    'M4.5 4h15a1.5 1.5 0 0 1 1.5 1.5v9a1.5 1.5 0 0 1 -1.5 1.5h-15a1.5 1.5 0 0 1 -1.5 -1.5v-9a1.5 1.5 0 0 1 1.5 -1.5zM8 20h8M12 16v4',
  'sign-out': 'M9 4H6a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h3M14 8l4 4-4 4M18 12H9',
  more: 'M4.9 12a1.1 1.1 0 1 0 2.2 0a1.1 1.1 0 1 0 -2.2 0M10.9 12a1.1 1.1 0 1 0 2.2 0a1.1 1.1 0 1 0 -2.2 0M16.9 12a1.1 1.1 0 1 0 2.2 0a1.1 1.1 0 1 0 -2.2 0',
  eject: 'M5 15h14L12 6zM5 19h14',
  refresh:
    'M4 12a8 8 0 0 1 13.7-5.7L20 8.5M20 4v4.5h-4.5M20 12a8 8 0 0 1-13.7 5.7L4 15.5M4 20v-4.5h4.5',
  source: 'm8 7-5 5 5 5m8-10 5 5-5 5',
} as const;

export type IconName = keyof typeof ICONS;
