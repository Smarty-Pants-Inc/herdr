//! Startup group-privilege guard (herdr#188).
//!
//! This build never needs the `herdr` group. A setgid `root:herdr` binary left by an
//! older install, or a user in the `herdr` group, could otherwise read a legacy
//! `0640 root:herdr` server key from this process or any pane child. So before any
//! file, socket or child: drop an inherited set-group-id and verify the drop, then
//! refuse to run while any group in the process is the `herdr` group.
//! herdr-2636-enroll (herdr#195) replaces this guard with its own key-loading gid handling.

use std::io;

pub(crate) const SETGID_REFUSAL: &str = "herdr: refusing to run with a set-group-id it cannot drop";
pub(crate) const HERDR_GROUP_REFUSAL: &str =
    "herdr: refusing to run with the herdr group; this build does not use it";

const GROUP_FILE: &str = "/etc/group";
const HERDR_GROUP: &str = "herdr";

/// The process gid facts the guard reads and changes. Tests inject a fake.
pub(crate) trait GroupIdSource {
    /// Real, effective and saved gid.
    fn resgid(&self) -> io::Result<[libc::gid_t; 3]>;
    /// Set real, effective and saved gid to `gid`.
    fn set_resgid(&mut self, gid: libc::gid_t) -> io::Result<()>;
    fn supplementary_groups(&self) -> io::Result<Vec<libc::gid_t>>;
    /// Contents of `/etc/group`; `Ok(None)` when the file does not exist.
    fn group_file(&self) -> io::Result<Option<String>>;
}

struct ProcessGroupIds;

