//! Startup group-privilege guard (herdr#188).
//!
//! This build never needs the `herdr` group. A setgid `root:herdr` binary left by an
//! older install, or a user in the `herdr` group, could otherwise read a legacy
//! `0640 root:herdr` server key from this process or any pane child. So before any
//! file, socket or child: drop an inherited set-group-id and verify the drop, then
//! refuse to run while the real gid or any supplementary gid is a key gid: a gid named
//! `herdr` in `/etc/group`, the group owner of an existing `/etc/herdr/server.key` or
//! `server.pub`, or the set-group-id this process started with. A supplementary gid kept
//! from an earlier login or NSS that `/etc/group` does not map still refuses when a key
//! file carries it.
//! herdr-2636-enroll (herdr#195) replaces this guard with its own key-loading gid handling.

use std::io;

pub(crate) const SETGID_REFUSAL: &str = "herdr: refusing to run with a set-group-id it cannot drop";
pub(crate) const HERDR_GROUP_REFUSAL: &str =
    "herdr: refusing to run with a group that may read the herdr server key";

const GROUP_FILE: &str = "/etc/group";
const KEY_FILES: [&str; 2] = ["/etc/herdr/server.key", "/etc/herdr/server.pub"];
const HERDR_GROUP: &str = "herdr";

/// The process gid facts the guard reads and changes. Tests inject a fake.
pub(crate) trait GroupIdSource {
    /// Real, effective and saved gid.
    fn resgid(&self) -> io::Result<[libc::gid_t; 3]>;
    /// Set real, effective and saved gid to `gid`.
    fn set_resgid(&mut self, gid: libc::gid_t) -> io::Result<()>;
    fn supplementary_groups(&self) -> io::Result<Vec<libc::gid_t>>;
    /// Contents of `/etc/group`.
    fn group_file(&self) -> io::Result<String>;
    /// Group owner of `path` without following a symlink; `Ok(None)` when it does not
    /// exist or this process cannot reach it. A symlink is an error.
    fn key_file_group(&self, path: &str) -> io::Result<Option<libc::gid_t>>;
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

    fn group_file(&self) -> io::Result<String> {
        std::fs::read_to_string(GROUP_FILE)
    }

    fn key_file_group(&self, path: &str) -> io::Result<Option<libc::gid_t>> {
        use std::os::unix::fs::MetadataExt;
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::other("key symlink")),
            Ok(meta) => Ok(Some(meta.gid())),
            // ponytail: the guard runs after the gid drop, so EACCES means neither this
            // process nor its children (same credentials) can reach the key.
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
            {
                Ok(None)
            }
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
    let mut key_gids: Vec<libc::gid_t> = [effective, saved]
        .into_iter()
        .filter(|gid| *gid != real)
        .collect();
    if effective != real || saved != real {
        ids.set_resgid(real).map_err(|_| SETGID_REFUSAL)?;
        if ids.resgid().map_err(|_| SETGID_REFUSAL)? != [real; 3] {
            return Err(SETGID_REFUSAL);
        }
    }
    let groups = ids
        .supplementary_groups()
        .map_err(|_| HERDR_GROUP_REFUSAL)?;
    // A missing or unreadable /etc/group fails closed.
    let group_file = ids.group_file().map_err(|_| HERDR_GROUP_REFUSAL)?;
    key_gids.extend(herdr_group_ids(&group_file));
    for path in KEY_FILES {
        key_gids.extend(ids.key_file_group(path).map_err(|_| HERDR_GROUP_REFUSAL)?);
    }
    if groups
        .iter()
        .chain(std::iter::once(&real))
        .any(|gid| key_gids.contains(gid))
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
        key_files: Vec<(&'static str, Option<libc::gid_t>, bool)>,
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
                key_files: Vec::new(),
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
        fn group_file(&self) -> io::Result<String> {
            self.group_file
                .clone()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
        /// Entries are (path, group owner, is a symlink).
        fn key_file_group(&self, path: &str) -> io::Result<Option<libc::gid_t>> {
            match self.key_files.iter().find(|(p, ..)| *p == path) {
                Some((_, _, true)) => Err(io::Error::other("key symlink")),
                Some((_, gid, false)) => Ok(*gid),
                None => Ok(None),
            }
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
    fn a_missing_group_file_refuses() {
        let mut ids = FakeIds::new([1000; 3]);
        ids.group_file = None;
        assert_eq!(enforce_group_privilege(&mut ids), Err(HERDR_GROUP_REFUSAL));
    }

    #[test]
    fn a_supplementary_gid_matching_only_the_key_file_group_refuses() {
        // gid 4242 is not in /etc/group (kept from an earlier login or NSS).
        for path in KEY_FILES {
            let mut ids = FakeIds::new([1000; 3]);
            ids.groups = vec![1000, 4242];
            ids.key_files = vec![(path, Some(4242), false)];
            assert_eq!(
                enforce_group_privilege(&mut ids),
                Err(HERDR_GROUP_REFUSAL),
                "{path}"
            );
        }
    }

    #[test]
    fn a_key_file_symlink_refuses() {
        let mut ids = FakeIds::new([1000; 3]);
        ids.key_files = vec![(KEY_FILES[0], None, true)];
        assert_eq!(enforce_group_privilege(&mut ids), Err(HERDR_GROUP_REFUSAL));
    }

    #[test]
    fn no_key_file_and_clean_groups_runs() {
        let mut ids = FakeIds::new([1000; 3]);
        ids.groups = vec![1000, 4242];
        assert_eq!(enforce_group_privilege(&mut ids), Ok(()));
    }

    #[test]
    fn a_key_file_in_a_group_the_process_lacks_runs() {
        let mut ids = FakeIds::new([1000; 3]);
        ids.groups = vec![1000, 4242];
        ids.key_files = vec![
            (KEY_FILES[0], Some(0), false),
            (KEY_FILES[1], Some(977), false),
        ];
        assert_eq!(enforce_group_privilege(&mut ids), Ok(()));
    }

    #[test]
    fn a_supplementary_gid_matching_the_dropped_setgid_refuses() {
        // setgid 4242: not named herdr in /etc/group and carried by no key file.
        let mut ids = FakeIds::new([1000, 4242, 4242]);
        ids.groups = vec![1000, 4242];
        assert_eq!(enforce_group_privilege(&mut ids), Err(HERDR_GROUP_REFUSAL));
    }

    #[test]
    fn the_real_key_file_lstat_does_not_follow_a_symlink() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("herdr-keygid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let target = dir.join("target");
        std::fs::write(&target, b"").expect("write");
        let link = dir.join("server.key");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let ids = ProcessGroupIds;
        let path = |p: &std::path::Path| p.to_str().expect("utf8").to_owned();
        assert!(ids.key_file_group(&path(&link)).is_err());
        assert_eq!(
            ids.key_file_group(&path(&target)).expect("lstat"),
            Some(std::fs::metadata(&target).expect("stat").gid())
        );
        assert_eq!(
            ids.key_file_group(&path(&dir.join("absent")))
                .expect("absent"),
            None
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
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
