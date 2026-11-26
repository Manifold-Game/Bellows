use super::*;

fn request(machine: &str, label: &str, priority: i32, pr: Option<u32>) -> Request {
    Request {
        machine: machine.into(),
        label: label.into(),
        priority,
        pr,
        estimate_secs: 600,
        session: false,
        token: None,
        client_pid: 0,
    }
}

fn queued(admission: Admission) -> u64 {
    match admission {
        Admission::Queued(id) => id,
        other => panic!("expected queued, got {other:?}"),
    }
}

#[test]
fn one_holder_per_machine_in_priority_then_merge_queue_then_arrival_order() {
    let mut s = Scheduler::new(Config::default());
    let first = queued(s.admit(request("laptop", "first", 0, None), 1));
    let late_pr = queued(s.admit(request("laptop", "pr 9", 0, Some(9)), 2));
    let early_pr = queued(s.admit(request("laptop", "pr 7", 0, Some(7)), 3));
    let urgent = queued(s.admit(request("laptop", "urgent", 5, None), 4));
    let desktop = queued(s.admit(request("desktop", "desktop", 0, None), 5));
    s.set_merge_queue(HashMap::from([(7, 0), (9, 1)]));

    let grants = s.grant(10);
    let granted = grants.iter().map(|grant| grant.id).collect::<Vec<_>>();
    assert_eq!(granted, [urgent, desktop], "both machines granted once");
    assert!(
        s.grant(11).is_empty(),
        "a held machine is not granted again"
    );

    let mut order = Vec::new();
    for id in [urgent, early_pr, late_pr] {
        s.release(id);
        let next = s.grant(20);
        order.push(next[0].id);
    }
    assert_eq!(order, [early_pr, late_pr, first]);
}

#[test]
fn sessions_hand_out_tokens_that_run_nested_until_preempted() {
    let mut s = Scheduler::new(Config::default());
    let mut session = request("desktop", "batch", 0, None);
    session.session = true;
    let id = queued(s.admit(session, 1));
    let grant = s.grant(2).remove(0);
    let token = grant.token.clone().expect("session token");

    let mut nested = request("desktop", "step", 0, None);
    nested.token = Some(token.clone());
    let Admission::Nested(inner) = s.admit(nested.clone(), 3) else {
        panic!("nested run should start at once")
    };
    assert!(inner.nested);
    s.release(inner.id);
    assert!(s.holds(id), "a nested run does not end its session");

    // A stranger's token queues like anyone else.
    let mut stranger = request("desktop", "stranger", 0, None);
    stranger.token = Some("1-0000".into());
    assert!(matches!(s.admit(stranger, 4), Admission::Queued(_)));

    assert!(s.preempt(id));
    assert!(matches!(s.admit(nested, 5), Admission::Refused(_)));
}

#[test]
fn administration_reorders_pauses_and_reserves() {
    let mut s = Scheduler::new(Config::default());
    let a = queued(s.admit(request("laptop", "talos a", 0, None), 1));
    let b = queued(s.admit(request("laptop", "enki b", 0, None), 2));
    let c = queued(s.admit(request("laptop", "talos c", 0, None), 3));
    assert!(s.reorder(c, 0));
    assert_eq!(s.position(c), Some(0));
    assert_eq!(s.position(a), Some(1));

    s.set_paused("laptop", true);
    assert!(s.grant(4).is_empty());
    s.set_paused("laptop", false);

    s.reserve("laptop", Some(("enki".into(), 100)));
    assert_eq!(s.grant(5)[0].id, b, "only the reserved label is granted");
    s.release(b);
    s.reserve("laptop", None);
    assert_eq!(s.grant(6)[0].id, c);
}

#[test]
fn ci_shares_the_machine_with_fewer_jobs_until_stopped_or_expired() {
    let mut s = Scheduler::new(Config::default());
    s.ci_start("laptop", "run-1/native-test-gpu", 1);
    queued(s.admit(request("laptop", "bee", 0, None), 2));
    assert_eq!(s.grant(3)[0].jobs, 12);
    assert_eq!(s.status(4).machines[0].ci_jobs, ["run-1/native-test-gpu"]);
    s.ci_stop("laptop", "run-1/native-test-gpu");
    assert_eq!(s.status(5).machines[0].jobs, 24);
    s.ci_start("laptop", "forgotten", 10);
    assert_eq!(s.status(10 + 3 * 60 * 60 * 1000 + 1).machines[0].jobs, 24);
}

#[test]
fn overruns_are_reported_once_past_one_and_a_half_estimates() {
    let mut s = Scheduler::new(Config::default());
    queued(s.admit(request("laptop", "long", 0, None), 0));
    s.grant(0);
    assert!(s.overruns(800_000).is_empty());
    assert_eq!(s.overruns(900_001).len(), 1);
    assert!(s.overruns(2_000_000).is_empty());
}

#[test]
fn unknown_machines_are_refused() {
    let mut s = Scheduler::new(Config::default());
    assert!(matches!(
        s.admit(request("toaster", "x", 0, None), 1),
        Admission::Refused(_)
    ));
}
