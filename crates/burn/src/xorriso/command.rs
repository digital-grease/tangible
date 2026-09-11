// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Turning a validated plan into xorriso's arguments.
//!
//! This module is where the project's oldest rule about external tools lives:
//! an engine receives a typed plan and converts it into a fixed argument
//! array. Nothing here builds a command line, nothing takes a string of
//! options, and no plan field can add an argument: the only values that reach
//! the vector are paths the caller already validated and labels drawn from
//! closed sets.
//!
//! That matters more than it looks. A filename in a downloaded release can
//! contain anything, and the difference between an argument array and a shell
//! string is the difference between a file called `--eject` being a filename
//! and it being a flag.
//!
//! The arguments themselves are deliberately conservative:
//!
//! - `-abort_on FAILURE` so a problem stops the run rather than continuing
//!   into a half-written disc;
//! - no `-force`, no overburn, no speed override unless the plan names a
//!   speed from its own closed set;
//! - `blank=as_needed` only, which erases a rewritable medium that already
//!   holds data and does nothing to a blank one.

use std::path::Path;

use crate::plan::{BurnPlan, DriveRef};

/// The xorriso executable.
///
/// A fixed name resolved through `PATH` by the process spawner rather than an
/// absolute path baked in here: the binary lives in different places on
/// different distributions, and the worker container is where that is pinned.
pub const XORRISO: &str = "xorriso";

/// Arguments that ask xorriso which version it is.
#[must_use]
pub fn version() -> Vec<String> {
    vec!["-version".to_owned()]
}

/// Arguments that ask xorriso what drives exist.
#[must_use]
pub fn devices() -> Vec<String> {
    vec![
        // Tolerate the "no drives found" case as a report rather than an
        // abort: an empty machine is a fact, not an error.
        "-abort_on".to_owned(),
        "FATAL".to_owned(),
        "-devices".to_owned(),
    ]
}

/// Arguments that report what is in a drive.
#[must_use]
pub fn inspect(drive: &DriveRef) -> Vec<String> {
    vec![
        "-abort_on".to_owned(),
        "FATAL".to_owned(),
        "-outdev".to_owned(),
        device_argument(&drive.device_alias),
        "-toc".to_owned(),
    ]
}

/// Arguments that write one prepared image to a medium.
///
/// The cdrecord personality, because the input is a finished image and the job
/// is to put those bytes on a disc rather than to build a filesystem. Building
/// one here would produce a disc that does not match the artifact, which is
/// the one thing a preservation tool must not do.
///
/// Returns `None` when the plan does not describe a single-image write. A plan
/// with several inputs is a track layout, which is `cdrdao`'s job, and quietly
/// writing only the first input would produce a disc missing most of itself.
#[must_use]
pub fn write(plan: &BurnPlan) -> Option<Vec<String>> {
    let [input] = plan.inputs.as_slice() else {
        return None;
    };

    let mut arguments = vec![
        "-abort_on".to_owned(),
        "FAILURE".to_owned(),
        "-as".to_owned(),
        "cdrecord".to_owned(),
        // Progress reporting. Without it a twenty-minute write is silent, and
        // the operator watching it has nothing to watch.
        "-v".to_owned(),
        format!("dev={}", device_argument(&plan.drive.device_alias)),
        // Erases a rewritable medium that already holds data, and does
        // nothing to a blank one. Never an unconditional blank: that would
        // destroy a disc somebody put in the drive by mistake.
        "blank=as_needed".to_owned(),
    ];

    if let Some(speed) = &plan.speed {
        // Speeds come from a closed set on the plan, so this cannot become an
        // arbitrary argument.
        arguments.push(format!("speed={speed}"));
    }
    if plan.finalize {
        // Close the disc so it reads in players that do not understand an
        // open session.
        arguments.push("-eject".to_owned());
    }

    // Some drives need the tail of a track padded to read back reliably.
    // Fixed, small, and applied always rather than guessed at per drive.
    arguments.push("padsize=300k".to_owned());
    arguments.push(path_argument(&input.staged_path)?);

    Some(arguments)
}

/// Arguments that read a medium back for comparison.
///
/// Reading is done with `-check_media` rather than by copying the medium to a
/// file: the comparison is of what the drive can actually read, block by
/// block, and a copy would hide a sector that needed twenty retries.
#[must_use]
pub fn verify(drive: &DriveRef, blocks: u64) -> Vec<String> {
    vec![
        "-abort_on".to_owned(),
        "FAILURE".to_owned(),
        "-indev".to_owned(),
        device_argument(&drive.device_alias),
        "-check_media".to_owned(),
        "use=indev".to_owned(),
        format!("data_to={blocks}"),
        "--".to_owned(),
    ]
}

/// Render a device for xorriso.
///
/// A path that is not a device node is addressed as `stdio:`, which is
/// xorriso's own way of writing to a file. That is what makes a burn testable
/// without hardware: the same arguments, the same tool, a file instead of a
/// laser.
fn device_argument(alias: &str) -> String {
    if alias.starts_with("/dev/") {
        alias.to_owned()
    } else {
        format!("stdio:{alias}")
    }
}