impl GroupIdSource for ProcessGroupIds {
    fn resgid(&self) -> io::Result<[libc::gid_t; 3]> {
        let (mut real, mut effective, mut saved) = (0, 0, 0);
        // SAFETY: the three out-pointers are valid, distinct gid_t locals.
        if unsafe { libc::getresgid(&mut real, &mut effective, &mut saved) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok([real, effective, saved])
    }

    fn set_resgid(&mut self, gid: libc::gid_t) -> io::Result<()> {
        // SAFETY: setresgid takes plain integers and changes only this process's gids.
        if unsafe { libc::setresgid(gid, gid, gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn supplementary_groups(&self) -> io::Result<Vec<libc::gid_t>> {
        loop {
            // SAFETY: a zero size with a null buffer only returns the group count.
            let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut groups = vec![0; count as usize];
            // SAFETY: the buffer holds exactly `count` gid_t entries.
            let filled = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
            if filled >= 0 {
                groups.truncate(filled as usize);
                return Ok(groups);
            }
            let err = io::Error::last_os_error();
            // The list grew between the two calls; ask again.
            if err.raw_os_error() != Some(libc::EINVAL) {
                return Err(err);
            }
        }
    }

    fn group_file(&self) -> io::Result<Option<String>> {
        match std::fs::read_to_string(GROUP_FILE) {
            Ok(contents) => Ok(Some(contents)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
}

/// The gids named `herdr` in `/etc/group` content. Read directly, not through NSS.
fn herdr_group_ids(group_file: &str) -> Vec<libc::gid_t> {
    group_file
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let gid = fields.nth(1)?;
            (name == HERDR_GROUP).then(|| gid.trim().parse().ok())?
        })
        .collect()
}

pub(crate) fn enforce_group_privilege(ids: &mut impl GroupIdSource) -> Result<(), &'static str> {
    let [real, effective, saved] = ids.resgid().map_err(|_| SETGID_REFUSAL)?;
    if effective != real || saved != real {
        ids.set_resgid(real).map_err(|_| SETGID_REFUSAL)?;
        if ids.resgid().map_err(|_| SETGID_REFUSAL)? != [real; 3] {
            return Err(SETGID_REFUSAL);
        }
    }
    let groups = ids
        .supplementary_groups()
        .map_err(|_| HERDR_GROUP_REFUSAL)?;
    // ponytail: an unreadable /etc/group fails closed; a missing one has no herdr group.
    let herdr = match ids.group_file().map_err(|_| HERDR_GROUP_REFUSAL)? {
        Some(contents) => herdr_group_ids(&contents),
        None => Vec::new(),
    };
    if groups
        .iter()
        .chain(std::iter::once(&real))
        .any(|gid| herdr.contains(gid))
    {
        return Err(HERDR_GROUP_REFUSAL);
    }
    Ok(())
}

/// Call first in `main`. Exits nonzero when the guard refuses.
pub(crate) fn drop_inherited_group_privilege() {
    if let Err(refusal) = enforce_group_privilege(&mut ProcessGroupIds) {
        eprintln!("{refusal}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeIds {
        ids: [libc::gid_t; 3],
        groups: Vec<libc::gid_t>,
        group_file: Option<String>,
        drop_fails: bool,
        drop_ignored: bool,
        set_calls: Vec<libc::gid_t>,
    }

    impl FakeIds {
        fn new(ids: [libc::gid_t; 3]) -> Self {
            Self {
                ids,
                groups: vec![1000],
                group_file: Some("root:x:0:\nherdr:x:977:\npaul:x:1000:\n".into()),
                drop_fails: false,
                drop_ignored: false,
                set_calls: Vec::new(),
            }
        }
    }

    impl GroupIdSource for FakeIds {
        fn resgid(&self) -> io::Result<[libc::gid_t; 3]> {
            Ok(self.ids)
        }
        fn set_resgid(&mut self, gid: libc::gid_t) -> io::Result<()> {
            self.set_calls.push(gid);
            if self.drop_fails {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            if !self.drop_ignored {
                self.ids = [gid; 3];
            }
            Ok(())
        }
        fn supplementary_groups(&self) -> io::Result<Vec<libc::gid_t>> {
            Ok(self.groups.clone())
        }
        fn group_file(&self) -> io::Result<Option<String>> {
            Ok(self.group_file.clone())
        }
    }

    #[test]
    fn setgid_herdr_is_dropped_to_the_real_gid_and_verified() {
        let mut ids = FakeIds::new([1000, 977, 977]);
        assert_eq!(enforce_group_privilege(&mut ids), Ok(()));
        assert_eq!(ids.set_calls, vec![1000]);
        assert_eq!(ids.ids, [1000; 3]);
    }

    #[test]
    fn a_saved_gid_alone_is_dropped() {
        let mut ids = FakeIds::new([1000, 1000, 977]);
        assert_eq!(enforce_group_privilege(&mut ids), Ok(()));
        assert_eq!(ids.ids, [1000; 3]);
    }

    #[test]
    fn a_failed_drop_refuses() {
        let mut ids = FakeIds::new([1000, 977, 977]);
        ids.drop_fails = true;
        assert_eq!(enforce_group_privilege(&mut ids), Err(SETGID_REFUSAL));
    }

    #[test]
    fn a_drop_that_does_not_verify_refuses() {
        let mut ids = FakeIds::new([1000, 977, 977]);
        ids.drop_ignored = true;
        assert_eq!(enforce_group_privilege(&mut ids), Err(SETGID_REFUSAL));
    }

    #[test]
    fn a_supplementary_herdr_group_refuses() {
        let mut ids = FakeIds::new([1000; 3]);
        ids.groups = vec![1000, 977];
        assert_eq!(enforce_group_privilege(&mut ids), Err(HERDR_GROUP_REFUSAL));
    }

    #[test]
    fn a_real_herdr_gid_refuses() {
        let mut ids = FakeIds::new([977; 3]);
        assert_eq!(enforce_group_privilege(&mut ids), Err(HERDR_GROUP_REFUSAL));
    }

    #[test]
    fn a_normal_process_is_unchanged() {
        let mut ids = FakeIds::new([1000; 3]);
        assert_eq!(enforce_group_privilege(&mut ids), Ok(()));
        assert!(ids.set_calls.is_empty());
        assert_eq!(ids.ids, [1000; 3]);
    }

    #[test]
    fn no_group_file_means_no_herdr_group() {
        let mut ids = FakeIds::new([1000; 3]);
        ids.group_file = None;
        ids.groups = vec![1000, 977];
        assert_eq!(enforce_group_privilege(&mut ids), Ok(()));
    }

    #[test]
    fn herdr_group_ids_match_the_exact_name_only() {
        let file = "herdr-dev:x:5:\n#herdr:x:6:\nherdr:x:977:a,b\nbad\nherdr:x:nope:\n";
        assert_eq!(herdr_group_ids(file), vec![977]);
    }

    #[test]
    fn the_live_process_ids_are_readable() {
        let ids = ProcessGroupIds;
        let [real, ..] = ids.resgid().expect("getresgid");
        // SAFETY: getgid has no preconditions.
        assert_eq!(real, unsafe { libc::getgid() });
        ids.supplementary_groups().expect("getgroups");
    }
}
