use std::{fs, process};
use std::fs::{File, OpenOptions};
use std::sync::mpsc::Receiver;
use std::sync::{Arc};
use diesel::{QueryResult, SqliteConnection};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, Process, ProcessStatus, System};
use thiserror::Error;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tonic::{Response, Status};
use tracing::{debug, error, info, warn};
use crate::app_details::{APPLICATION, ORGANIZATION, QUALIFIER};
use crate::dao::VaultDao;

#[derive(Debug, Error, Serialize, Deserialize)]
pub enum VaultHandlerError {
    #[error("cannot lock vault")]
    CannotLockVault,
    #[error("cannot unlock vault")]
    CannotUnlockVault,
    #[error("cannot change mount point")]
    CannotChangeMountPoint,
    #[error("cannot change data dir")]
    CannotChangeDataDir,
}

pub struct VaultHandler {
    id: u32,
    child: Option<Child>,
    db_conn: Arc<Mutex<SqliteConnection>>,
}

impl VaultHandler {
    pub fn new(id: u32, db_conn: Arc<Mutex<SqliteConnection>>) -> Self {
        Self { id, child: None, db_conn }
    }

    pub async fn lock(&mut self) -> Result<(), VaultHandlerError> {
        info!("VaultHandler {} received lock request", self.id);

        let mut guard = self.db_conn.lock().await;
        let mut dao = VaultDao::new(&mut *guard);

        match self.db_update_locked(true, &mut dao).await {
            Ok(_) => {}
            Err(err) => {
                error!("Cannot update vault state {}", err);
                return Err(VaultHandlerError::CannotLockVault.into());
            }
        }

        if self.child.is_none() {
            info!("VaultHandler {} already locked", self.id);
            return Ok(());
        }
        info!("VaultHandler {} killing child process to lock the vault", self.id);
        if let Err(err) = self.child.take().unwrap().kill().await {
            error!("Error killing child process: {:?}", err);
            return Err(VaultHandlerError::CannotLockVault.into());
        }

        // for some reason of we use 'kill' method the child process doesn't receive the SIGKILL signal
        // for that case we use `umount` command
        // TODO: umount for windows
        if cfg!(any(linux, unix, macos, freebsd, openbsd, netbsd)) {
            match dao.get(self.id as i32) {
                Ok(vault) => {
                    process::Command::new("umount")
                        .arg(&vault.mount_point)
                        .output()
                        .expect("Cannot umount vault");
                }
                Err(err) => return {
                    error!("Cannot get vault {}", err);
                    return Err(VaultHandlerError::CannotLockVault.into());
                }
            }
        }

        Ok(())
    }

    pub async fn unlock(&mut self) -> Result<(), VaultHandlerError> {
        info!("VaultHandler {} received unlock request", self.id);

        if self.child.is_some() {
            info!("VaultHandler {} already unlocked", self.id);
            return Ok(());
        }

        let base_data_dir = if let Some(proj_dirs) = ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION) {
            proj_dirs.data_local_dir().to_path_buf()
        } else {
            error!("Cannot get project directories");
            panic!("Cannot get project directories");
        };
        // create logs files
        let stdout = OpenOptions::new().append(true).create(true).open(base_data_dir.join("logs").join(format!("vault_{}.out", self.id))).expect("Cannot create stdout file");
        let stderr = OpenOptions::new().append(true).create(true).open(base_data_dir.join("logs").join(format!("vault_{}.err", self.id))).expect("Cannot create stderr file");

        let mut guard = self.db_conn.lock().await;
        let mut dao = VaultDao::new(&mut *guard);
        let vault = match dao.get(self.id as i32) {
            Ok(vault) => vault,
            Err(err) => return {
                error!("Cannot get vault {}", err);
                return Err(VaultHandlerError::CannotLockVault.into());
            }
        };

