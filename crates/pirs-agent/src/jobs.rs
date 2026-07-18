use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Running,
    Exited(i32),
    Killed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Bash,
    Agent,
}

pub struct Job {
    pub id: u64,
    pub kind: JobKind,
    pub description: String,
    pub group: Option<String>,
    pub status: JobStatus,
    pub output_path: PathBuf,
    pub started_at: u64,
    pub pid: Option<u32>,
    pub progress: Option<Arc<Mutex<String>>>,
    pub steer: Option<NotifyFn>,
}

impl Job {
    pub fn status_line(&self) -> String {
        let status = match &self.status {
            JobStatus::Running => "running".to_string(),
            JobStatus::Exited(code) => format!("exited({code})"),
            JobStatus::Killed => "killed".to_string(),
        };
        format!(
            "#{} [{}] {} — {}",
            self.id,
            status,
            kind_name(self.kind),
            self.description
        )
    }
}

fn kind_name(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Bash => "bash",
        JobKind::Agent => "agent",
    }
}

pub type NotifyFn = Arc<dyn Fn(String) + Send + Sync>;

pub struct JobRegistry {
    jobs: Mutex<HashMap<u64, Arc<Mutex<Job>>>>,
    next_id: Mutex<u64>,
    notifier: Mutex<Option<NotifyFn>>,
    waiters: Mutex<HashMap<u64, Vec<tokio::sync::oneshot::Sender<()>>>>,
    stop_flags: Mutex<HashMap<u64, std::sync::Arc<std::sync::atomic::AtomicBool>>>,
}

static REGISTRY: OnceLock<JobRegistry> = OnceLock::new();

pub fn registry() -> &'static JobRegistry {
    REGISTRY.get_or_init(|| JobRegistry {
        jobs: Mutex::new(HashMap::new()),
        next_id: Mutex::new(1),
        notifier: Mutex::new(None),
        waiters: Mutex::new(HashMap::new()),
        stop_flags: Mutex::new(HashMap::new()),
    })
}

impl JobRegistry {
    pub fn set_notifier(&self, notify: NotifyFn) {
        *self.notifier.lock().unwrap() = Some(notify);
    }

    pub fn notify(&self, message: impl Into<String>) {
        let cb = self.notifier.lock().unwrap().clone();
        if let Some(cb) = cb {
            cb(message.into());
        }
    }

    pub fn register(
        &self,
        kind: JobKind,
        description: String,
        output_path: PathBuf,
        pid: Option<u32>,
    ) -> (u64, Arc<Mutex<Job>>) {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            let id = *n;
            *n += 1;
            id
        };
        let job = Arc::new(Mutex::new(Job {
            id,
            kind,
            description,
            group: None,
            status: JobStatus::Running,
            output_path,
            started_at: pirs_ai::now_millis(),
            pid,
            progress: None,
            steer: None,
        }));
        self.jobs.lock().unwrap().insert(id, Arc::clone(&job));
        (id, job)
    }

    pub fn register_stop_flag(&self, id: u64, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.stop_flags.lock().unwrap().insert(id, flag);
    }

    pub fn request_stop(&self, id: u64) -> bool {
        if let Some(flag) = self.stop_flags.lock().unwrap().get(&id) {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    pub fn set_status(&self, id: u64, status: JobStatus) {
        if let Some(job) = self.jobs.lock().unwrap().get(&id) {
            job.lock().unwrap().status = status.clone();
        }
        if !matches!(status, JobStatus::Running) {
            if let Some(list) = self.waiters.lock().unwrap().remove(&id) {
                for tx in list {
                    let _ = tx.send(());
                }
            }
        }
    }

    pub async fn wait(&self, id: u64, timeout: std::time::Duration) -> Option<JobStatus> {
        let rx = {
            let current = self.get(id)?;
            let status = current.lock().unwrap().status.clone();
            if !matches!(status, JobStatus::Running) {
                return Some(status);
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.waiters.lock().unwrap().entry(id).or_default().push(tx);
            rx
        };
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(())) => self.get(id).map(|j| j.lock().unwrap().status.clone()),
            _ => None,
        }
    }

    pub fn set_group(&self, id: u64, group: impl Into<String>) {
        if let Some(job) = self.jobs.lock().unwrap().get(&id) {
            job.lock().unwrap().group = Some(group.into());
        }
    }

    pub fn set_progress_handle(&self, id: u64, progress: Arc<Mutex<String>>) {
        if let Some(job) = self.jobs.lock().unwrap().get(&id) {
            job.lock().unwrap().progress = Some(progress);
        }
    }

    pub fn set_steer(&self, id: u64, steer: NotifyFn) {
        if let Some(job) = self.jobs.lock().unwrap().get(&id) {
            job.lock().unwrap().steer = Some(steer);
        }
    }

    pub fn steer(&self, id: u64, message: &str) -> Result<(), String> {
        let jobs = self.jobs.lock().unwrap();
        let Some(job) = jobs.get(&id) else {
            return Err(format!("no such job: {id}"));
        };
        let job = job.lock().unwrap();
        let Some(steer) = &job.steer else {
            return Err(format!("job {id} is not steerable"));
        };
        steer(message.to_string());
        Ok(())
    }

    pub fn get(&self, id: u64) -> Option<Arc<Mutex<Job>>> {
        self.jobs.lock().unwrap().get(&id).cloned()
    }

    pub fn list(&self) -> Vec<String> {
        self.jobs
            .lock()
            .unwrap()
            .values()
            .map(|j| {
                let job = j.lock().unwrap();
                match &job.group {
                    Some(g) => format!("[{g}] {}", job.status_line()),
                    None => job.status_line(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_list_steer() {
        let r = registry();
        let (id, _job) = r.register(
            JobKind::Bash,
            "sleep 1".into(),
            PathBuf::from("/tmp/x.log"),
            Some(1),
        );
        assert!(r.list().iter().any(|l| l.contains(&format!("#{id}"))));
        assert!(r.steer(id, "hi").is_err(), "bash jobs are not steerable");
        let got = Arc::new(Mutex::new(String::new()));
        let got2 = Arc::clone(&got);
        r.set_steer(
            id,
            Arc::new(move |m| {
                *got2.lock().unwrap() = m;
            }),
        );
        r.steer(id, "hello job").unwrap();
        assert_eq!(*got.lock().unwrap(), "hello job");
        r.set_status(id, JobStatus::Exited(0));
        assert!(r.list().iter().any(|l| l.contains("exited(0)")));
    }

    #[test]
    fn notifier_fires() {
        let r = registry();
        let got = Arc::new(Mutex::new(String::new()));
        let got2 = Arc::clone(&got);
        r.set_notifier(Arc::new(move |m| {
            *got2.lock().unwrap() = m;
        }));
        r.notify("job done");
        assert_eq!(*got.lock().unwrap(), "job done");
    }
}
