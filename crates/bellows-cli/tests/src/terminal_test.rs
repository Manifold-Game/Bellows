use super::*;

#[test]
fn event_lines_are_aligned_without_color() {
    assert_eq!(
        status(false, "hit", "serde", "restored remote compiler result"),
        "  CACHE HIT serde  restored remote compiler result"
    );
    assert_eq!(
        status(false, "miss", "manifold-engine", "no candidate"),
        " CACHE MISS manifold-engine  no candidate"
    );
}

#[test]
fn event_lines_apply_semantic_ansi_styles() {
    let line = status(true, "fallback", "serde", "remote unavailable");
    assert!(line.contains("\x1b[1;35m   FALLBACK\x1b[0m"));
    assert!(line.contains("\x1b[1mserde\x1b[0m"));
    assert!(line.contains("\x1b[2mremote unavailable\x1b[0m"));
}

#[test]
fn unknown_events_remain_readable() {
    assert_eq!(
        status(false, "future-event", "crate", "detail"),
        "    BELLOWS crate  detail"
    );
}

#[test]
fn output_levels_filter_live_decisions() {
    assert_eq!(Output::parse(None), Output::Events);
    assert_eq!(Output::parse(Some("SUMMARY")), Output::Summary);
    assert_eq!(Output::parse(Some("unknown")), Output::Events);
    assert!(Output::Events.prints("bypass"));
    assert!(!Output::Summary.prints("miss"));
    assert!(!Output::Summary.prints("bypass"));
    assert!(Output::Summary.prints("fallback"));
    assert!(Output::Summary.prints("corrupt"));
    assert!(!Output::Quiet.prints("fallback"));
    assert!(Output::Summary.prints_build_summary());
    assert!(!Output::Quiet.prints_build_summary());
}

#[test]
fn color_policy_honors_term_ci_and_explicit_overrides() {
    assert!(decide_color("auto", false, false, true, false));
    assert!(decide_color("auto", false, false, false, true));
    assert!(!decide_color("auto", true, false, true, true));
    assert!(!decide_color("auto", false, true, true, true));
    assert!(decide_color("always", true, true, false, false));
    assert!(!decide_color("never", false, false, true, true));
}
