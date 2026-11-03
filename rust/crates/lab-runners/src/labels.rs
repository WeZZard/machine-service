//! The labels a lab job asks for, and what each needs (LR-3,
//! DC-266).

/// Every self-hosted job carries it.
pub const SELF_HOSTED: &str = "self-hosted";
/// A runner VM of the `macos26` image.
pub const MACOS: &str = "lab-macos";
/// A runner VM of the `ubuntu2404` image.
pub const UBUNTU: &str = "lab-ubuntu";
/// The job will lease one macOS VM of its host's test environment as its
/// target.
pub const MACOS_TARGET: &str = "lab-macos-target";
/// The job's target is a durable Mac's test environment, which exists only
/// during a run the owner dispatched (LR-10).
pub const DURABLE_TARGET: &str = "lab-durable-target";

/// The image of a macOS runner VM.
pub const MACOS_IMAGE: &str = "macos26";
/// The image of an Ubuntu runner VM.
pub const UBUNTU_IMAGE: &str = "ubuntu2404";

/// What a lab job needs from its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Need {
    /// The runner VM's image.
    pub image: &'static str,
    /// The runner VM's guest kind, as an image line names it.
    pub kind: Kind,
    /// The macOS slots the job takes on its host: its runner VM's and its
    /// target's.
    pub macos_slots: u32,
    /// The slots of those that the runner VM itself takes.
    pub runner_slots: u32,
    /// Whether its target is a durable Mac's test environment rather than
    /// this host's own.
    pub durable: bool,
}

/// A runner VM's guest kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// macOS.
    Macos,
    /// Ubuntu.
    Linux,
}

impl Kind {
    /// The name an image line gives the kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Macos => "macos",
            Kind::Linux => "linux",
        }
    }
}

/// A job's labels, compared without case and order, as GitHub matches them.
pub fn key(labels: &[String]) -> Vec<String> {
    let mut key: Vec<String> = labels
        .iter()
        .map(|label| label.to_ascii_lowercase())
        .collect();
    key.sort();
    key.dedup();
    key
}

/// What a job with `labels` needs, or `None` when it is not a lab job this
/// controller serves: `self-hosted`, exactly one of `lab-macos` and
/// `lab-ubuntu`, optionally `lab-macos-target`, and nothing else. A job
/// that asks for any other label, such as a durable Mac's name or `macOS`,
/// is never given a runner VM.
pub fn need(labels: &[String]) -> Option<Need> {
    let key = key(labels);
    let known = [SELF_HOSTED, MACOS, UBUNTU, MACOS_TARGET, DURABLE_TARGET];
    if !key.iter().all(|label| known.contains(&label.as_str())) {
        return None;
    }
    if !key.iter().any(|label| label == SELF_HOSTED) {
        return None;
    }
    let has = |label: &str| key.iter().any(|item| item == label);
    let target = u32::from(has(MACOS_TARGET));
    let durable = has(DURABLE_TARGET);
    if durable && target > 0 {
        return None;
    }
    match (has(MACOS), has(UBUNTU)) {
        (true, false) => Some(Need {
            image: MACOS_IMAGE,
            kind: Kind::Macos,
            macos_slots: 1 + target,
            runner_slots: 1,
            durable,
        }),
        (false, true) => Some(Need {
            image: UBUNTU_IMAGE,
            kind: Kind::Linux,
            macos_slots: target,
            runner_slots: 0,
            durable,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn a_lab_job_names_its_image_and_its_target() {
        let macos = need(&labels(&["self-hosted", "lab-macos"])).unwrap();
        assert_eq!(
            (macos.image, macos.macos_slots, macos.runner_slots),
            ("macos26", 1, 1)
        );
        let both = need(&labels(&["lab-macos-target", "Lab-MacOS", "self-hosted"])).unwrap();
        assert_eq!(both.macos_slots, 2);
        let ubuntu = need(&labels(&["self-hosted", "lab-ubuntu"])).unwrap();
        assert_eq!(
            (ubuntu.image, ubuntu.macos_slots, ubuntu.kind),
            ("ubuntu2404", 0, Kind::Linux)
        );
        let ubuntu_target =
            need(&labels(&["self-hosted", "lab-ubuntu", "lab-macos-target"])).unwrap();
        assert_eq!(
            (ubuntu_target.macos_slots, ubuntu_target.runner_slots),
            (1, 0)
        );
        // A durable cell's target is on a durable Mac: no slot here.
        let durable = need(&labels(&["self-hosted", "lab-macos", "lab-durable-target"])).unwrap();
        assert_eq!((durable.macos_slots, durable.durable), (1, true));
        assert!(!macos.durable);
    }

    #[test]
    fn any_other_job_is_not_served() {
        for job in [
            labels(&["self-hosted", "lab-macos", "lab-ubuntu"]),
            labels(&["self-hosted", "lab-macos-target"]),
            labels(&[
                "self-hosted",
                "lab-macos",
                "lab-macos-target",
                "lab-durable-target",
            ]),
            labels(&["self-hosted", "lab-durable-target"]),
            labels(&["lab-macos"]),
            labels(&["self-hosted", "lab"]),
            labels(&["self-hosted", "macOS", "ARM64", "lab"]),
            labels(&["self-hosted", "lab-macos", "macOS"]),
            labels(&["ubuntu-latest"]),
            labels(&[]),
        ] {
            assert_eq!(need(&job), None, "{job:?}");
        }
    }

    #[test]
    fn the_key_ignores_case_order_and_repeats() {
        assert_eq!(
            key(&labels(&["lab-macos", "Self-Hosted", "lab-macos"])),
            labels(&["lab-macos", "self-hosted"])
        );
    }
}
