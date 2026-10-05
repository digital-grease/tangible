// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tangible_fuzz::toc_file(data));
