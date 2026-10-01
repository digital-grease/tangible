// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Turning a validated plan into cdrdao's arguments.
//!
//! The same rule as the xorriso builder: a typed plan goes in, a fixed
//! argument array comes out, and no plan field can add an argument. The only
//! values that reach the vector are the device alias the worker was configured
//! with, a path this engine chose itself for the table of contents, and a
//! speed parsed into a number.
//!
//! Everything a disc's shape needs is in the table of contents rather than on
//! the command line, which keeps this list short. What is on it is
//! deliberately conservative:
//!
//! - `-n`, so cdrdao does not pause ten seconds before writing. The operator
//!   confirmed the burn long before this runs, and a pause nobody can see is
//!   ten seconds of a drive held for nothing.
//! - `-v 2`, stated rather than defaulted, because the parser depends on the
//!   messages that level prints.
//! - `--multi` only when the plan asks for the disc to be left open.
//! - Never `--force`, which would write a table of contents cdrdao warned
//!   about; never `--overburn` or `--full-burn`; never `--swap`, because byte
//!   order is stated per file in the table of contents; never `--driver`,
//!   `--reload` or `--eject`. Ejecting is the runner's decision, made from the
//!   job's eject policy, and it asks [`eject`] for it.

use std::path::Path;

use crate::plan::{BurnPlan, DriveRef};

/// The cdrdao executable, resolved through `PATH` by the spawner. The worker
/// image is where its location is pinned.
pub const CDRDAO: &str = "cdrdao";

/// Why a plan could not be turned into write arguments.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    /// The table of contents is not at an absolute path that is valid text.
    ///
    /// cdrdao takes it as a bare trailing argument, so a relative path that
    /// began with a dash would be read as an option.
    #[error("the table of contents path {0} cannot be passed as an argument")]
    UnusableTocPath(String),

    /// A speed label that is not a whole-number multiple.
    #[error("the speed {0:?} is not one cdrdao can be asked for")]
    UnrecognisedSpeed(String),
}

/// Arguments that make cdrdao print its banner.
///
/// There is no version command. Every invocation prints the version first, and
/// running it with no command prints the banner and the usage, exiting 1.
#[must_use]
pub fn version() -> Vec<String> {
    Vec::new()
}

/// Arguments that make cdrdao read a table of contents back and say what it
/// understood, without touching a drive.
///
/// # Errors
///
/// [`CommandError::UnusableTocPath`] if the path could be mistaken for an
/// option.
pub fn show_toc(toc: &Path) -> Result<Vec<String>, CommandError> {
    Ok(vec!["show-toc".to_owned(), toc_argument(toc)?])
}

/// Arguments that report what is in a drive.
#[must_use]
pub fn disk_info(drive: &DriveRef) -> Vec<String> {
    vec![
        "disk-info".to_owned(),
        "--device".to_owned(),
        drive.device_alias.clone(),
    ]
}

/// Arguments that report what a drive can do.
#[must_use]
pub fn drive_info(drive: &DriveRef) -> Vec<String> {
    vec![
        "drive-info".to_owned(),
        "--device".to_owned(),
        drive.device_alias.clone(),
    ]
}

/// Arguments that write a plan's table of contents to its drive.
///
/// # Errors
///
/// [`CommandError`] if the table of contents path or the speed cannot be
/// expressed safely.
pub fn write(plan: &BurnPlan, toc: &Path) -> Result<Vec<String>, CommandError> {
    let mut arguments = vec![
        "write".to_owned(),
        "--device".to_owned(),
        plan.drive.device_alias.clone(),
        "-n".to_owned(),
        "-v".to_owned(),
        "2".to_owned(),
    ];

    if let Some(label) = &plan.speed {
        let speed = speed(label).ok_or_else(|| CommandError::UnrecognisedSpeed(label.clone()))?;
        arguments.push("--speed".to_owned());
        arguments.push(speed.to_string());
    }
    if !plan.finalize {
        // Leaves the session open. A layout spanning sessions is refused
        // before this, so this is only ever one session not yet closed.
        arguments.push("--multi".to_owned());
    }

    arguments.push(toc_argument(toc)?);
    Ok(arguments)
}

