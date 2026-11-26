//! The lease state machine, free of I/O so its ordering is testable.
use crate::protocol::{HolderView, MachineStatus, QueuedView, Request, StatusReport};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Clone, Debug)]
pub struct Config {
    pub machines: Vec<String>,
    /// Build jobs a grant gets on an idle machine.
    pub jobs: u32,
    /// Build jobs a laptop grant gets while a CI job shares the machine.
    pub ci_jobs: u32,
    /// A CI registration nobody stopped is dropped after this long.
    pub ci_expiry_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            machines: vec!["laptop".into(), "desktop".into()],
            jobs: 24,
            ci_jobs: 12,
            ci_expiry_ms: 3 * 60 * 60 * 1000,
        }
    }
}

#[derive(Clone, Debug)]
struct Waiting {
    id: u64,
    request: Request,
    since_ms: u64,
    /// Set by an administrator: the position this request was moved to.
    manual_position: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Holder {
    pub id: u64,
    pub machine: String,
    pub label: String,
    pub since_ms: u64,
    pub estimate_secs: u64,
    pub session: bool,
    pub token: Option<String>,
    pub preempting: bool,
    pub overrun_logged: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub id: u64,
    pub machine: String,
    pub jobs: u32,
    pub token: Option<String>,
    pub nested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    Queued(u64),
    /// Runs at once inside the session whose token it carries.
    Nested(Grant),
    Refused(String),
}

#[derive(Default)]
pub struct Scheduler {
    config: Config,
    next_id: u64,
    waiting: Vec<Waiting>,
    holders: BTreeMap<String, Holder>,
    nested: HashMap<u64, String>,
    paused: BTreeSet<String>,
    reserved: HashMap<String, (String, u64)>,
    ci: BTreeMap<String, BTreeMap<String, u64>>,
    merge_queue: HashMap<u32, usize>,
    /// Grants made but not yet claimed by their waiting connection.
    unclaimed: HashMap<u64, Grant>,
    token_seed: u64,
}

impl Scheduler {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            next_id: 1,
            token_seed: crate::now_ms(),
            ..Self::default()
        }
    }

    pub fn knows(&self, machine: &str) -> bool {
        self.config.machines.iter().any(|known| known == machine)
    }

    pub fn admit(&mut self, request: Request, now_ms: u64) -> Admission {
        if !self.knows(&request.machine) {
            return Admission::Refused(format!(
                "unknown machine {:?} (known: {})",
                request.machine,
                self.config.machines.join(", ")
            ));
        }
        let id = self.next_id;
        self.next_id += 1;
        if let Some(token) = &request.token
            && let Some(holder) = self.holders.get(&request.machine)
            && holder.token.as_deref() == Some(token.as_str())
        {
            if holder.preempting {
                return Admission::Refused(
                    "the session was preempted: it ends at this boundary".into(),
                );
            }
            self.nested.insert(id, request.machine.clone());
            return Admission::Nested(Grant {
                id,
                jobs: self.jobs(&request.machine, now_ms),
                machine: request.machine,
                token: Some(token.clone()),
                nested: true,
            });
        }
        self.waiting.push(Waiting {
            id,
            request,
            since_ms: now_ms,
            manual_position: None,
        });
        Admission::Queued(id)
    }

    /// Requests for `machine` in grant order.
    fn order(&self, machine: &str) -> Vec<&Waiting> {
        let mut automatic = self
            .waiting
            .iter()
            .filter(|waiting| {
                waiting.request.machine == machine && waiting.manual_position.is_none()
            })
            .collect::<Vec<_>>();
        automatic.sort_by_key(|waiting| {
            (
                std::cmp::Reverse(waiting.request.priority),
                waiting
                    .request
                    .pr
                    .and_then(|pr| self.merge_queue.get(&pr).copied())
                    .unwrap_or(usize::MAX),
                waiting.since_ms,
                waiting.id,
            )
        });
        let mut manual = self
            .waiting
            .iter()
            .filter(|waiting| {
                waiting.request.machine == machine && waiting.manual_position.is_some()
            })
            .collect::<Vec<_>>();
        manual.sort_by_key(|waiting| (waiting.manual_position, waiting.id));
        for waiting in manual {
            let at = waiting.manual_position.unwrap_or(0).min(automatic.len());
            automatic.insert(at, waiting);
        }
        automatic
    }

    pub fn position(&self, id: u64) -> Option<usize> {
        let machine = &self
            .waiting
            .iter()
            .find(|waiting| waiting.id == id)?
            .request
            .machine;
        self.order(machine)
            .iter()
            .position(|waiting| waiting.id == id)
    }

    pub fn holder_label(&self, machine: &str) -> Option<String> {
        self.holders.get(machine).map(|holder| holder.label.clone())
    }

    pub fn machine_of(&self, id: u64) -> Option<String> {
        self.waiting
            .iter()
            .find(|waiting| waiting.id == id)
            .map(|waiting| waiting.request.machine.clone())
            .or_else(|| {
                self.holders
                    .values()
                    .find(|holder| holder.id == id)
                    .map(|holder| holder.machine.clone())
            })
    }

    fn jobs(&self, machine: &str, now_ms: u64) -> u32 {
        if self.ci_active(machine, now_ms) {
            self.config.ci_jobs
        } else {
            self.config.jobs
        }
    }

    fn ci_active(&self, machine: &str, now_ms: u64) -> bool {
        self.ci.get(machine).is_some_and(|jobs| {
            jobs.values()
                .any(|started| now_ms.saturating_sub(*started) < self.config.ci_expiry_ms)
        })
    }

    /// Grants every free machine to its first eligible request.
    pub fn grant(&mut self, now_ms: u64) -> Vec<Grant> {
        let mut grants = Vec::new();
        for machine in self.config.machines.clone() {
            if self.holders.contains_key(&machine) || self.paused.contains(&machine) {
                continue;
            }
            let reservation = self
                .reserved
                .get(&machine)
                .filter(|(_, until)| *until > now_ms)
                .map(|(label, _)| label.clone());
            let Some(id) = self
                .order(&machine)
                .into_iter()
                .find(|waiting| {
                    reservation
                        .as_deref()
                        .is_none_or(|label| waiting.request.label.contains(label))
                })
                .map(|waiting| waiting.id)
            else {
                continue;
            };
            let index = self
                .waiting
                .iter()
                .position(|waiting| waiting.id == id)
                .expect("ordered request is waiting");
            let waiting = self.waiting.remove(index);
            let token = waiting.request.session.then(|| self.token(id));
            let jobs = self.jobs(&machine, now_ms);
            self.holders.insert(
                machine.clone(),
                Holder {
                    id,
                    machine: machine.clone(),
                    label: waiting.request.label,
                    since_ms: now_ms,
                    estimate_secs: waiting.request.estimate_secs,
                    session: waiting.request.session,
                    token: token.clone(),
                    preempting: false,
                    overrun_logged: false,
                },
            );
            let grant = Grant {
                id,
                machine,
                jobs,
                token,
                nested: false,
            };
            self.unclaimed.insert(id, grant.clone());
            grants.push(grant);
        }
        grants
    }

    /// The grant made for `id`, once, whichever connection's call made it.
    pub fn claim(&mut self, id: u64) -> Option<Grant> {
        self.unclaimed.remove(&id)
    }

    fn token(&mut self, id: u64) -> String {
        self.token_seed = self
            .token_seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        format!("{id}-{:016x}", self.token_seed)
    }

    /// Ends a request or lease, whatever its state. Returns the holder it
    /// freed, if it was one.
    pub fn release(&mut self, id: u64) -> Option<Holder> {
        self.waiting.retain(|waiting| waiting.id != id);
        self.unclaimed.remove(&id);
        if self.nested.remove(&id).is_some() {
            return None;
        }
        let machine = self
            .holders
            .iter()
            .find(|(_, holder)| holder.id == id)
            .map(|(machine, _)| machine.clone())?;
        self.holders.remove(&machine)
    }

    pub fn is_waiting(&self, id: u64) -> bool {
        self.waiting.iter().any(|waiting| waiting.id == id)
    }

    pub fn holds(&self, id: u64) -> bool {
        self.holders.values().any(|holder| holder.id == id) || self.nested.contains_key(&id)
    }

    pub fn reorder(&mut self, id: u64, position: usize) -> bool {
        match self.waiting.iter_mut().find(|waiting| waiting.id == id) {
            Some(waiting) => {
                waiting.manual_position = Some(position);
                true
            }
            None => false,
        }
    }

    pub fn set_paused(&mut self, machine: &str, paused: bool) {
        if paused {
            self.paused.insert(machine.to_owned());
        } else {
            self.paused.remove(machine);
        }
    }

    pub fn reserve(&mut self, machine: &str, label: Option<(String, u64)>) {
        match label {
            Some(reservation) => {
                self.reserved.insert(machine.to_owned(), reservation);
            }
            None => {
                self.reserved.remove(machine);
            }
        }
    }

    /// Marks a session to end at its next boundary. Returns whether it held.
    pub fn preempt(&mut self, id: u64) -> bool {
        match self.holders.values_mut().find(|holder| holder.id == id) {
            Some(holder) => {
                holder.preempting = true;
                true
            }
            None => false,
        }
    }

    pub fn ci_start(&mut self, machine: &str, job: &str, now_ms: u64) {
        self.ci
            .entry(machine.to_owned())
            .or_default()
            .insert(job.to_owned(), now_ms);
    }

    pub fn ci_stop(&mut self, machine: &str, job: &str) {
        if let Some(jobs) = self.ci.get_mut(machine) {
            jobs.remove(job);
        }
    }

    pub fn set_merge_queue(&mut self, positions: HashMap<u32, usize>) {
        self.merge_queue = positions;
    }

    /// Holders past 1.5× their estimate, reported once each.
    pub fn overruns(&mut self, now_ms: u64) -> Vec<Holder> {
        let mut over = Vec::new();
        for holder in self.holders.values_mut() {
            let limit = holder.estimate_secs.saturating_mul(1500);
            if !holder.overrun_logged && now_ms.saturating_sub(holder.since_ms) > limit {
                holder.overrun_logged = true;
                over.push(holder.clone());
            }
        }
        over
    }

    pub fn status(&self, now_ms: u64) -> StatusReport {
        let machines = self
            .config
            .machines
            .iter()
            .map(|machine| MachineStatus {
                machine: machine.clone(),
                paused: self.paused.contains(machine),
                reserved_for: self
                    .reserved
                    .get(machine)
                    .filter(|(_, until)| *until > now_ms)
                    .map(|(label, _)| label.clone()),
                ci_jobs: self
                    .ci
                    .get(machine)
                    .map(|jobs| {
                        jobs.iter()
                            .filter(|(_, started)| {
                                now_ms.saturating_sub(**started) < self.config.ci_expiry_ms
                            })
                            .map(|(job, _)| job.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
                jobs: self.jobs(machine, now_ms),
                holder: self.holders.get(machine).map(|holder| HolderView {
                    id: holder.id,
                    label: holder.label.clone(),
                    held_secs: now_ms.saturating_sub(holder.since_ms) / 1000,
                    estimate_secs: holder.estimate_secs,
                    session: holder.session,
                    preempting: holder.preempting,
                }),
                queue: self
                    .order(machine)
                    .into_iter()
                    .map(|waiting| QueuedView {
                        id: waiting.id,
                        label: waiting.request.label.clone(),
                        priority: waiting.request.priority,
                        pr: waiting.request.pr,
                        merge_queue_position: waiting
                            .request
                            .pr
                            .and_then(|pr| self.merge_queue.get(&pr).copied()),
                        waited_secs: now_ms.saturating_sub(waiting.since_ms) / 1000,
                        estimate_secs: waiting.request.estimate_secs,
                    })
                    .collect(),
            })
            .collect();
        StatusReport { machines }
    }
}

#[cfg(test)]
#[path = "../tests/src/scheduler_test.rs"]
mod tests;
