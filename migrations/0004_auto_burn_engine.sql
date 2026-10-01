-- SPDX-FileCopyrightText: 2026 digitalgrease
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- A burn attempt may be claimed by a worker running the combined engine.
--
-- A worker set to `auto` holds xorriso and cdrdao and gives each plan to the
-- one its shape needs. It registers as `auto`, and a claim records the
-- worker's engine, so the first claim by such a worker violated the old list
-- and every claim after it rolled back: found by the first end-to-end run,
-- where a queued job was never picked up.
--
-- `auto` is what an attempt says between claim and completion. Completion
-- replaces it with the engine that actually wrote, taken from the write
-- report, so a finished attempt names xorriso or cdrdao.
--
-- Widening only: a server still running the previous version never writes the
-- new value, so this is safe to apply while it runs.

ALTER TABLE burn_attempts DROP CONSTRAINT burn_attempts_engine_check;
ALTER TABLE burn_attempts ADD CONSTRAINT burn_attempts_engine_check
    CHECK (engine IN ('fake', 'xorriso', 'cdrdao', 'auto'));