/// Arguments that read a whole disc back: its table of contents into `toc`,
/// and every track's data into `datafile`.
///
/// Read over SCSI, as the write went, and every sector of every track is read,
/// which is what makes an unreadable audio sector show up as an error rather
/// than as a length check that passed. cdrdao refuses to overwrite either
/// file, so the caller removes them first.
///
/// # Errors
///
/// [`CommandError::UnusableTocPath`] if either path could be mistaken for an
/// option.
pub fn read_cd(drive: &DriveRef, datafile: &Path, toc: &Path) -> Result<Vec<String>, CommandError> {
    Ok(vec![
        "read-cd".to_owned(),
        "--device".to_owned(),
        drive.device_alias.clone(),
        "-v".to_owned(),
        "2".to_owned(),
        "--datafile".to_owned(),
        toc_argument(datafile)?,
        toc_argument(toc)?,
    ])
}

/// Arguments that open the tray.
///
/// cdrdao has no eject command. `unlock --eject` is the nearest: it releases
/// the medium lock and opens the tray. It also asks the driver to abort a
/// disc-at-once write, which for the generic MMC driver every current drive
/// uses is a cache flush and nothing else, and this is only ever run while
/// the worker holds the drive's lock, so there is no write to abort.
#[must_use]
pub fn eject(drive: &DriveRef) -> Vec<String> {
    vec![
        "unlock".to_owned(),
        "--device".to_owned(),
        drive.device_alias.clone(),
        "--eject".to_owned(),
    ]
}

/// Read a speed label into the whole-number multiple cdrdao takes.
///
/// `8x`, `8X` and `8` all mean eight. Anything else is refused rather than
/// rounded: a label that does not parse is a label nobody validated.
#[must_use]
pub fn speed(label: &str) -> Option<u32> {
    let digits = label.trim().trim_end_matches(['x', 'X']);
    let speed: u32 = digits.parse().ok()?;
    (1..=99).contains(&speed).then_some(speed)
}