/// Render a path as an argument, refusing anything that is not usable text.
///
/// A path that is not valid UTF-8 cannot be passed through this interface, and
/// guessing at an encoding would put bytes nobody chose on a command line.
fn path_argument(path: &Path) -> Option<String> {
    path.to_str().map(ToOwned::to_owned)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::plan::{PlannedInput, WriteMode};
    use std::path::PathBuf;
    use tangible_domain::{BurnAttemptId, DriveId, Sha256Digest, WorkerId};

    fn drive(alias: &str) -> DriveRef {
        DriveRef {
            worker_id: WorkerId::generate(),
            drive_id: DriveId::generate(),
            device_alias: alias.to_owned(),
        }
    }

    fn plan(inputs: Vec<PlannedInput>) -> BurnPlan {
        BurnPlan {
            attempt_id: BurnAttemptId::generate(),
            drive: drive("/dev/disc-block"),
            inputs,
            tracks: Vec::new(),
            catalog: None,
            mode: WriteMode::DataDiscAtOnce,
            accepted_profiles: vec!["CD-R".to_owned()],
            speed: None,
            finalize: true,
            eject_on_success: true,
            total_bytes: 100,
        }
    }

    fn input(path: &str) -> PlannedInput {
        PlannedInput {
            staged_path: PathBuf::from(path),
            sha256: Sha256Digest::from_bytes([0; 32]),
            length_bytes: 100,
        }
    }

    #[test]
    fn a_write_names_the_device_and_the_image_and_nothing_else() {
        let arguments = write(&plan(vec![input("/var/lib/staging/disc.iso")])).expect("arguments");
        assert!(arguments.contains(&"dev=/dev/disc-block".to_owned()));
        assert!(arguments.contains(&"/var/lib/staging/disc.iso".to_owned()));
        assert!(arguments.contains(&"blank=as_needed".to_owned()));
    }

    #[test]
    fn a_filename_can_never_become_a_flag() {
        // The reason this module exists. In a shell string a file called
        // `--eject` is an option; in an argument array it is a filename.
        let arguments = write(&plan(vec![input("/staging/--eject")])).expect("arguments");
        let last = arguments.last().expect("the image is last");
        assert_eq!(last, "/staging/--eject");
        assert_eq!(
            arguments.iter().filter(|a| *a == "--eject").count(),
            0,
            "the filename must not be counted as the eject flag: {arguments:?}"
        );
    }

    #[test]
    fn nothing_dangerous_is_ever_added() {
        // Overburning and force flags are disabled until an ADR introduces an
        // expert-only workflow, and no path through this builder may add one.
        let mut plans = vec![plan(vec![input("/staging/disc.iso")])];
        let mut fast = plans[0].clone();
        fast.speed = Some("8x".to_owned());
        fast.finalize = false;
        plans.push(fast);

        for plan in plans {
            let arguments = write(&plan).expect("arguments");
            for forbidden in [
                "-force",
                "force",
                "overburn",
                "-overwrite",
                "stream_recording",
            ] {
                assert!(
                    !arguments
                        .iter()
                        .any(|argument| argument.contains(forbidden)),
                    "{forbidden} appeared in {arguments:?}"
                );
            }
        }
    }

    #[test]
    fn a_failure_stops_the_run() {
        // Continuing past a failure is how a half-written disc gets reported
        // as finished.
        let arguments = write(&plan(vec![input("/staging/disc.iso")])).expect("arguments");
        let position = arguments
            .iter()
            .position(|argument| argument == "-abort_on")
            .expect("abort_on is set");
        assert_eq!(arguments[position + 1], "FAILURE");
    }

    #[test]
    fn a_plan_with_several_inputs_is_refused_rather_than_truncated() {
        // Several inputs is a track layout, which this engine does not write.
        // Writing only the first would produce a disc missing most of itself.
        assert!(write(&plan(vec![input("/a.bin"), input("/b.bin")])).is_none());
        assert!(write(&plan(vec![])).is_none());
    }

    #[test]
    fn a_speed_appears_only_when_the_plan_names_one() {
        let mut quiet = plan(vec![input("/staging/disc.iso")]);
        assert!(
            !write(&quiet)
                .expect("arguments")
                .iter()
                .any(|argument| argument.starts_with("speed=")),
            "no speed means the drive chooses"
        );

        quiet.speed = Some("4x".to_owned());
        assert!(
            write(&quiet)
                .expect("arguments")
                .contains(&"speed=4x".to_owned())
        );
    }

    #[test]
    fn finalizing_is_what_ejects() {
        let mut open = plan(vec![input("/staging/disc.iso")]);
        open.finalize = false;
        assert!(
            !write(&open)
                .expect("arguments")
                .contains(&"-eject".to_owned())
        );

        open.finalize = true;
        assert!(
            write(&open)
                .expect("arguments")
                .contains(&"-eject".to_owned())
        );
    }

    #[test]
    fn a_file_target_is_addressed_the_way_xorriso_expects() {
        // The mechanism that makes a real burn testable without a drive: the
        // same tool and the same arguments, writing to a file.
        let arguments = write(&BurnPlan {
            drive: drive("/var/lib/tangible/media/drive.img"),
            ..plan(vec![input("/staging/disc.iso")])
        })
        .expect("arguments");
        assert!(arguments.contains(&"dev=stdio:/var/lib/tangible/media/drive.img".to_owned()));
    }

    #[test]
    fn a_device_node_is_passed_through_unchanged() {
        assert_eq!(device_argument("/dev/sr0"), "/dev/sr0");
        assert_eq!(device_argument("/dev/disc-block"), "/dev/disc-block");
    }

    #[test]
    fn inspecting_and_verifying_name_the_same_device() {
        let drive = drive("/dev/disc-block");
        assert!(inspect(&drive).contains(&"/dev/disc-block".to_owned()));
        assert!(verify(&drive, 1000).contains(&"/dev/disc-block".to_owned()));
        assert!(verify(&drive, 1000).contains(&"data_to=1000".to_owned()));
    }

    #[test]
    fn probing_tolerates_a_machine_with_no_drives() {
        // An empty machine is a fact to report, not an error to abort on.
        let arguments = devices();
        let position = arguments
            .iter()
            .position(|argument| argument == "-abort_on")
            .expect("abort_on is set");
        assert_eq!(arguments[position + 1], "FATAL");
    }
}
