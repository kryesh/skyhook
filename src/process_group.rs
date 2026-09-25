//! A spawned child's process group, so stopping it also reaches descendants.

/// The process group led by a child spawned with `process_group(0)` or `setsid`.
/// The group is killed at most once: by [`kill`](Self::kill) or on drop, unless
/// [`release`](Self::release)d.
pub(crate) struct ProcessGroup(Option<libc::pid_t>);

impl ProcessGroup {
    /// The group of a just-spawned child that leads its own group.
    pub(crate) fn led_by(child: &tokio::process::Child) -> Self {
        Self(child.id().and_then(|id| libc::pid_t::try_from(id).ok()))
    }

    /// SIGKILL every process in the group.
    pub(crate) fn kill(&mut self) {
        if let Some(leader) = self.0.take() {
            // SAFETY: kill takes integer IDs and accesses no memory. The negative
            // ID targets only the group our child was spawned to lead.
            unsafe {
                libc::kill(-leader, libc::SIGKILL);
            }
        }
    }

    /// Stop owning the group without signalling it.
    pub(crate) fn release(mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}
