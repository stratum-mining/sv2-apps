use std::collections::{HashMap, VecDeque};

use stratum_apps::stratum_core::channels_sv2::client::MAX_PAST_JOBS;

/// Bounded active and past-job storage for the SV1 side of the translator.
///
/// This mirrors the client-channel lifecycle in `channels_sv2`: one active job and at most
/// `MAX_PAST_JOBS` jobs retired under the current chain tip. SV1 future jobs need no equivalent
/// because tProxy does not advertise them to miners before activation.
#[derive(Debug)]
pub(super) struct Sv1JobStore<T> {
    active_job: Option<(String, T)>,
    past_jobs: HashMap<String, T>,
    past_job_order: VecDeque<String>,
}

impl<T> Default for Sv1JobStore<T> {
    fn default() -> Self {
        Self {
            active_job: None,
            past_jobs: HashMap::new(),
            past_job_order: VecDeque::new(),
        }
    }
}

impl<T> Sv1JobStore<T> {
    /// Installs a new active job.
    ///
    /// When `clean_jobs` is true, all previous work is invalidated. Otherwise the displaced
    /// active job remains available for late-share validation, subject to `MAX_PAST_JOBS`.
    pub(super) fn activate(&mut self, job_id: String, job: T, clean_jobs: bool) {
        if clean_jobs {
            self.clear();
        } else {
            // An upstream may reuse a job ID. Keep only its newest value and ordering entry.
            self.remove_past(&job_id);
            if let Some((active_job_id, active_job)) = self.active_job.take() {
                if active_job_id != job_id {
                    self.past_job_order.push_back(active_job_id.clone());
                    self.past_jobs.insert(active_job_id, active_job);
                }
            }

            while self.past_jobs.len() > MAX_PAST_JOBS {
                if let Some(evicted_job_id) = self.past_job_order.pop_front() {
                    self.past_jobs.remove(&evicted_job_id);
                }
            }
        }

        self.active_job = Some((job_id, job));
    }

    pub(super) fn get(&self, job_id: &str) -> Option<&T> {
        self.active_job
            .as_ref()
            .filter(|(active_job_id, _)| active_job_id == job_id)
            .map(|(_, active_job)| active_job)
            .or_else(|| self.past_jobs.get(job_id))
    }

    pub(super) fn active(&self) -> Option<&T> {
        self.active_job.as_ref().map(|(_, job)| job)
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        usize::from(self.active_job.is_some()) + self.past_jobs.len()
    }

    fn remove_past(&mut self, job_id: &str) {
        if self.past_jobs.remove(job_id).is_some() {
            self.past_job_order
                .retain(|past_job_id| past_job_id != job_id);
        }
    }

    fn clear(&mut self) {
        self.active_job = None;
        self.past_jobs.clear();
        self.past_job_order.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_one_active_and_bounded_past_jobs() {
        let mut jobs = Sv1JobStore::default();

        for job_id in 0..MAX_PAST_JOBS + 2 {
            jobs.activate(job_id.to_string(), job_id, false);
        }

        assert_eq!(jobs.len(), MAX_PAST_JOBS + 1);
        assert!(jobs.get("0").is_none());
        assert_eq!(jobs.get("1"), Some(&1));
        assert_eq!(jobs.active(), Some(&(MAX_PAST_JOBS + 1)));
    }

    #[test]
    fn clean_job_discards_active_and_past_jobs() {
        let mut jobs = Sv1JobStore::default();
        jobs.activate("old".to_string(), 1, false);
        jobs.activate("current".to_string(), 2, false);

        jobs.activate("new-tip".to_string(), 3, true);

        assert_eq!(jobs.len(), 1);
        assert!(jobs.get("old").is_none());
        assert!(jobs.get("current").is_none());
        assert_eq!(jobs.get("new-tip"), Some(&3));
    }

    #[test]
    fn replacing_a_job_id_keeps_a_single_entry() {
        let mut jobs = Sv1JobStore::default();
        jobs.activate("job".to_string(), 1, false);
        jobs.activate("other".to_string(), 2, false);

        jobs.activate("job".to_string(), 3, false);

        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs.get("job"), Some(&3));
        assert_eq!(jobs.get("other"), Some(&2));
    }
}
