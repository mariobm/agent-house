//! Immutable builder identity, image accounting and durable retention pins.
//! Guest projections never supply these provenance/reference records.
use crate::{Error, Result, Store};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectEnvironmentBuilder {
    pub id: String,
    pub sandbox_id: String,
    pub owner_user_id: String,
    pub request_json: String,
    pub metadata_json: String,
    pub phase: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectEnvironmentImage {
    pub id: String,
    pub owner_user_id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub metadata_json: String,
    pub request_json: String,
    pub source_volume_id: String,
    pub source_sandbox_id: String,
    pub source_logical_bytes: i64,
    pub logical_bytes: i64,
    pub phase: String,
    pub digest: Option<String>,
    pub image_path: Option<String>,
    pub detail: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn builder_row(r: &Row<'_>) -> rusqlite::Result<ProjectEnvironmentBuilder> {
    Ok(ProjectEnvironmentBuilder {
        id: r.get(0)?,
        sandbox_id: r.get(1)?,
        owner_user_id: r.get(2)?,
        request_json: r.get(3)?,
        metadata_json: r.get(4)?,
        phase: r.get(5)?,
        created_at: r.get(6)?,
    })
}
fn image_row(r: &Row<'_>) -> rusqlite::Result<ProjectEnvironmentImage> {
    Ok(ProjectEnvironmentImage {
        id: r.get(0)?,
        owner_user_id: r.get(1)?,
        tenant_id: r.get(2)?,
        project_id: r.get(3)?,
        metadata_json: r.get(4)?,
        request_json: r.get(5)?,
        source_volume_id: r.get(6)?,
        source_sandbox_id: r.get(7)?,
        source_logical_bytes: r.get(8)?,
        logical_bytes: r.get(9)?,
        phase: r.get(10)?,
        digest: r.get(11)?,
        image_path: r.get(12)?,
        detail: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
    })
}
const IMAGE_COLUMNS: &str = "id,owner_user_id,tenant_id,project_id,metadata_json,request_json,source_volume_id,source_sandbox_id,source_logical_bytes,logical_bytes,phase,digest,image_path,detail,created_at,updated_at";
fn image(conn: &Connection, id: &str) -> Result<ProjectEnvironmentImage> {
    conn.query_row(
        &format!("SELECT {IMAGE_COLUMNS} FROM project_environment_images WHERE id=?1"),
        [id],
        image_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound("project image".into()))
}
pub(crate) fn image_usage(conn: &Connection, owner: &str) -> Result<i64> {
    Ok(conn.query_row("SELECT COALESCE(SUM(logical_bytes),0) FROM project_environment_images WHERE owner_user_id=?1 AND phase!='retired'",[owner],|r|r.get(0))?)
}

impl Store {
    pub fn admit_project_builder(&self, row: &ProjectEnvironmentBuilder) -> Result<bool> {
        self.transaction(|tx| {
            let old = tx.tx.query_row("SELECT id,sandbox_id,owner_user_id,request_json,metadata_json,phase,created_at FROM project_environment_builders WHERE id=?1",[&row.id],builder_row).optional()?;
            if let Some(old) = old {
                if old.sandbox_id!=row.sandbox_id || old.owner_user_id!=row.owner_user_id || old.request_json!=row.request_json || old.metadata_json!=row.metadata_json {
                    return Err(Error::Conflict("builder identity already has another request".into()));
                }
                return Ok(false);
            }
            let occupied:bool=tx.tx.query_row("SELECT EXISTS(SELECT 1 FROM sandboxes WHERE id=?1) OR EXISTS(SELECT 1 FROM replicated_reservations WHERE sandbox_id=?1 AND logical_released=0)",[&row.sandbox_id],|r|r.get(0))?;
            if occupied { return Err(Error::Conflict("a project builder must be a fresh VM".into())); }
            tx.tx.execute("INSERT INTO project_environment_builders(id,sandbox_id,owner_user_id,request_json,metadata_json,phase,created_at) VALUES(?1,?2,?3,?4,?5,'creating',?6)",params![row.id,row.sandbox_id,row.owner_user_id,row.request_json,row.metadata_json,row.created_at])?;
            Ok(true)
        })
    }
    pub fn project_builder(&self, id: &str) -> Result<ProjectEnvironmentBuilder> {
        self.with_conn(|c|c.query_row("SELECT id,sandbox_id,owner_user_id,request_json,metadata_json,phase,created_at FROM project_environment_builders WHERE id=?1",[id],builder_row).optional()?.ok_or_else(||Error::NotFound("project builder".into())))
    }
    pub fn project_builder_for_sandbox(
        &self,
        id: &str,
    ) -> Result<Option<ProjectEnvironmentBuilder>> {
        self.with_conn(|c|Ok(c.query_row("SELECT id,sandbox_id,owner_user_id,request_json,metadata_json,phase,created_at FROM project_environment_builders WHERE sandbox_id=?1",[id],builder_row).optional()?))
    }
    pub fn set_project_builder_phase(&self, id: &str, phase: &str) -> Result<()> {
        if !["creating", "sealed", "captured", "retired", "failed"].contains(&phase) {
            return Err(Error::Conflict("invalid builder phase".into()));
        }
        self.with_conn(|c| {
            if c.execute(
                "UPDATE project_environment_builders SET phase=?2 WHERE id=?1",
                params![id, phase],
            )? != 1
            {
                return Err(Error::NotFound("project builder".into()));
            }
            Ok(())
        })
    }
    pub fn admit_project_image(&self, row: &ProjectEnvironmentImage) -> Result<bool> {
        if row.logical_bytes
            != row
                .source_logical_bytes
                .checked_add(65536)
                .ok_or_else(|| Error::Conflict("image accounting overflow".into()))?
            || row.source_logical_bytes <= 0
            || row.logical_bytes > 64 * 1024 * 1024 * 1024
            || row.source_logical_bytes % 65536 != 0
        {
            return Err(Error::Conflict("invalid project image geometry".into()));
        }
        self.transaction(|tx| {
            match image(&tx.tx,&row.id) {
                Ok(old) => {
                    if old.owner_user_id!=row.owner_user_id || old.tenant_id!=row.tenant_id || old.project_id!=row.project_id || old.request_json!=row.request_json || old.metadata_json!=row.metadata_json || old.source_volume_id!=row.source_volume_id || old.logical_bytes!=row.logical_bytes || old.source_sandbox_id!=row.source_sandbox_id {
                        return Err(Error::Conflict("image identity already has another capture".into()));
                    }
                    return Ok(false);
                }
                Err(Error::NotFound(_))=>{},Err(e)=>return Err(e),
            }
            let (owner,metadata):(String,String)=tx.tx.query_row("SELECT owner_user_id,metadata_json FROM project_environment_builders WHERE id=?1 AND sandbox_id=?2",params![row.id,row.source_sandbox_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
            let provenance:serde_json::Value=serde_json::from_str(&metadata)?;
            if owner!=row.owner_user_id || metadata!=row.metadata_json || provenance["tenant_id"]!=row.tenant_id || provenance["project_id"]!=row.project_id || provenance["build_id"]!=row.id {return Err(Error::Conflict("image builder provenance changed".into()));}
            let disk:i64=tx.tx.query_row("SELECT COALESCE(SUM(logical_bytes),0) FROM replicated_reservations WHERE owner_user_id=?1 AND logical_released=0",[&owner],|r|r.get(0))?;
            let (legacy,smallest):(i64,i64)=tx.tx.query_row("SELECT COALESCE(SUM(size_mb),0),COALESCE(MIN(size_mb),0) FROM volumes WHERE owner_user_id=?1",[&owner],|r|Ok((r.get(0)?,r.get(1)?)))?;
            let max:i64=tx.tx.query_row("SELECT max_volumes_mb FROM users WHERE id=?1",[&owner],|r|r.get(0))?;
            let total=disk.checked_add(image_usage(&tx.tx,&owner)?).and_then(|n|legacy.checked_mul(1024*1024).and_then(|l|n.checked_add(l))).and_then(|n|n.checked_add(row.logical_bytes)).ok_or_else(||Error::Conflict("image accounting overflow".into()))?;
            if smallest<0 || max<0 || total>max.checked_mul(1024*1024).ok_or_else(||Error::Conflict("invalid image quota".into()))? {
                return Err(Error::Conflict("project image storage quota exceeded".into()));
            }
            tx.tx.execute("INSERT INTO project_environment_images(id,owner_user_id,tenant_id,project_id,metadata_json,request_json,source_volume_id,source_sandbox_id,source_logical_bytes,logical_bytes,phase,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'preparing',?11,?11)",params![row.id,row.owner_user_id,row.tenant_id,row.project_id,row.metadata_json,row.request_json,row.source_volume_id,row.source_sandbox_id,row.source_logical_bytes,row.logical_bytes,row.created_at])?;
            tx.tx.execute("INSERT INTO project_environment_image_refs(kind,reference_id,image_id) VALUES('build',?1,?1)",[&row.id])?;
            Ok(true)
        })
    }
    pub fn project_image(&self, id: &str) -> Result<ProjectEnvironmentImage> {
        self.with_conn(|c| image(c, id))
    }
    pub fn project_image_usage(&self, owner: &str) -> Result<i64> {
        self.with_conn(|c| image_usage(c, owner))
    }
    pub fn project_storage_usage(&self, owner: &str) -> Result<(i64, i64, i64)> {
        self.with_conn(|c| {
            let (maximum,images,replicated,legacy,smallest):(i64,i64,i64,i64,i64)=c.query_row("SELECT max_volumes_mb,(SELECT COALESCE(SUM(logical_bytes),0) FROM project_environment_images WHERE owner_user_id=?1 AND phase!='retired'),(SELECT COALESCE(SUM(logical_bytes),0) FROM replicated_reservations WHERE owner_user_id=?1 AND logical_released=0),(SELECT COALESCE(SUM(size_mb),0) FROM volumes WHERE owner_user_id=?1),(SELECT COALESCE(MIN(size_mb),0) FROM volumes WHERE owner_user_id=?1) FROM users WHERE id=?1",[owner],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
            if maximum<0 || images<0 || replicated<0 || smallest<0 {return Err(Error::Conflict("invalid image storage accounting".into()));}
            let quota=maximum.checked_mul(1024*1024).ok_or_else(||Error::Conflict("image storage quota overflow".into()))?;
            let disks=legacy.checked_mul(1024*1024).and_then(|n|n.checked_add(replicated)).ok_or_else(||Error::Conflict("image storage usage overflow".into()))?;
            Ok((quota,images,disks))
        })
    }
    pub fn pending_project_image_bytes(&self) -> Result<i64> {
        self.with_conn(|c|Ok(c.query_row("SELECT COALESCE(SUM(logical_bytes),0) FROM project_environment_images WHERE phase IN ('preparing','capturing')",[],|r|r.get(0))?))
    }
    pub fn unfinished_project_images(&self) -> Result<Vec<ProjectEnvironmentImage>> {
        self.with_conn(|c| {
            let mut q=c.prepare(&format!("SELECT {IMAGE_COLUMNS} FROM project_environment_images WHERE phase IN ('preparing','capturing','deleting') ORDER BY created_at LIMIT 4096"))?;
            let rows=q.query_map([],image_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    pub fn set_project_image_phase(
        &self,
        id: &str,
        phase: &str,
        detail: Option<&str>,
        now: i64,
    ) -> Result<()> {
        if !["capturing", "failed"].contains(&phase) {
            return Err(Error::Conflict("invalid image phase".into()));
        }
        self.with_conn(|c| {
            if c.execute("UPDATE project_environment_images SET phase=?2,detail=?3,updated_at=?4 WHERE id=?1 AND ((?2='capturing' AND phase='preparing') OR (?2='failed' AND phase IN ('preparing','capturing')))",params![id,phase,detail,now])?!=1 {return Err(Error::Conflict("image phase already advanced or absent".into()));}
            Ok(())
        })
    }
    pub fn finish_project_image(&self, id: &str, digest: &str, path: &str, now: i64) -> Result<()> {
        self.with_conn(|c| {
            if c.execute("UPDATE project_environment_images SET phase='ready',digest=?2,image_path=?3,detail=NULL,updated_at=?4 WHERE id=?1 AND phase='capturing'",params![id,digest,path,now])?!=1 {return Err(Error::Conflict("image capture is not active".into()));}
            Ok(())
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub fn pin_project_image_vm(
        &self,
        id: &str,
        owner: &str,
        tenant: &str,
        project: &str,
        digest: &str,
        sandbox: &str,
    ) -> Result<()> {
        self.transaction(|tx| {
            let row=image(&tx.tx,id)?;
            if row.owner_user_id!=owner || row.tenant_id!=tenant || row.project_id!=project || row.digest.as_deref()!=Some(digest) || row.phase!="ready" {return Err(Error::Conflict("project image is not available for this project and generation".into()));}
            let prior:Option<String>=tx.tx.query_row("SELECT image_id FROM project_environment_image_refs WHERE kind='vm' AND reference_id=?1",[sandbox],|r|r.get(0)).optional()?;
            if prior.is_some_and(|p|p!=id) {return Err(Error::Conflict("VM already pins another project image".into()));}
            tx.tx.execute("INSERT OR IGNORE INTO project_environment_image_refs(kind,reference_id,image_id) VALUES('vm',?1,?2)",params![sandbox,id])?;
            Ok(())
        })
    }
    /// Caller must prove the original VM/volume has retired or never existed.
    pub fn release_project_image_vm(&self, sandbox: &str) -> Result<()> {
        self.transaction(|tx| {
            let retained:bool=tx.tx.query_row("SELECT EXISTS(SELECT 1 FROM sandboxes WHERE id=?1) OR EXISTS(SELECT 1 FROM replicated_reservations WHERE sandbox_id=?1 AND logical_released=0)",[sandbox],|r|r.get(0))?;
            if retained {return Err(Error::Conflict("VM image pin still owns a retained disk".into()));}
            tx.tx.execute("DELETE FROM project_environment_image_refs WHERE kind='vm' AND reference_id=?1",[sandbox])?;
            Ok(())
        })
    }
    pub fn project_image_vm(&self, sandbox: &str) -> Result<Option<ProjectEnvironmentImage>> {
        self.with_conn(|c| {
            let id:Option<String>=c.query_row("SELECT image_id FROM project_environment_image_refs WHERE kind='vm' AND reference_id=?1",[sandbox],|r|r.get(0)).optional()?;
            id.map(|id|image(c,&id)).transpose()
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub fn retain_project_images(
        &self,
        owner: &str,
        tenant: &str,
        project: &str,
        generation: i64,
        request_json: &str,
        current: Option<&str>,
        previous: Option<&str>,
        active: &[String],
        release: &[String],
    ) -> Result<()> {
        if generation < 1
            || active.len() > 64
            || release.len() > 64
            || active.iter().any(|id| release.contains(id))
            || (current.is_some() && current == previous)
        {
            return Err(Error::Conflict(
                "invalid project image retention set".into(),
            ));
        }
        self.transaction(|tx| {
            let old:Option<(i64,String)>=tx.tx.query_row("SELECT generation,request_json FROM project_environment_retention WHERE owner_user_id=?1 AND tenant_id=?2 AND project_id=?3",params![owner,tenant,project],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((prior,body))=old {
                if generation<prior || (generation==prior && body!=request_json) {return Err(Error::Conflict("project retention generation is stale or changed".into()));}
                if generation==prior {return Ok(());}
            }
            for (id,ready) in current.into_iter().map(|id|(id,true)).chain(previous.into_iter().map(|id|(id,true))).chain(active.iter().chain(release.iter()).map(|id|(id.as_str(),false))) {
                let row=image(&tx.tx,id)?;
                if row.owner_user_id!=owner || row.tenant_id!=tenant || row.project_id!=project || row.phase=="retired" || row.phase=="deleting" || (ready && row.phase!="ready") {return Err(Error::Conflict("retention image does not belong to this project or is unavailable".into()));}
                if release.iter().any(|r|r==id) && !["ready","failed"].contains(&row.phase.as_str()) {return Err(Error::Conflict("build pin still protects an unfinished capture".into()));}
            }
            tx.tx.execute("DELETE FROM project_environment_image_refs WHERE kind IN ('current','previous') AND image_id IN (SELECT id FROM project_environment_images WHERE owner_user_id=?1 AND tenant_id=?2 AND project_id=?3)",params![owner,tenant,project])?;
            let key=format!("{tenant}/{project}");
            for (kind,id) in [("current",current),("previous",previous)] {if let Some(id)=id {tx.tx.execute("INSERT INTO project_environment_image_refs(kind,reference_id,image_id) VALUES(?1,?2,?3)",params![kind,key,id])?;}}
            for id in active {tx.tx.execute("INSERT OR IGNORE INTO project_environment_image_refs(kind,reference_id,image_id) VALUES('build',?1,?1)",[id])?;}
            for id in release {tx.tx.execute("DELETE FROM project_environment_image_refs WHERE kind='build' AND reference_id=?1 AND image_id=?1",[id])?;}
            tx.tx.execute("INSERT INTO project_environment_retention(owner_user_id,tenant_id,project_id,generation,request_json) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(owner_user_id,tenant_id,project_id) DO UPDATE SET generation=excluded.generation,request_json=excluded.request_json",params![owner,tenant,project,generation,request_json])?;
            Ok(())
        })
    }
    pub fn begin_project_image_delete(
        &self,
        id: &str,
        owner: &str,
        now: i64,
    ) -> Result<ProjectEnvironmentImage> {
        self.transaction(|tx| {
            let row = image(&tx.tx, id)?;
            if row.owner_user_id != owner {
                return Err(Error::NotFound("project image".into()));
            }
            if row.phase == "retired" {
                return Ok(row);
            }
            let refs: i64 = tx.tx.query_row(
                "SELECT COUNT(*) FROM project_environment_image_refs WHERE image_id=?1",
                [id],
                |r| r.get(0),
            )?;
            if refs != 0 || ["preparing", "capturing"].contains(&row.phase.as_str()) {
                return Err(Error::Conflict(
                    "project image is referenced or capture is active".into(),
                ));
            }
            tx.tx.execute(
                "UPDATE project_environment_images SET phase='deleting',updated_at=?2 WHERE id=?1",
                params![id, now],
            )?;
            image(&tx.tx, id)
        })
    }
    pub fn finish_project_image_retirement(&self, id: &str, now: i64) -> Result<()> {
        self.transaction(|tx| {
            let row = image(&tx.tx, id)?;
            if row.phase == "retired" {
                return Ok(());
            }
            let refs: i64 = tx.tx.query_row(
                "SELECT COUNT(*) FROM project_environment_image_refs WHERE image_id=?1",
                [id],
                |r| r.get(0),
            )?;
            if row.phase != "deleting" || refs != 0 {
                return Err(Error::Conflict("image is still retained".into()));
            }
            tx.tx.execute(
                "UPDATE project_environment_images SET phase='retired',updated_at=?2 WHERE id=?1",
                params![id, now],
            )?;
            Ok(())
        })
    }
}
