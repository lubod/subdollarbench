use super::*;
use ratatui::{backend::TestBackend, Terminal};

fn campaign(id: &str, state: &str, model: &str, points: f64, cost: f64, ticket: u64) -> Value {
    serde_json::json!({
        "id": id, "state": state, "current_stage": ticket,
        "live": {"ticket": ticket, "turn": 19, "max_turns": 24, "title":"Aggregates",
                 "round":1, "ticket_spent":0.0412, "campaign_spent":cost,
                 "wall_elapsed_min":112, "wall_min":480},
        "rollup": {"model":model, "points":points, "max_points":76.0,
                   "total_cost_usd":cost, "budget_usd":0.99, "rules_version":"2026-10-19.42"},
        "ladder": [
            {"id":0,"title":"Genesis","status":"pass"},
            {"id":1,"title":"Expressions","status":"lite"},
            {"id":2,"title":"Sorting","status":"fail"},
            {"id":3,"title":"Updates","status":"fault"},
            {"id":4,"title":"Persistence","status":"not_reached"}],
        "stop_reason": if state == "done" { "budget cap" } else { "" }
    })
}
fn draw(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| render(f, app)).unwrap();
    let b = t.backend().buffer().clone();
    (0..h)
        .map(|y| (0..w).map(|x| b[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
#[test]
fn r42_f1_panic_restores_terminal() {
    let before = RESTORE_CALLS.load(Ordering::SeqCst);
    install_panic_hook();
    let r = std::panic::catch_unwind(|| panic!("restore seam"));
    assert!(r.is_err());
    assert!(RESTORE_CALLS.load(Ordering::SeqCst) > before);
}
#[test]
fn r42_f1_engine_unreachable() {
    let mut a = App::new();
    a.unreachable("connection refused");
    assert!(draw(&a, 120, 24).contains("engine ● unreachable (connection refused)"));
}
#[test]
fn r42_f2_cells_pane_render() {
    let mut a = App::new();
    a.set_campaigns(vec![
        campaign(
            "campaign_run",
            "running",
            "openrouter/a/long-model",
            7.8,
            0.6412,
            7,
        ),
        campaign(
            "campaign_stall",
            "stalled",
            "openrouter/b/stalled-model",
            5.8,
            0.7722,
            3,
        ),
        campaign(
            "campaign_done",
            "done",
            "openrouter/c/done-model",
            12.0,
            0.5,
            20,
        ),
    ]);
    for w in [120, 80] {
        let s = draw(&a, w, 30);
        for n in [
            "long-model",
            "stalled",
            "done-model",
            "7.8/76",
            "done · budget cap",
        ] {
            assert!(s.contains(n), "{n}:\n{s}");
        }
    }
}
#[test]
fn r42_f2_order_is_the_apis() {
    let mut a = App::new();
    a.set_campaigns(vec![
        campaign("campaign_b", "done", "b-model", 99.0, 0.1, 20),
        campaign("campaign_a", "done", "a-model", 1.0, 0.2, 0),
    ]);
    assert_eq!(a.campaigns()[0]["id"], "campaign_b");
    let s = draw(&a, 120, 20);
    assert!(
        s.find("b-model").unwrap() < s.find("a-model").unwrap(),
        "{s}"
    );
}
#[test]
fn r42_f3_ticket_table_render() {
    let mut a = App::new();
    a.set_campaigns(vec![campaign("campaign_t", "running", "m", 1.0, 0.1, 2)]);
    a.set_tickets(vec![
        serde_json::json!({"id":0,"title":"Genesis","status":"pass"}),
        serde_json::json!({"id":1,"title":"Expressions","status":"lite"}),
        serde_json::json!({"id":2,"title":"Sorting","status":"fail","cause":"expected ROWS"}),
        serde_json::json!({"id":3,"title":"Updates","status":"skipped","cause":"skipped: needs 2"}),
    ]);
    let s = draw(&a, 120, 30);
    for n in [
        "Genesis",
        "Expressions",
        "Sorting",
        "expected ROWS",
        "Updates",
        "skipped: needs 2",
    ] {
        assert!(s.contains(n), "{n}");
    }
}
#[test]
fn r42_f4_log_is_incremental() {
    let mut a = App::new();
    a.set_campaigns(vec![campaign("campaign_log", "running", "m", 1.0, 0.1, 2)]);
    assert!(a.apply_log_page(10, vec![serde_json::json!({"kind":"verdict","text":"one"})]));
    assert!(a.apply_log_page(
        20,
        vec![
            serde_json::json!({"kind":"verdict","text":"two"}),
            serde_json::json!({"kind":"verdict","text":"three"}),
            serde_json::json!({"kind":"verdict","text":"four"})
        ]
    ));
    assert_eq!(a.log_len(), 4);
    assert!(a.log_request_path(10).unwrap().contains("after=10"));
}
#[test]
fn r42_f4_kind_toggle_refetches() {
    let mut a = App::new();
    a.set_campaigns(vec![campaign("campaign_k", "running", "m", 1.0, 0.1, 2)]);
    a.apply_log_page(42, vec![serde_json::json!({"kind":"verdict","text":"x"})]);
    assert!(a.toggle_kind("tools"));
    assert_eq!(a.log_offset(), 0);
    assert!(a.log_kinds().contains("tools"));
    assert!(a
        .log_request_path(0)
        .unwrap()
        .contains("kinds=agent,tools,verdict"));
}
#[test]
fn r42_f4_live_pane_render() {
    let mut a = App::new();
    a.set_campaigns(vec![
        campaign("campaign_live", "running", "m/live", 7.8, 0.6412, 7),
        campaign("campaign_stall", "stalled", "m/stall", 5.8, 0.7722, 3),
    ]);
    a.apply_log_page(
        4,
        vec![serde_json::json!({"kind":"verdict","text":"Stage 7: Aggregates -> FAIL"})],
    );
    let s = draw(&a, 120, 30);
    assert!(s.contains("campaign $ 0.6412"));
    assert!(s.contains("Stage 7: Aggregates -> FAIL"));
    a.handle_key(KeyCode::Down);
    assert!(draw(&a, 120, 30).contains("stalled"));
}
#[test]
fn r42_f5_help_lists_every_key() {
    let mut a = App::new();
    a.handle_key(KeyCode::Char('?'));
    assert!(a.help_open());
    let s = draw(&a, 100, 24);
    for k in [
        "↑/↓", "Enter", "Esc", "Tab", "v", "a", "t", "s", "End", "?", "q",
    ] {
        assert!(s.contains(k), "help {k}:\n{s}");
    }
}
#[test]
fn r42_f5_header_states() {
    let mut a = App::new();
    a.set_status(serde_json::json!({"is_running":false,"rules_version":"2026-10-19.42","sandbox_image":"sha256:abc","queue":{"pending":0}}));
    assert!(draw(&a, 120, 10).contains("engine ● idle"));
    a.set_status(serde_json::json!({"is_running":true,"rules_version":"2026-10-19.42","sandbox_image":"sha256:abc","queue":{"pending":2}}));
    assert!(draw(&a, 120, 10).contains("engine ● running"));
    a.unreachable("refused");
    assert!(draw(&a, 120, 10).contains("engine ● unreachable"));
}
