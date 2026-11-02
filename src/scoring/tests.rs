use super::*;

/// R26 R23-7 acceptance: with a dollar budget and the 200 guard, the
/// first session gets the full guard, not the retired 50-turn checkpoint.
#[test]
fn r26_first_session_gets_the_full_guard_not_50() {
    assert_eq!(initial_turn_limit(TURN_GUARD), Some(200));
    assert_eq!(initial_turn_limit(50), Some(50));
    assert_eq!(initial_turn_limit(0), None);
}

/// R26 R23-7 acceptance: a repair's turn bound is the guard remainder,
/// not the retired 25-turn cap.
#[test]
fn r26_repair_gets_the_guard_remainder_not_25() {
    assert_eq!(repair_turn_allowance(200, 50), TurnAllowance::Capped(150));
    assert_eq!(repair_turn_allowance(200, 199), TurnAllowance::Capped(1));
    assert_eq!(repair_turn_allowance(200, 200), TurnAllowance::Exhausted);
    assert_eq!(repair_turn_allowance(200, 240), TurnAllowance::Exhausted);
    assert_eq!(repair_turn_allowance(0, 50), TurnAllowance::Uncapped);
}

/// R26 R23-7 acceptance: dollars decide whether a repair runs. Above the
/// floor it runs with the money left; below it, it is skipped.
#[test]
fn r26_repair_runs_with_money_and_is_skipped_below_the_floor() {
    let (budget, cap) = repair_round_spend(0.99, 0.455, None).expect("money left");
    assert_eq!(budget, Some(0.99 - 0.455));
    assert_eq!(cap, None);

    let err = repair_round_spend(0.99, 0.98, None).expect_err("below the floor");
    assert!(err.contains("budget floor"), "{err}");
}
