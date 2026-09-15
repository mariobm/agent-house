//! Resident pause has no snapshot or disk lifecycle side effects. The persisted
//! intent lets adoption distinguish idle pause from an interrupted snapshot.
use super::*;
impl KrucibleBackend {
    pub(super) fn resume_resident(&self, id: &str) -> Result<bool> {
        let dir = {
            let mut inner = self.lock();
            let rec = inner
                .sandboxes
                .get_mut(id)
                .ok_or_else(|| Error::NotFound(id.into()))?;
            if rec.record.info.state != State::Paused && !rec.needs_resume {
                return Ok(false);
            }
            if !rec.worker.as_mut().is_some_and(|w| w.alive()) {
                return Ok(false);
            }
            rec.dir.clone()
        };
        recover_control(&dir)?;
        let mut inner = self.lock();
        let rec = inner.sandboxes.get_mut(id).expect("reserved VM");
        let mut record = rec.record.clone();
        record.info.state = State::Running;
        record.info.thermal = Thermal::Hot;
        self.persist_record(&dir, &record)?;
        rec.record = record;
        rec.needs_resume = false;
        Ok(true)
    }
    pub(super) fn pause_resident(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        let _guard = OpGuard::take(self, id)?;
        let dir = {
            let mut inner = self.lock();
            let rec = inner
                .sandboxes
                .get_mut(id)
                .ok_or_else(|| Error::NotFound(id.into()))?;
            if rec.record.spec.desktop {
                return Err(Error::InvalidState("desktop pause is not qualified".into()));
            }
            if !rec.worker.as_mut().is_some_and(|w| w.alive()) {
                return Err(Error::InvalidState("pause requires a live worker".into()));
            }
            if rec.record.info.state == State::Paused {
                return Ok(());
            }
            if rec.record.info.state != State::Running {
                return Err(Error::InvalidState("pause requires a running VM".into()));
            }
            let mut record = rec.record.clone();
            record.info.state = State::Paused;
            record.info.thermal = Thermal::Warm;
            self.persist_record(&rec.dir, &record)?;
            rec.record = record;
            rec.dir.clone()
        };
        let ctl = control_sock(&dir);
        if send_ctl(&ctl, "PAUSE").is_ok_and(|s| s == "OK paused")
            || send_ctl(&ctl, "STATUS").is_ok_and(|s| s == "OK paused")
        {
            return Ok(());
        }
        // A lost reply must not strand a running VM behind a false success.
        let recovered = recover_control(&dir).is_ok();
        let mut inner = self.lock();
        let rec = inner.sandboxes.get_mut(id).expect("reserved VM");
        rec.needs_resume = !recovered;
        rec.record.info.state = if recovered {
            State::Running
        } else {
            State::Failed
        };
        if recovered {
            rec.record.info.thermal = Thermal::Hot;
        }
        self.persist_record(&dir, &rec.record)?;
        Err(Error::Control("pause was not confirmed".into()))
    }
}
