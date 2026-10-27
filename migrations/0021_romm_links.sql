-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Linking exported games to RomM's ROMs (docs/plans/romm-api.md M2,
-- backlog E9-07).
--
-- Once RomM's API is configured, each export is looked for in RomM by its
-- platform and folder name, and what was found is recorded beside the
-- export: RomM's id for the ROM, which its page is named by, and whether
-- RomM's files agree with what was written. A quick scan marks a ROM
-- missing when its folder goes, so the id is kept for a removed export
-- until RomM says it is missing. A renamed folder keeps its ROM only when
-- its files hash the same, so a renamed export is found again by name.
--
-- Comparing sizes needs the size of each file written, which receipts now
-- record. Receipts written before this have none until the exporter's
-- next pass fills them in, and are compared by name meanwhile.
--
-- Additive: the previous version never reads or writes the new columns.

ALTER TABLE romm_edition_exports
    -- RomM's id for the ROM the export became.
    ADD COLUMN romm_rom_id     BIGINT CHECK (romm_rom_id IS NULL OR romm_rom_id > 0),
    -- unseen: RomM has no ROM for the folder yet. linked: it has, and its
    -- files agree with what was written. missing: RomM marks the ROM
    -- missing from its library. differs: RomM's files do not agree, and
    -- romm_detail says how.
    ADD COLUMN romm_state      TEXT CHECK (romm_state IS NULL
                                   OR romm_state IN ('unseen', 'linked', 'missing', 'differs')),
    ADD COLUMN romm_detail     TEXT CHECK (romm_detail IS NULL OR length(romm_detail) <= 2000),
    ADD COLUMN romm_checked_at TIMESTAMPTZ,
    -- A ROM found has an id, and one not found has none.
    ADD CONSTRAINT romm_edition_exports_found_has_id CHECK (
        romm_state IS NULL OR (romm_state = 'unseen') = (romm_rom_id IS NULL)),
    ADD CONSTRAINT romm_edition_exports_checked_when_looked CHECK (
        (romm_state IS NULL) = (romm_checked_at IS NULL));

ALTER TABLE export_receipts
    ADD COLUMN length_bytes BIGINT CHECK (length_bytes IS NULL OR length_bytes >= 0);
