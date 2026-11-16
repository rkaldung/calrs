// REST API invites: `POST /api/v1/invites` names an event type by its page
// and follows the dashboard's and MCP's distribution rules.

use super::api::{api_post, error_code, pat, send, user_id, ALL};
use super::*;
use axum::http::StatusCode;
use serde_json::{json, Value};

async fn set_visibility(pool: &SqlitePool, et_id: &str, visibility: &str) {
    sqlx::query("UPDATE event_types SET visibility = ? WHERE id = ?")
        .bind(visibility)
        .bind(et_id)
        .execute(pool)
        .await
        .unwrap();
}

fn token_of(url: &str) -> &str {
    url.split("invite=").nth(1).unwrap()
}

#[tokio::test]
async fn api_invites_follow_the_distribution_rules() {
    let _lock = crate::test_support::ENV_LOCK.lock().await;
    let _env = super::oauth::environment();
    let (app, pool, _, et_id) = setup_test_app().await;
    let owner = user_id(&pool, "testuser").await;
    let other = insert_role_user(&pool, "other@test.com", "user").await;
    let admin = insert_role_user(&pool, "boss@test.com", "admin").await;
    sqlx::query("UPDATE auth_config SET api_tokens_allow_all_users = 1")
        .execute(&pool)
        .await
        .unwrap();
    let owner_token = pat(&pool, &owner, ALL).await;
    let other_token = pat(&pool, &other, ALL).await;
    let admin_token = pat(&pool, &admin, ALL).await;
    let invite = |token: &str, body: Value| api_post("/api/v1/invites", token, &body);
    let personal = json!({"username": "testuser", "slug": "test-meeting"});

    // Public: the reusable page, nothing stored, and only for its owner.
    let (status, body) = send(&app, invite(&owner_token, personal.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"url": "https://cal.example/u/testuser/test-meeting", "event_type_id": et_id,
               "expires_at": null, "max_uses": null})
    );
    let (status, _) = send(&app, invite(&other_token, personal.clone())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Internal: any user gets a single-use invite carrying the guest.
    set_visibility(&pool, &et_id, "internal").await;
    let (status, body) = send(
        &app,
        invite(
            &other_token,
            json!({"username": "testuser", "slug": "test-meeting", "expires_days": 14,
                   "guest_name": " Jeanne Martin ", "guest_email": "jeanne@example.com"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let url = body["url"].as_str().unwrap();
    assert!(url.starts_with("https://cal.example/u/testuser/test-meeting?invite="));
    assert_eq!(body["max_uses"], 1);
    assert_eq!(body["event_type_id"], et_id.as_str());
    let expires = chrono::DateTime::parse_from_rfc3339(body["expires_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc);
    assert!(
        (expires - Utc::now() - Duration::days(14))
            .num_minutes()
            .abs()
            < 5
    );
    let stored: (String, String, String, i32) = sqlx::query_as(
        "SELECT guest_name, guest_email, created_by_user_id, max_uses FROM booking_invites WHERE token = ?",
    )
    .bind(token_of(url))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        stored,
        (
            "Jeanne Martin".into(),
            "jeanne@example.com".into(),
            other.clone(),
            1
        )
    );
    // The booking form opens with the guest filled in.
    let page = body_string(
        app.clone()
            .oneshot(get(&booking_form_uri(Some(token_of(url)))))
            .await
            .unwrap(),
    )
    .await;
    assert!(page.contains("Jeanne Martin") && page.contains("jeanne@example.com"));

    // Never expiring, and no guest: a plain quick link.
    let (status, body) = send(
        &app,
        invite(
            &other_token,
            json!({"username": "testuser", "slug": "test-meeting", "expires_days": 0}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["expires_at"], Value::Null);

    // Private: the owner only; an instance admin gets no bypass.
    set_visibility(&pool, &et_id, "private").await;
    for token in [&other_token, &admin_token] {
        let (status, body) = send(&app, invite(token, personal.clone())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error_code(&body), "not_found");
    }
    let (status, _) = send(&app, invite(&owner_token, personal.clone())).await;
    assert_eq!(status, StatusCode::CREATED);

    // A disabled internal type is not offered to others.
    set_visibility(&pool, &et_id, "internal").await;
    sqlx::query("UPDATE event_types SET enabled = 0 WHERE id = ?")
        .bind(&et_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = send(&app, invite(&other_token, personal.clone())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_invites_find_team_event_types_by_the_team_slug() {
    let _lock = crate::test_support::ENV_LOCK.lock().await;
    let _env = super::oauth::environment();
    let (app, pool, _, _) = setup_test_app().await;
    let owner = user_id(&pool, "testuser").await;
    let member = insert_role_user(&pool, "member@test.com", "user").await;
    sqlx::query("UPDATE auth_config SET api_tokens_allow_all_users = 1")
        .execute(&pool)
        .await
        .unwrap();
    let (_, team_et) = insert_team_with_et(
        &pool,
        "crew",
        "crew-call",
        &[(&owner, "admin"), (&member, "member")],
    )
    .await;
    set_visibility(&pool, &team_et, "private").await;
    let body = json!({"team": "crew", "slug": "crew-call"});
    let (status, answer) = send(
        &app,
        api_post("/api/v1/invites", &pat(&pool, &owner, ALL).await, &body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    assert!(answer["url"]
        .as_str()
        .unwrap()
        .starts_with("https://cal.example/team/crew/crew-call?invite="));
    // A plain member does not manage a private team type.
    let (status, _) = send(
        &app,
        api_post("/api/v1/invites", &pat(&pool, &member, ALL).await, &body),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The team's slug does not name a personal type, nor the reverse.
    let token = pat(&pool, &owner, ALL).await;
    for body in [
        json!({"username": "testuser", "slug": "crew-call"}),
        json!({"team": "crew", "slug": "test-meeting"}),
    ] {
        let (status, _) = send(&app, api_post("/api/v1/invites", &token, &body)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }
}

#[tokio::test]
async fn api_invites_validate_their_input() {
    let _lock = crate::test_support::ENV_LOCK.lock().await;
    let _env = super::oauth::environment();
    let (app, pool, _, _) = setup_test_app().await;
    let owner = user_id(&pool, "testuser").await;
    let token = pat(&pool, &owner, ALL).await;
    let refused = |body: Value| api_post("/api/v1/invites", &token, &body);
    for (body, code) in [
        (json!({"slug": "test-meeting"}), "invalid_request"),
        (
            json!({"username": "testuser", "team": "crew", "slug": "test-meeting"}),
            "invalid_request",
        ),
        (
            json!({"username": "testuser", "slug": "test-meeting", "expires_days": 5}),
            "invalid_expiry",
        ),
        (
            json!({"username": "testuser", "slug": "test-meeting", "guest_email": "nope"}),
            "invalid_request",
        ),
        (
            json!({"username": "testuser", "slug": "test-meeting", "guest_name": "x".repeat(256)}),
            "invalid_request",
        ),
        (
            json!({"username": "testuser", "slug": "test-meeting", "extra": 1}),
            "invalid_json",
        ),
        (
            json!({"username": "testuser", "slug": "no-such-meeting"}),
            "not_found",
        ),
    ] {
        let (status, answer) = send(&app, refused(body.clone())).await;
        assert_ne!(status, StatusCode::OK, "{body}");
        assert_eq!(error_code(&answer), code, "{body}: {answer}");
    }
    // The write scope is required.
    let read_only = pat(&pool, &owner, "event-types:read bookings:read").await;
    let (status, answer) = send(
        &app,
        api_post(
            "/api/v1/invites",
            &read_only,
            &json!({"username": "testuser", "slug": "test-meeting"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&answer), "insufficient_scope");
}
