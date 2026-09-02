//! Task-lifecycle transitions: fork, exec, exit, and fsuid/fsgid
//! pre-commit. Fork prepares fallibly and returns an undo record so a
//! later clone failure can roll the keyring state back exactly.

use super::*;

impl KeyManager {
    pub(in crate::keyring) fn prepare_fork(
        &mut self,
        parent: KeyTaskOwner,
        child: KeyTaskOwner,
        clone_thread: bool,
        child_ruid: Kuid,
        child_rgid: Kgid,
    ) -> AxResult<ForkUndo> {
        if parent.thread_owner() == child.thread_owner()
            || clone_thread && parent.process_owner() != child.process_owner()
            || !clone_thread
                && (child.thread_owner() != child.process_owner()
                    || parent.process_owner() == child.process_owner())
            || self.thread_keyrings.contains_key(&child.thread_owner())
            || self.session_keyrings.contains_key(&child.thread_owner())
            || self.reqkey_defaults.contains_key(&child.thread_owner())
            || !clone_thread && self.process_keyrings.contains_key(&child.process_owner())
        {
            return Err(AxError::BadState);
        }

        let parent_thread =
            self.validate_lifecycle_root(RootSource::Thread(parent.thread_owner()))?;
        let parent_session =
            self.validate_lifecycle_root(RootSource::Session(parent.thread_owner()))?;
        if clone_thread {
            let _ = self.validate_lifecycle_root(RootSource::Process(parent.process_owner()))?;
        }
        if let Some(serial) = parent_session {
            self.keys
                .get(&serial)
                .ok_or(AxError::BadState)?
                .root_refs
                .checked_add(1)
                .ok_or(AxError::NoMemory)?;
        }
        let reqkey_default = self.reqkey_defaults.get(&parent.thread_owner()).copied();
        if reqkey_default.is_some_and(|setting| ReqKeyDefault::from_raw(setting).is_none()) {
            return Err(AxError::BadState);
        }

        let mut undo = ForkUndo {
            child,
            thread_keyring: None,
            session_keyring: None,
            reqkey_default,
        };
        if clone_thread && parent_thread.is_some() {
            undo.thread_keyring = Some(self.try_create_rooted_keyring(
                RootSource::Thread(child.thread_owner()),
                format!("_tid.{}", child.thread_owner()),
                child_ruid,
                child_rgid,
                thread_process_keyring_permissions(),
                QuotaAdmission::AllowOverrun,
            )?);
        }
        if let Some(serial) = parent_session {
            if let Err(error) = self.install_root(RootSource::Session(child.thread_owner()), serial)
            {
                self.rollback_fork(undo).unwrap_or_else(|rollback_error| {
                    panic!(
                        "prepared keyring fork rollback lost exact child ownership: \
                         {rollback_error}"
                    )
                });
                return Err(error);
            }
            undo.session_keyring = Some(serial);
        }
        if let Some(setting) = reqkey_default {
            self.reqkey_defaults.insert(child.thread_owner(), setting);
        }
        Ok(undo)
    }

    pub(in crate::keyring) fn rollback_fork(&mut self, undo: ForkUndo) -> AxResult<()> {
        let ForkUndo {
            child,
            thread_keyring,
            session_keyring,
            reqkey_default,
        } = undo;

        for (actual, expected) in [
            (
                self.thread_keyrings.get(&child.thread_owner()).copied(),
                thread_keyring,
            ),
            (
                self.session_keyrings.get(&child.thread_owner()).copied(),
                session_keyring,
            ),
        ] {
            if expected.is_none() && actual.is_some() || actual.is_some() && actual != expected {
                return Err(AxError::BadState);
            }
        }
        let actual_default = self.reqkey_defaults.get(&child.thread_owner()).copied();
        if reqkey_default.is_none() && actual_default.is_some()
            || actual_default.is_some() && actual_default != reqkey_default
        {
            return Err(AxError::BadState);
        }

        self.detach_expected_task_roots(
            [
                thread_keyring.map(|serial| ExpectedTaskRoot {
                    source: RootSource::Thread(child.thread_owner()),
                    serial,
                }),
                session_keyring.map(|serial| ExpectedTaskRoot {
                    source: RootSource::Session(child.thread_owner()),
                    serial,
                }),
                None,
            ],
            true,
        )?;
        if actual_default == reqkey_default {
            self.reqkey_defaults.remove(&child.thread_owner());
        }
        Ok(())
    }

    pub(in crate::keyring) fn exec_committed(&mut self, owner: KeyTaskOwner) -> AxResult<()> {
        self.abandon_construction(owner.thread_owner())?;
        let roots = [
            self.current_task_root(RootSource::Thread(owner.thread_owner()))?,
            self.current_task_root(RootSource::Process(owner.process_owner()))?,
            None,
        ];
        self.detach_expected_task_roots(roots, false)
    }

    pub(in crate::keyring) fn exit_committed(
        &mut self,
        owner: KeyTaskOwner,
        final_thread: bool,
    ) -> AxResult<()> {
        self.abandon_construction(owner.thread_owner())?;
        let roots = [
            self.current_task_root(RootSource::Thread(owner.thread_owner()))?,
            self.current_task_root(RootSource::Session(owner.thread_owner()))?,
            if final_thread {
                self.current_task_root(RootSource::Process(owner.process_owner()))?
            } else {
                None
            },
        ];
        self.detach_expected_task_roots(roots, false)?;
        self.reqkey_defaults.remove(&owner.thread_owner());
        Ok(())
    }

    fn abandon_construction(&mut self, thread_owner: u32) -> AxResult<()> {
        let Some(serial) = self.construction_authorities.remove(&thread_owner) else {
            return Ok(());
        };
        let key = self.keys.get(&serial).ok_or(AxError::BadState)?;
        if key.state != KeyState::Pending || key.construction_owner != Some(thread_owner) {
            return Err(AxError::BadState);
        }
        let result = self.remove_key_everywhere(serial);
        if result.is_ok() {
            self.remove_pending_construction(serial);
        }
        crate::keyring::service::notify_request_key_waiters();
        result
    }

    pub(in crate::keyring) fn credential_fsids_precommit(
        &mut self,
        thread_owner: u32,
        fsuid_change: Option<Kuid>,
        fsgid_change: Option<Kgid>,
    ) -> AxResult<()> {
        if fsuid_change.is_none() && fsgid_change.is_none() {
            return Ok(());
        }
        let Some(serial) = self.task_root_serial(RootSource::Thread(thread_owner))? else {
            return Ok(());
        };
        let key = self.keys.get_mut(&serial).ok_or(AxError::BadState)?;
        if !key.is_keyring()
            || key.root_refs == 0
            || !key.gc_plan.is_idle()
            || key.gc_next.is_some()
        {
            return Err(AxError::BadState);
        }
        if let Some(fsuid) = fsuid_change {
            key.uid = fsuid;
        }
        if let Some(fsgid) = fsgid_change {
            key.owner_gid = Some(fsgid);
        }
        Ok(())
    }
}
