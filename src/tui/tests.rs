use super::*;
use ratatui::{backend::TestBackend, Terminal};

fn campaign(id: &str, state: &str, model: &str, points: f64, cost: f64, ticket: u64) -> Value {
    // R55 F2: /api/campaigns' campaign_spent counts finished tickets; a
    // live ticket's 0.0412 is added by the TUI.
    let finished = if matches!(state, "running" | "stalled") {
        (cost - 0.0412).max(0.0)
    } else {
        cost
    };
    serde_json::json!({
        "id": id, "state": state, "current_stage": ticket,
        "live": {"ticket": ticket, "turn": 19, "max_turns": 24, "title":"Aggregates",
                 "round":1, "ticket_spent":0.0412, "campaign_spent":finished,
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
    a.handle_key(KeyCode::Down, KeyModifiers::NONE);
    assert!(draw(&a, 120, 30).contains("stalled"));
}
#[test]
fn r42_f5_help_lists_every_key() {
    let mut a = App::new();
    a.handle_key(KeyCode::Char('?'), KeyModifiers::NONE);
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
    assert!(draw(&a, 120, 10).contains("cells ● idle"));
    a.set_campaigns(vec![campaign("campaign_h", "running", "m", 1.0, 0.1, 1)]);
    a.set_status(serde_json::json!({"is_running":true,"rules_version":"2026-10-19.42","sandbox_image":"sha256:abc","queue":{"pending":2}}));
    assert!(draw(&a, 120, 10).contains("cells ● 1 running"));
    a.unreachable("refused");
    assert!(draw(&a, 120, 10).contains("engine ● unreachable"));
}

/// R52 F2: a real /api/campaigns row (captured fixture); the test sets a
/// running state and a start 37 minutes ago without adding fields the API
/// does not serve.
fn api_cell() -> Value {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/api-campaigns.json")).unwrap();
    let mut v = fixture
        .as_array()
        .and_then(|rows| rows.first())
        .cloned()
        .expect("api-campaigns fixture rows");
    v["state"] = serde_json::json!("running");
    v["started_at"] =
        serde_json::json!((chrono::Utc::now() - chrono::Duration::minutes(37)).to_rfc3339());
    v["wall_cap_min"] = serde_json::json!(480);
    assert!(v["live"].get("wall_elapsed_min").is_none());
    assert!(v["live"].get("wall_min").is_none());
    v
}

fn header_of(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim_end()
}

#[test]
fn r52_f2_header_fits_80() {
    let mut a = App::new();
    a.set_status(serde_json::json!({"rules_version":"2026-10-19.42","sandbox_image":"sha256:abcdef","queue":{"pending":1}}));
    a.set_campaigns(vec![api_cell()]);
    for w in [80u16, 200] {
        let s = draw(&a, w, 20);
        let h = header_of(&s);
        assert!(h.ends_with("?help q quit"), "width {w}: {h}");
        assert!(h.chars().count() <= w as usize, "width {w}: {h}");
    }
}

#[test]
fn r52_f2_header_counts_running_cells() {
    let mut a = App::new();
    a.set_status(serde_json::json!({"is_running":false,"rules_version":"2026-10-19.42","queue":{"pending":0}}));
    let mut one = api_cell();
    one["id"] = serde_json::json!("campaign_one");
    let mut two = api_cell();
    two["id"] = serde_json::json!("campaign_two");
    a.set_campaigns(vec![one, two]);
    let h = header_of(&draw(&a, 120, 20)).to_string();
    assert!(h.contains("cells ● 2 running"), "{h}");
    assert!(!h.contains("engine ● idle"), "{h}");
}

#[test]
fn r52_f2_wall_from_api_fields() {
    let mut a = App::new();
    a.set_status(serde_json::json!({"rules_version":"2026-10-19.42","queue":{"pending":0}}));
    a.set_campaigns(vec![api_cell()]);
    let s = draw(&a, 120, 30);
    assert!(s.contains("wall 37/480 m"), "{s}");
}

#[test]
fn r42_f1_ctrl_c_quits() {
    let mut a = App::new();
    assert!(a.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(!a.handle_key(KeyCode::Char('c'), KeyModifiers::NONE));
}

#[test]
fn r42_f2_selection_resets_log_and_tickets() {
    let mut a = App::new();
    a.set_campaigns(vec![
        campaign("campaign_a", "running", "a-model", 1.0, 0.1, 1),
        campaign("campaign_b", "running", "b-model", 2.0, 0.2, 2),
    ]);
    a.apply_log_page(40, vec![serde_json::json!({"kind":"verdict","text":"old"})]);
    a.set_tickets(vec![
        serde_json::json!({"id":0,"title":"old","status":"pass"}),
    ]);
    a.handle_key(KeyCode::Esc, KeyModifiers::NONE);
    a.handle_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(a.log_len(), 0);
    assert_eq!(a.log_offset(), 0);
    assert_eq!(a.tickets_len(), 0);
    let path = a.log_request_path(0).unwrap();
    assert!(path.contains("/campaign_b/log"), "{path}");
    assert!(path.contains("tail=400"), "{path}");
}

#[test]
fn r42_f2_selection_survives_poll_reorder() {
    let mut a = App::new();
    a.set_campaigns(vec![
        campaign("campaign_a", "running", "a-model", 1.0, 0.1, 1),
        campaign("campaign_b", "running", "b-model", 2.0, 0.2, 2),
    ]);
    a.handle_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(a.selected_id(), Some("campaign_b"));
    a.apply_log_page(9, vec![serde_json::json!({"kind":"verdict","text":"keep"})]);
    a.set_campaigns(vec![
        campaign("campaign_b", "running", "b-model", 2.0, 0.2, 2),
        campaign("campaign_a", "running", "a-model", 1.0, 0.1, 1),
    ]);
    assert_eq!(a.selected_id(), Some("campaign_b"));
    assert_eq!(a.log_len(), 1);
    a.set_campaigns(vec![campaign(
        "campaign_a",
        "running",
        "a-model",
        1.0,
        0.1,
        1,
    )]);
    assert_eq!(a.selected_id(), Some("campaign_a"));
    assert_eq!(a.log_len(), 0);
}

/// R55 F1: every career phase uses the 76-point denominator, while a cell
/// outside a career keeps its own max_points. A running phase adds the
/// chain's points.
#[test]
fn r55_f1_points_scale_per_row() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/api-campaigns.json")).unwrap();
    let rows = fixture.as_array().expect("fixture rows").clone();
    let mut a = App::new();
    a.set_campaigns(rows);
    let s = draw(&a, 120, 30);
    for want in ["0.0/76", "10.0/76", "1.0/76", "6.0/76"] {
        assert!(s.contains(want), "missing {want}:\n{s}");
    }
    assert!(!s.contains("/10"), "a career row used /10:\n{s}");

    let mut plain = campaign("plain", "running", "m", 4.0, 0.1, 1);
    plain["rollup"]["max_points"] = serde_json::json!(10.0);
    let mut b = App::new();
    b.set_campaigns(vec![plain]);
    assert!(draw(&b, 120, 20).contains("4.0/10"));

    let mut p1 = campaign("p1", "done", "m", 10.0, 0.1, 1);
    p1["rollup"]["phase"] = serde_json::json!(1);
    let mut p2 = campaign("p2", "running", "m", 2.0, 0.1, 1);
    p2["rollup"]["phase"] = serde_json::json!(2);
    p2["rollup"]["chain_from"] = serde_json::json!("p1");
    let mut c = App::new();
    c.set_campaigns(vec![p1, p2]);
    assert!(draw(&c, 120, 20).contains("12.0/76"));
}

/// R55 F2: a running ticket's spend is added to the finished-ticket
/// campaign spend, so campaign $ is never below ticket $.
#[test]
fn r55_f2_campaign_spend_counts_ticket() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/api-campaigns.json")).unwrap();
    let rows = fixture.as_array().expect("fixture rows").clone();
    let mut a = App::new();
    a.set_campaigns(rows);
    let s = draw(&a, 120, 30);
    assert!(s.contains("campaign $ 0.0345"), "w0:\n{s}");
    a.handle_key(KeyCode::Down, KeyModifiers::NONE);
    a.handle_key(KeyCode::Down, KeyModifiers::NONE);
    let s = draw(&a, 120, 30);
    assert!(s.contains("campaign $ 0.0440"), "w1:\n{s}");
}

/// R55 F3: the header's image comes from /api/status; the captured
/// fixture has no field, so the test adds the response's new field.
#[test]
fn r55_f3_header_image_from_status() {
    let raw: Value = serde_json::from_str(include_str!("fixtures/api-status.json")).unwrap();
    assert!(raw.get("sandbox_image").is_none());
    let mut a = App::new();
    a.set_status(raw.clone());
    let h = header_of(&draw(&a, 120, 10)).to_string();
    assert!(!h.contains("image "), "no image field when absent: {h}");
    let mut with = raw;
    with["sandbox_image"] = serde_json::json!("sha256:abcdef1234567890");
    a.set_status(with);
    let h = header_of(&draw(&a, 120, 10)).to_string();
    assert!(h.contains("image abcdef123456"), "{h}");
}