/// Render the table of contents path, refusing one that could be an option.
fn toc_argument(toc: &Path) -> Result<String, CommandError> {
    let text = toc
        .to_str()
        .ok_or_else(|| CommandError::UnusableTocPath(toc.display().to_string()))?;
    if !toc.is_absolute() {
        return Err(CommandError::UnusableTocPath(text.to_owned()));
    }
    Ok(text.to_owned())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::plan::WriteMode;
    use std::path::PathBuf;
    use tangible_domain::{BurnAttemptId, DriveId, WorkerId};

    fn drive() -> DriveRef {
        DriveRef {
            worker_id: WorkerId::generate(),
            drive_id: DriveId::generate(),
            device_alias: "/dev/disc-block".to_owned(),
        }
    }

    fn plan() -> BurnPlan {
        BurnPlan {
            attempt_id: BurnAttemptId::generate(),
            drive: drive(),
            inputs: Vec::new(),
            tracks: Vec::new(),
            catalog: None,
            mode: WriteMode::TocDiscAtOnce,
            accepted_profiles: vec!["CD-R".to_owned()],
            speed: None,
            finalize: true,
            eject_on_success: true,
            total_bytes: 0,
        }
    }

    fn toc() -> PathBuf {
        PathBuf::from("/var/lib/tangible-worker/toc/attempt.toc")
    }

    #[test]
    fn a_write_is_exactly_these_arguments() {
        // Asserted in full: an argument that appears here without a test
        // saying why is an argument nobody decided to pass.
        assert_eq!(
            write(&plan(), &toc()).expect("arguments"),
            vec![
                "write",
                "--device",
                "/dev/disc-block",
                "-n",
                "-v",
                "2",
                "/var/lib/tangible-worker/toc/attempt.toc",
            ]
        );
    }

    #[test]
    fn nothing_dangerous_is_ever_added() {
        let mut open = plan();
        open.finalize = false;
        open.speed = Some("8x".to_owned());
        let mut plans = vec![plan(), open];
        plans[0].eject_on_success = true;

        for plan in plans {
            let arguments = write(&plan, &toc()).expect("arguments");
            for forbidden in [
                "--force",
                "--overburn",
                "--full-burn",
                "--swap",
                "--driver",
                "--reload",
                "--eject",
                "--capacity",
            ] {
                assert!(
                    !arguments.iter().any(|argument| argument == forbidden),
                    "{forbidden} appeared in {arguments:?}"
                );
            }
        }
    }

    #[test]
    fn a_speed_is_passed_as_the_number_cdrdao_takes() {
        let mut fast = plan();
        fast.speed = Some("8x".to_owned());
        let arguments = write(&fast, &toc()).expect("arguments");
        let position = arguments
            .iter()
            .position(|argument| argument == "--speed")
            .expect("a speed");
        assert_eq!(arguments[position + 1], "8");
    }

    #[test]
    fn a_speed_nobody_could_have_meant_is_refused() {
        for label in ["fast", "8.5x", "0x", "100x", "-4", ""] {
            let mut odd = plan();
            odd.speed = Some(label.to_owned());
            assert_eq!(
                write(&odd, &toc()),
                Err(CommandError::UnrecognisedSpeed(label.to_owned())),
                "{label}"
            );
        }
        assert_eq!(speed("8X"), Some(8));
        assert_eq!(speed("24"), Some(24));
    }

    #[test]
    fn an_open_disc_is_asked_for_by_name() {
        let mut open = plan();
        open.finalize = false;
        assert!(
            write(&open, &toc())
                .expect("arguments")
                .contains(&"--multi".to_owned())
        );
        assert!(
            !write(&plan(), &toc())
                .expect("arguments")
                .contains(&"--multi".to_owned())
        );
    }

    #[test]
    fn a_relative_toc_path_could_be_an_option_and_is_refused() {
        // A trailing bare argument beginning with a dash is an option to
        // cdrdao. The engine always chooses an absolute path; this is the
        // check that it did.
        let relative = PathBuf::from("-n.toc");
        assert!(matches!(
            write(&plan(), &relative),
            Err(CommandError::UnusableTocPath(_))
        ));
        assert!(show_toc(&relative).is_err());
    }

    #[test]
    fn the_toc_path_is_last_and_is_one_argument() {
        let spaced = PathBuf::from("/var/lib/tangible worker/--eject.toc");
        let arguments = write(&plan(), &spaced).expect("arguments");
        assert_eq!(
            arguments.last().map(String::as_str),
            Some("/var/lib/tangible worker/--eject.toc")
        );
        assert!(!arguments.contains(&"--eject".to_owned()));
    }

    #[test]
    fn a_read_back_is_exactly_these_arguments() {
        assert_eq!(
            read_cd(
                &drive(),
                Path::new("/var/lib/tangible-worker/toc/a.readback/readback.bin"),
                Path::new("/var/lib/tangible-worker/toc/a.readback/readback.toc"),
            )
            .expect("arguments"),
            vec![
                "read-cd",
                "--device",
                "/dev/disc-block",
                "-v",
                "2",
                "--datafile",
                "/var/lib/tangible-worker/toc/a.readback/readback.bin",
                "/var/lib/tangible-worker/toc/a.readback/readback.toc",
            ]
        );
        assert!(read_cd(&drive(), Path::new("rel.bin"), Path::new("/a.toc")).is_err());
    }

    #[test]
    fn inspection_probing_and_ejecting_name_the_configured_device() {
        let drive = drive();
        for arguments in [disk_info(&drive), drive_info(&drive), eject(&drive)] {
            let position = arguments
                .iter()
                .position(|argument| argument == "--device")
                .expect("a device");
            assert_eq!(arguments[position + 1], "/dev/disc-block");
        }
        assert_eq!(eject(&drive)[0], "unlock");
    }
}
