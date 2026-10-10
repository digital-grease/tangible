-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- How a drive is attached: USB or SATA, as the worker reads it from the
-- kernel's device tree. Issue reports name it, because USB bridges and SATA
-- ports fail differently.
--
-- Additive, with a default, so the previous release keeps working against
-- this schema: its workers do not report a connection and get 'unknown'.

ALTER TABLE drives
    ADD COLUMN connection TEXT NOT NULL DEFAULT 'unknown'
        CHECK (connection IN ('usb', 'sata', 'unknown'));
