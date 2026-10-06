//! Overlay control attributes belong to the selected mount metadata namespace.
pub(super) fn control_key(userxattr: bool, trusted: &'static [u8]) -> &'static [u8] {
    if !userxattr {
        return trusted;
    }
    match trusted {
        b"trusted.overlay.whiteout" => b"user.overlay.whiteout",
        b"trusted.overlay.opaque" => b"user.overlay.opaque",
        b"trusted.overlay.origin" => b"user.overlay.origin",
        b"trusted.overlay.redirect" => b"user.overlay.redirect",
        b"trusted.overlay.index" => b"user.overlay.index",
        b"trusted.overlay.index.pending" => b"user.overlay.index.pending",
        b"trusted.overlay.index.target" => b"user.overlay.index.target",
        b"trusted.overlay.index.kind" => b"user.overlay.index.kind",
        b"trusted.overlay.tombstones" => b"user.overlay.tombstones",
        _ => unreachable!("overlay control key must be a declared private attribute"),
    }
}

pub(super) fn private_name(userxattr: bool, name: &[u8]) -> bool {
    name.starts_with(if userxattr {
        b"user.overlay."
    } else {
        b"trusted.overlay."
    })
}

#[cfg(test)]
mod tests {
    use axfs_ng_vfs::VfsError;

    use super::*;
    use crate::fs::overlay::OverlayMountOptions;

    #[test]
    fn flag_selects_real_control_namespace_and_rejects_unsafe_combinations() {
        let mut options = OverlayMountOptions::empty();
        options.set_option(b"lowerdir", b"/lower").unwrap();
        options.set_option(b"upperdir", b"/upper").unwrap();
        options.set_option(b"workdir", b"/work").unwrap();
        options.set_flag(b"userxattr").unwrap();
        assert!(options.features.userxattr);
        options.validate_shape().unwrap();
        assert_eq!(
            control_key(true, b"trusted.overlay.whiteout"),
            b"user.overlay.whiteout"
        );
        assert_eq!(
            control_key(false, b"trusted.overlay.whiteout"),
            b"trusted.overlay.whiteout"
        );
        assert!(private_name(true, b"user.overlay.origin"));
        assert!(!private_name(true, b"trusted.overlay.origin"));
        assert!(private_name(false, b"trusted.overlay.origin"));
        assert!(!private_name(false, b"user.overlay.origin"));
        assert_eq!(
            options.set_option(b"userxattr", b"on"),
            Err(VfsError::InvalidInput)
        );
        options.features.metacopy = true;
        assert_eq!(options.validate_shape(), Err(VfsError::InvalidInput));
        options.features.metacopy = false;
        options.features.redirect_dir = true;
        assert_eq!(options.validate_shape(), Err(VfsError::InvalidInput));
    }
}