        // spawn new process
        let child = Command::new("/home/gnome/dev/RustroverProjects/encrypted_fs/target/debug/encrypted_fs")
            // TODO get pass from keystore
            .env("ENCRYPTED_FS_PASSWORD", "pass-42")
            .stdout(stdout)
            .stderr(stderr)
            .arg("--mount-point")
            .arg(&vault.mount_point)
            .arg("--data-dir")
            .arg(&vault.data_dir)
            .arg("--umount-on-start")
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(err) => {
                error!("Cannot start process {}", err);
                return Err(VaultHandlerError::CannotUnlockVault.into());
            }
        };

        // wait few second and check if it started correctly
        tokio::time::sleep(tokio::time::Duration::from_secs(8)).await;
        if child.id().is_none() {
            return Err(VaultHandlerError::CannotUnlockVault.into());
        }
        let mut sys = System::new();
        sys.refresh_processes();
        let mut is_defunct = false;
        match sys.process(Pid::from_u32(child.id().unwrap())) {
            Some(process) => {
                println!("{:?}", process.status());
                if process.status() == ProcessStatus::Dead ||
                    process.status() == ProcessStatus::Zombie ||
                    process.status() == ProcessStatus::Stop {
                    warn!("Process is dead or zombie, killing it");
                    is_defunct = true;
                } else {
                    // try to check if it's defunct with ps command
                    // TODO: ps for windows
                    if cfg!(any(linux, unix, macos, freebsd, openbsd, netbsd)) {
                        let out = Command::new("ps")
                            .arg("-f")
                            .arg(child.id().unwrap().to_string())
                            .output().await
                            .expect("Cannot run ps command");
                        String::from_utf8(out.stdout).unwrap().lines().for_each(|line| {
                            if line.contains("defunct") {
                                warn!("Process is defunct, killing it");
                                is_defunct = true;
                            }
                        });
                    }
                }
            }
            None => return Err(VaultHandlerError::CannotUnlockVault.into())
        }
        if is_defunct {
            // TODO: kill for windows
            if cfg!(any(linux, unix, macos, freebsd, openbsd, netbsd)) {
                process::Command::new("kill")
                    .arg(child.id().unwrap().to_string())
                    .output()
                    .expect("Cannot kill process");
            }
            return Err(VaultHandlerError::CannotUnlockVault.into());
        }

        self.child = Some(child);

        match self.db_update_locked(false, &mut dao).await {
            Ok(_) => {}
            Err(err) => {
                error!("Cannot update vault state {}", err);
                return Err(VaultHandlerError::CannotUnlockVault.into());
            }
        }

        Ok(())
    }

    pub async fn change_mount_point(&mut self, mount_point_v: String) -> Result<(), VaultHandlerError> {
        use crate::schema::vaults::dsl::{mount_point};
        use diesel::ExpressionMethods;

        let unlocked = self.child.is_some();
        if unlocked {
            self.lock().await?;
        }

        {
            let mut guard = self.db_conn.lock().await;
            let mut dao = VaultDao::new(&mut *guard);
            match dao.update(self.id as i32, mount_point.eq(mount_point_v)) {
                Err(err) => {
                    error!("Cannot update vault {}", err);
                    return Err(VaultHandlerError::CannotChangeMountPoint.into());
                }
                Ok(_) => {}
            }
        }

        if unlocked {
            self.unlock().await?;
        }

        Ok(())
    }

    pub async fn change_data_dir(&mut self, data_dir_v: String) -> Result<(), VaultHandlerError> {
        use crate::schema::vaults::dsl::{data_dir};
        use diesel::ExpressionMethods;

        let unlocked = self.child.is_some();
        if unlocked {
            self.lock().await?;
        }

        {
            let mut guard = self.db_conn.lock().await;
            let mut dao = VaultDao::new(&mut *guard);
            match dao.update(self.id as i32, data_dir.eq(data_dir_v)) {
                Err(err) => {
                    error!("Cannot update vault {}", err);
                    return Err(VaultHandlerError::CannotChangeDataDir.into());
                }
                Ok(_) => {}
            }
            println!("Data dir updated {}", dao.get(self.id as i32).unwrap().data_dir);
        }

        if unlocked {
            self.unlock().await?;
        }

        Ok(())
    }

    async fn db_update_locked(&self, state: bool, mut dao: &mut VaultDao<'_>) -> QueryResult<()> {
        use crate::schema::vaults::dsl::{locked};
        use diesel::ExpressionMethods;

        dao.update(self.id as i32, locked.eq(if state { 1 } else { 0 }))
    }
}
