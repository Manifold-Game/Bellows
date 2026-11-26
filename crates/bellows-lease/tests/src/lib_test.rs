use super::*;

#[test]
fn durations_parse_with_minutes_by_default() {
    assert_eq!(parse_duration_secs("20m").unwrap(), 1200);
    assert_eq!(parse_duration_secs("1h").unwrap(), 3600);
    assert_eq!(parse_duration_secs("90s").unwrap(), 90);
    assert_eq!(parse_duration_secs("15").unwrap(), 900);
    assert!(parse_duration_secs("soon").is_err());
    assert!(parse_duration_secs("5d").is_err());
}
