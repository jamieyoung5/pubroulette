//! Integration tests for the rooms module, against a real Redis.
//!
//! `#[ignore]`d so plain `cargo test` stays hermetic. Run with:
//!
//! ```sh
//! make redis-up
//! cargo test --test rooms_integration -- --ignored
//! ```
//!
//! Room codes are random and TTL'd, so a missed cleanup only leaves a key
//! that expires in a day.

use pubroulette::decision::{self, DecisionMode, Round};
use pubroulette::places::Mode;
use pubroulette::rooms::{self, Member, Room, RoomError, RoomSettings, RoomStatus};
use redis::AsyncCommands;
use std::sync::Once;

static SETUP: Once = Once::new();

fn setup() {
    SETUP.call_once(|| {
        let _ = dotenvy::dotenv();
    });
}

/// A fresh connection per test. `rooms::connect()` caches one, and its I/O
/// task dies with the runtime that opened it, so every later test would get
/// a broken pipe.
async fn test_connect() -> redis::aio::MultiplexedConnection {
    let url = std::env::var("REDIS_URL").expect("REDIS_URL must be set for integration tests");
    let client = redis::Client::open(url).expect("redis client");
    client
        .get_multiplexed_async_connection()
        .await
        .expect("connect to Redis")
}

fn settings() -> RoomSettings {
    RoomSettings {
        latitude: 55.94,
        longitude: -3.18,
        radius: 500,
        bearing: None,
        num_pubs: 3,
        mode: Mode::Pubs,
        beer_garden: false,
        house_rules_deck: None,
        custom_rules: None,
        decision_mode: DecisionMode::Dictator,
        candidates_per_round: None,
        round_seconds: None,
        custom_pool: None,
    }
}

async fn cleanup(code: &str) {
    let mut conn = test_connect().await;
    let _: redis::RedisResult<()> = conn.del(format!("room:{code}")).await;
}

#[tokio::test]
#[ignore]
async fn create_then_get_round_trip() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "device-host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    assert_eq!(room.status, RoomStatus::Lobby);
    assert_eq!(room.members.len(), 1);
    assert_eq!(room.members[0].name, "Host");
    assert_eq!(room.host_device_id, "device-host");
    assert_eq!(room.version, 1);

    let fetched = rooms::get_room(&mut conn, &room.code).await.expect("get");
    assert_eq!(fetched.code, room.code);
    assert_eq!(fetched.members.len(), 1);

    cleanup(&room.code).await;
}

#[tokio::test]
#[ignore]
async fn join_adds_a_member_and_bumps_version() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    let after = rooms::join_room(
        &mut conn,
        &room.code,
        "guest".to_string(),
        "Guest".to_string(),
    )
    .await
    .expect("join");

    assert_eq!(after.members.len(), 2);
    assert!(after.version > room.version);
    let guest = after
        .members
        .iter()
        .find(|m| m.device_id == "guest")
        .unwrap();
    assert_eq!(guest.name, "Guest");

    cleanup(&room.code).await;
}

#[tokio::test]
#[ignore]
async fn join_twice_only_updates_name() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    let _ = rooms::join_room(
        &mut conn,
        &room.code,
        "guest".to_string(),
        "Guest".to_string(),
    )
    .await
    .expect("join 1");
    let after = rooms::join_room(
        &mut conn,
        &room.code,
        "guest".to_string(),
        "Guest Updated".to_string(),
    )
    .await
    .expect("join 2");

    assert_eq!(after.members.len(), 2, "no duplicate member entry");
    let guest = after
        .members
        .iter()
        .find(|m| m.device_id == "guest")
        .unwrap();
    assert_eq!(guest.name, "Guest Updated");

    cleanup(&room.code).await;
}

#[tokio::test]
#[ignore]
async fn get_room_with_unknown_code_returns_not_found() {
    setup();
    let mut conn = test_connect().await;

    let err = rooms::get_room(&mut conn, "ZZZZZZ")
        .await
        .expect_err("not found");
    assert!(matches!(err, RoomError::NotFound));
}

#[tokio::test]
#[ignore]
async fn get_room_with_invalid_code_rejected() {
    setup();
    let mut conn = test_connect().await;

    let err = rooms::get_room(&mut conn, "lower!")
        .await
        .expect_err("invalid");
    assert!(matches!(err, RoomError::InvalidCode));
}

#[tokio::test]
#[ignore]
async fn advance_requires_host() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    // Plant a started room so advance can be tested without Geoapify.
    let mut started = room.clone();
    started.status = RoomStatus::Active;
    started.pubs = Some(vec![
        fake_pub("Pub A"),
        fake_pub("Pub B"),
        fake_pub("Pub C"),
    ]);
    started.current_index = 0;
    write_room_directly(&mut conn, &started).await;

    let err = rooms::advance_room(&mut conn, &room.code, "not-the-host")
        .await
        .expect_err("non-host should be denied");
    assert!(matches!(err, RoomError::NotHost));

    cleanup(&room.code).await;
}

#[tokio::test]
#[ignore]
async fn advance_walks_through_pubs_and_finishes() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    let mut started = room.clone();
    started.status = RoomStatus::Active;
    started.pubs = Some(vec![fake_pub("A"), fake_pub("B"), fake_pub("C")]);
    started.current_index = 0;
    write_room_directly(&mut conn, &started).await;

    // Finished happens on the advance that would step past the last pub, so
    // 3 pubs give three active advances and a fourth that finishes.
    let r1 = rooms::advance_room(&mut conn, &room.code, "host")
        .await
        .unwrap();
    assert_eq!(r1.current_index, 1);
    assert_eq!(r1.status, RoomStatus::Active);

    let r2 = rooms::advance_room(&mut conn, &room.code, "host")
        .await
        .unwrap();
    assert_eq!(r2.current_index, 2);
    assert_eq!(r2.status, RoomStatus::Active);

    let r3 = rooms::advance_room(&mut conn, &room.code, "host")
        .await
        .unwrap();
    assert_eq!(r3.current_index, 2, "index clamps at last pub");
    assert_eq!(r3.status, RoomStatus::Finished);

    let err = rooms::advance_room(&mut conn, &room.code, "host")
        .await
        .expect_err("can't advance past finished");
    assert!(matches!(err, RoomError::NotStarted));

    cleanup(&room.code).await;
}

#[tokio::test]
#[ignore]
async fn join_after_start_rejects_new_members_but_lets_existing_resync() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    rooms::join_room(&mut conn, &room.code, "guest".to_string(), "G".to_string())
        .await
        .expect("join");

    let mut started = rooms::get_room(&mut conn, &room.code).await.unwrap();
    started.status = RoomStatus::Active;
    started.pubs = Some(vec![fake_pub("A")]);
    write_room_directly(&mut conn, &started).await;

    let resync = rooms::join_room(
        &mut conn,
        &room.code,
        "guest".to_string(),
        "G renamed".to_string(),
    )
    .await
    .expect("existing member resync allowed");
    assert_eq!(resync.members.len(), 2);
    assert_eq!(
        resync
            .members
            .iter()
            .find(|m| m.device_id == "guest")
            .unwrap()
            .name,
        "G renamed"
    );

    let err = rooms::join_room(
        &mut conn,
        &room.code,
        "newcomer".to_string(),
        "N".to_string(),
    )
    .await
    .expect_err("new joiner blocked after start");
    assert!(matches!(err, RoomError::AlreadyStarted));

    cleanup(&room.code).await;
}

#[tokio::test]
#[ignore]
async fn leave_promotes_new_host_or_deletes_room() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    rooms::join_room(&mut conn, &room.code, "guest".to_string(), "G".to_string())
        .await
        .unwrap();

    let after = rooms::leave_room(&mut conn, &room.code, "host")
        .await
        .expect("leave host")
        .expect("room still exists");
    assert_eq!(after.host_device_id, "guest");
    assert_eq!(after.members.len(), 1);

    let after = rooms::leave_room(&mut conn, &room.code, "guest")
        .await
        .expect("leave last");
    assert!(after.is_none(), "empty room should be removed");

    let err = rooms::get_room(&mut conn, &room.code)
        .await
        .expect_err("gone");
    assert!(matches!(err, RoomError::NotFound));
}

#[tokio::test]
#[ignore]
async fn create_persists_with_ttl() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    let key = format!("room:{}", room.code);
    let ttl: i64 = redis::cmd("TTL")
        .arg(&key)
        .query_async(&mut conn)
        .await
        .expect("TTL");
    assert!(ttl > 0, "key must have a TTL");
    assert!(ttl <= 24 * 60 * 60, "TTL must be <= 24h");

    cleanup(&room.code).await;
}

/// Write a room straight to Redis, for tests that need a specific starting
/// state without calling Geoapify.
async fn write_room_directly(conn: &mut redis::aio::MultiplexedConnection, room: &Room) {
    let key = format!("room:{}", room.code);
    let json = serde_json::to_string(room).unwrap();
    let _: () = conn
        .set_ex(&key, json, 24 * 60 * 60)
        .await
        .expect("write room");
}

/// A pub whose `place_id` is exactly `id`, for the vote tests, which approve
/// candidates by place_id.
fn fake_pub_with_id(id: &str) -> pubroulette::roulette::PubResponse {
    pubroulette::roulette::PubResponse {
        place_id: Some(id.to_string()),
        ..fake_pub(id)
    }
}

fn fake_pub(name: &str) -> pubroulette::roulette::PubResponse {
    pubroulette::roulette::PubResponse {
        name: name.to_string(),
        latitude: 0.0,
        longitude: 0.0,
        place_id: Some(format!("p_{name}")),
        address_line1: None,
        address_line2: None,
        phone: None,
        website: None,
        opening_hours: None,
        wikidata_id: None,
        distance_meters: None,
    }
}

// These check the scripts' atomicity: parallel tasks against one room, then
// an assertion that the final state is consistent. A read-modify-write in
// Rust would lose writes here.

use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Every concurrent join must land in the member list.
#[tokio::test]
#[ignore]
async fn concurrent_joins_all_land() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    const N: usize = 15; // host + 15 = 16 members, well under the cap of 20
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..N {
        let code = room.code.clone();
        tasks.spawn(async move {
            let mut c = test_connect().await;
            rooms::join_room(&mut c, &code, format!("guest-{i}"), format!("Guest {i}")).await
        });
    }
    while let Some(res) = tasks.join_next().await {
        res.expect("task panic").expect("join failed");
    }

    let final_room = rooms::get_room(&mut conn, &room.code).await.unwrap();
    assert_eq!(
        final_room.members.len(),
        N + 1,
        "expected host + {N} guests; got {} members",
        final_room.members.len()
    );
    for i in 0..N {
        let id = format!("guest-{i}");
        assert!(
            final_room.members.iter().any(|m| m.device_id == id),
            "guest-{i} missing from final member list"
        );
    }
    assert_eq!(final_room.version, 1 + N as u64);

    cleanup(&room.code).await;
}

/// Concurrent joins past the cap must reject the surplus, not exceed it.
#[tokio::test]
#[ignore]
async fn concurrent_joins_respect_max_members() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    // 26 candidates for a 20-cap room: 19 succeed, 6 hit RoomFull.
    const N: usize = 25;
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..N {
        let code = room.code.clone();
        tasks.spawn(async move {
            let mut c = test_connect().await;
            rooms::join_room(&mut c, &code, format!("g-{i}"), format!("G{i}")).await
        });
    }

    let mut ok = 0;
    let mut full = 0;
    while let Some(res) = tasks.join_next().await {
        match res.expect("task panic") {
            Ok(_) => ok += 1,
            Err(RoomError::RoomFull) => full += 1,
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }
    assert_eq!(ok, 19, "exactly 19 joins should succeed");
    assert_eq!(full, N - 19, "the rest must hit RoomFull");

    let final_room = rooms::get_room(&mut conn, &room.code).await.unwrap();
    assert_eq!(final_room.members.len(), 20, "host + 19 guests = 20 (cap)");

    cleanup(&room.code).await;
}

/// A mass concurrent leave must either delete the room or leave a host who is
/// genuinely still present.
#[tokio::test]
#[ignore]
async fn concurrent_leaves_no_phantom_host() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");
    for i in 0..5 {
        rooms::join_room(&mut conn, &room.code, format!("g-{i}"), format!("G{i}"))
            .await
            .expect("join");
    }

    let all_members: Vec<String> = (0..5)
        .map(|i| format!("g-{i}"))
        .chain(std::iter::once("host".to_string()))
        .collect();

    let mut tasks = tokio::task::JoinSet::new();
    for device in &all_members {
        let code = room.code.clone();
        let device = device.clone();
        tasks.spawn(async move {
            let mut c = test_connect().await;
            rooms::leave_room(&mut c, &code, &device).await
        });
    }
    while let Some(res) = tasks.join_next().await {
        res.expect("task panic").expect("leave failed");
    }

    match rooms::get_room(&mut conn, &room.code).await {
        Err(RoomError::NotFound) => { /* clean delete, ideal */ }
        Ok(final_room) => {
            assert!(
                !final_room.members.is_empty(),
                "non-deleted room must have at least 1 member"
            );
            assert!(
                final_room
                    .members
                    .iter()
                    .any(|m| m.device_id == final_room.host_device_id),
                "host_device_id ({}) must point at a real member; current members: {:?}",
                final_room.host_device_id,
                final_room
                    .members
                    .iter()
                    .map(|m| &m.device_id)
                    .collect::<Vec<_>>()
            );
        }
        Err(e) => panic!("unexpected error: {e:?}"),
    }

    cleanup(&room.code).await;
}

/// Two parallel advances must each move the index on by one.
#[tokio::test]
#[ignore]
async fn concurrent_advances_each_step_forward() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    // 5 pubs, so neither advance hits the Finished transition.
    let mut started = room.clone();
    started.status = RoomStatus::Active;
    started.pubs = Some(vec![
        fake_pub("A"),
        fake_pub("B"),
        fake_pub("C"),
        fake_pub("D"),
        fake_pub("E"),
    ]);
    started.current_index = 0;
    write_room_directly(&mut conn, &started).await;

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..2 {
        let code = room.code.clone();
        tasks.spawn(async move {
            let mut c = test_connect().await;
            rooms::advance_room(&mut c, &code, "host").await
        });
    }
    while let Some(res) = tasks.join_next().await {
        res.expect("task panic").expect("advance failed");
    }

    let final_room = rooms::get_room(&mut conn, &room.code).await.unwrap();
    assert_eq!(
        final_room.current_index, 2,
        "two atomic advances must each step +1 (final index = 2)"
    );
    assert_eq!(final_room.status, RoomStatus::Active);

    cleanup(&room.code).await;
}

/// Host starts the crawl while a member joins. The join either lands before
/// the start commits or is rejected as already-started; either way the pubs
/// list and member set must be coherent.
#[tokio::test]
#[ignore]
async fn start_racing_with_join_preserves_state() {
    setup();
    let mut conn = test_connect().await;

    let mock = MockServer::start().await;
    let body = serde_json::json!({
        "type": "FeatureCollection",
        "features": (0..5).map(|i| serde_json::json!({
            "type": "Feature",
            "properties": {
                "name": format!("Pub {i}"),
                "place_id": format!("id_{i}"),
                "osm_id": 1000 + i,
                "categories": ["catering.pub"],
            },
            "geometry": { "type": "Point", "coordinates": [-3.18, 55.94] },
        })).collect::<Vec<_>>()
    });
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&mock)
        .await;
    let geo = pubroulette::places::GeoapifyConfig::new(mock.uri(), "test-key");

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    let code = room.code.clone();
    let code_for_start = code.clone();
    let geo_for_start = geo.clone();
    let start_task = tokio::spawn(async move {
        let mut c = test_connect().await;
        rooms::start_room(&mut c, &geo_for_start, &code_for_start, "host").await
    });
    let join_task = tokio::spawn(async move {
        let mut c = test_connect().await;
        rooms::join_room(&mut c, &code, "guest".to_string(), "Guest".to_string()).await
    });

    let start_result = start_task.await.expect("start task panic");
    let join_result = join_task.await.expect("join task panic");

    let final_room = rooms::get_room(&mut conn, &room.code).await.unwrap();

    // CAS retries absorb the concurrent join, so start always succeeds.
    start_result.expect("start should succeed");
    assert_eq!(final_room.status, RoomStatus::Active);
    assert_eq!(
        final_room.pubs.as_ref().map(|p| p.len()),
        Some(3),
        "pubs list must be intact (num_pubs=3 from settings())"
    );

    // Either ordering is legal.
    match join_result {
        Ok(_) => assert!(
            final_room.members.iter().any(|m| m.device_id == "guest"),
            "join reported success but guest is missing from members"
        ),
        Err(RoomError::AlreadyStarted) => assert!(
            final_room.members.iter().all(|m| m.device_id != "guest"),
            "join was rejected as AlreadyStarted but guest is present anyway"
        ),
        Err(e) => panic!("unexpected join error: {e:?}"),
    }

    cleanup(&room.code).await;
}

/// Joins and leaves in parallel must leave no duplicates, a valid host, and
/// the member count the successful operations imply.
#[tokio::test]
#[ignore]
async fn mixed_join_and_leave_load() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    for i in 0..5 {
        rooms::join_room(&mut conn, &room.code, format!("seed-{i}"), format!("S{i}"))
            .await
            .expect("seed join");
    }

    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..5 {
        let code = room.code.clone();
        tasks.spawn(async move {
            let mut c = test_connect().await;
            rooms::join_room(&mut c, &code, format!("new-{i}"), format!("N{i}"))
                .await
                .map(|_| ())
        });
    }
    for i in 0..3 {
        let code = room.code.clone();
        tasks.spawn(async move {
            let mut c = test_connect().await;
            rooms::leave_room(&mut c, &code, &format!("seed-{i}"))
                .await
                .map(|_| ())
        });
    }
    while let Some(res) = tasks.join_next().await {
        res.expect("task panic").expect("op failed");
    }

    let final_room = rooms::get_room(&mut conn, &room.code).await.unwrap();
    assert_eq!(final_room.members.len(), 8);

    let mut seen = std::collections::HashSet::new();
    for m in &final_room.members {
        assert!(
            seen.insert(&m.device_id),
            "duplicate member: {}",
            m.device_id
        );
    }

    assert!(
        final_room
            .members
            .iter()
            .any(|m| m.device_id == final_room.host_device_id),
        "host_device_id points at a non-member"
    );

    cleanup(&room.code).await;
}

// These plant a vote-mode room with a known pool, then drive the public API.
// A deterministic pool is what lets a vote pattern have an expected winner.

use chrono::{Duration, Utc};

/// A vote-mode room on round 0, with a 7-pub pool and pids 0/1/2 drawn. Pass
/// a negative `expires_in` for an already-expired round.
fn make_vote_room_in_redis_state(
    code: &str,
    members: &[(&str, &str)],
    expires_in: Duration,
) -> Room {
    let now = Utc::now();
    let mut s = settings();
    s.decision_mode = DecisionMode::Vote;
    s.num_pubs = 3;
    s.candidates_per_round = Some(3);
    s.round_seconds = Some(60);

    let pool: Vec<_> = (0..7)
        .map(|i| fake_pub_with_id(&format!("pid{i}")))
        .collect();
    let candidates = pool[..3].to_vec();
    let host_id = members[0].0.to_string();

    Room {
        code: code.to_string(),
        host_device_id: host_id,
        created_at: now,
        updated_at: now,
        status: RoomStatus::Active,
        settings: s,
        members: members
            .iter()
            .map(|(id, name)| Member {
                device_id: id.to_string(),
                name: name.to_string(),
                joined_at: now,
            })
            .collect(),
        // `None`, not an empty vec: Lua would re-encode `[]` as `{}`, which
        // Rust can't read back. Production stores `None` for the same reason.
        pubs: None,
        current_index: 0,
        house_rules: None,
        pool: Some(pool),
        current_round: Some(Round {
            index: 0,
            candidates,
            opened_at: now,
            expires_at: now + expires_in,
            votes: None,
            resolved: None,
        }),
        skips_used: 0,
        finish_reason: None,
        version: 5,
    }
}

#[tokio::test]
#[ignore]
async fn vote_clear_winner_no_tiebreak() {
    setup();
    let mut conn = test_connect().await;
    let code = "VTEAAA";
    let room = make_vote_room_in_redis_state(
        code,
        &[("host", "Host"), ("dev2", "Sam"), ("dev3", "Jamie")],
        Duration::seconds(60),
    );
    write_room_directly(&mut conn, &room).await;

    let _ = rooms::vote_room(&mut conn, code, "host", &["pid0".to_string()])
        .await
        .unwrap();
    let _ = rooms::vote_room(&mut conn, code, "dev2", &["pid0".to_string()])
        .await
        .unwrap();
    let _ = rooms::vote_room(&mut conn, code, "dev3", &["pid1".to_string()])
        .await
        .unwrap();

    let after = rooms::advance_room(&mut conn, code, "host").await.unwrap();
    assert_eq!(after.pubs.as_ref().unwrap().len(), 1);
    assert_eq!(
        after.pubs.as_ref().unwrap()[0].place_id.as_deref(),
        Some("pid0")
    );

    // Advance only resolves. The round stays put, carrying its result, until
    // the host opens the next one.
    let resolved = after.current_round.as_ref().expect("round kept");
    assert_eq!(resolved.index, 0);
    assert_eq!(
        resolved
            .resolved
            .as_ref()
            .map(|r| r.winner.place_id.as_deref()),
        Some(Some("pid0"))
    );

    let opened = rooms::open_next_round_for_code(&mut conn, code, "host")
        .await
        .unwrap();
    let next = opened.current_round.as_ref().expect("next round opened");
    assert_eq!(next.index, 1);
    assert!(next.resolved.is_none());

    cleanup(code).await;
}

#[tokio::test]
#[ignore]
async fn vote_approval_voting_allows_multiple_picks() {
    setup();
    let mut conn = test_connect().await;
    let code = "VTEAAB";
    let room = make_vote_room_in_redis_state(
        code,
        &[("host", "Host"), ("dev2", "Sam")],
        Duration::seconds(60),
    );
    write_room_directly(&mut conn, &room).await;

    // pid0 and pid1 tie at 2, pid2 has 1, so the winner is one of the two.
    let r = rooms::vote_room(
        &mut conn,
        code,
        "host",
        &["pid0".to_string(), "pid1".to_string()],
    )
    .await
    .unwrap();
    let round = r.current_round.as_ref().unwrap();
    assert_eq!(
        round.votes.as_ref().unwrap().get("host").unwrap().len(),
        2,
        "approval voting must persist all ticked candidates"
    );

    let _ = rooms::vote_room(
        &mut conn,
        code,
        "dev2",
        &["pid0".to_string(), "pid1".to_string(), "pid2".to_string()],
    )
    .await
    .unwrap();

    let after = rooms::advance_room(&mut conn, code, "host").await.unwrap();
    let winner_pid = after.pubs.as_ref().unwrap()[0].place_id.as_deref().unwrap();
    assert!(
        winner_pid == "pid0" || winner_pid == "pid1",
        "expected tied winner from {{pid0, pid1}}, got {winner_pid}"
    );

    cleanup(code).await;
}

#[tokio::test]
#[ignore]
async fn vote_empty_approvals_clears_existing_vote() {
    setup();
    let mut conn = test_connect().await;
    let code = "VTEAAC";
    let room = make_vote_room_in_redis_state(code, &[("host", "Host")], Duration::seconds(60));
    write_room_directly(&mut conn, &room).await;

    let r1 = rooms::vote_room(&mut conn, code, "host", &["pid0".to_string()])
        .await
        .unwrap();
    assert_eq!(
        r1.current_round
            .as_ref()
            .unwrap()
            .votes
            .as_ref()
            .unwrap()
            .len(),
        1
    );

    // An emptied votes map must drop back to None for cjson's sake.
    let r2 = rooms::vote_room(&mut conn, code, "host", &[])
        .await
        .unwrap();
    assert!(
        r2.current_round.as_ref().unwrap().votes.is_none(),
        "empty votes map should serialise back to None"
    );

    cleanup(code).await;
}

#[tokio::test]
#[ignore]
async fn vote_rejects_non_members() {
    setup();
    let mut conn = test_connect().await;
    let code = "VTEAAD";
    let room = make_vote_room_in_redis_state(code, &[("host", "Host")], Duration::seconds(60));
    write_room_directly(&mut conn, &room).await;

    let err = rooms::vote_room(&mut conn, code, "outsider", &["pid0".to_string()])
        .await
        .expect_err("non-members must not be able to vote");
    assert!(matches!(err, RoomError::NotMember), "got {err:?}");

    cleanup(code).await;
}

#[tokio::test]
#[ignore]
async fn vote_rejects_invalid_candidate() {
    setup();
    let mut conn = test_connect().await;
    let code = "VTEAAE";
    let room = make_vote_room_in_redis_state(code, &[("host", "Host")], Duration::seconds(60));
    write_room_directly(&mut conn, &room).await;

    let err = rooms::vote_room(&mut conn, code, "host", &["not-a-candidate".to_string()])
        .await
        .expect_err("bogus place_id must be rejected");
    assert!(matches!(err, RoomError::InvalidCandidate), "got {err:?}");

    cleanup(code).await;
}

#[tokio::test]
#[ignore]
async fn vote_rejects_expired_round() {
    setup();
    let mut conn = test_connect().await;
    let code = "VTEAAF";
    let room = make_vote_room_in_redis_state(code, &[("host", "Host")], Duration::seconds(-10));
    write_room_directly(&mut conn, &room).await;

    let err = rooms::vote_room(&mut conn, code, "host", &["pid0".to_string()])
        .await
        .expect_err("expired rounds must not accept votes");
    assert!(matches!(err, RoomError::RoundExpired), "got {err:?}");

    cleanup(code).await;
}

#[tokio::test]
#[ignore]
async fn advance_finishes_after_num_pubs_resolutions() {
    setup();
    let mut conn = test_connect().await;
    let code = "VTEAAG";
    let room = make_vote_room_in_redis_state(code, &[("host", "Host")], Duration::seconds(60));
    write_room_directly(&mut conn, &room).await;

    for round_n in 0..3 {
        let r = rooms::get_room(&mut conn, code).await.unwrap();
        let pid = r.current_round.as_ref().unwrap().candidates[0]
            .place_id
            .clone()
            .unwrap();
        rooms::vote_room(&mut conn, code, "host", &[pid])
            .await
            .unwrap();
        let after = rooms::advance_room(&mut conn, code, "host").await.unwrap();

        if round_n < 2 {
            assert_eq!(after.status, RoomStatus::Active);
            rooms::open_next_round_for_code(&mut conn, code, "host")
                .await
                .unwrap();
        } else {
            assert_eq!(after.status, RoomStatus::Finished);
            assert_eq!(after.pubs.as_ref().unwrap().len(), 3);
        }
    }

    cleanup(code).await;
}

#[tokio::test]
#[ignore]
async fn skip_round_reshuffles_and_consumes_budget() {
    setup();
    let mut conn = test_connect().await;
    let code = "VTEAAH";
    let room = make_vote_room_in_redis_state(code, &[("host", "Host")], Duration::seconds(60));
    let original_candidates: Vec<String> = room
        .current_round
        .as_ref()
        .unwrap()
        .candidates
        .iter()
        .map(|c| c.place_id.clone().unwrap())
        .collect();
    write_room_directly(&mut conn, &room).await;

    let r1 = rooms::skip_round(&mut conn, code, "host").await.unwrap();
    assert_eq!(r1.skips_used, 1);
    let new_candidates: Vec<String> = r1
        .current_round
        .as_ref()
        .unwrap()
        .candidates
        .iter()
        .map(|c| c.place_id.clone().unwrap())
        .collect();
    assert_ne!(new_candidates, original_candidates);
    for pid in &new_candidates {
        assert!(
            !original_candidates.contains(pid),
            "skipped pid {pid} reappeared after skip"
        );
    }

    let r2 = rooms::skip_round(&mut conn, code, "host").await.unwrap();
    assert_eq!(r2.skips_used, 2);

    let err = rooms::skip_round(&mut conn, code, "host")
        .await
        .expect_err("third skip should be refused");
    assert!(matches!(err, RoomError::SkipBudgetExhausted), "got {err:?}");

    cleanup(code).await;
}

#[tokio::test]
#[ignore]
async fn skip_round_requires_host() {
    setup();
    let mut conn = test_connect().await;
    let code = "VTEAAJ";
    let room = make_vote_room_in_redis_state(
        code,
        &[("host", "Host"), ("dev2", "Sam")],
        Duration::seconds(60),
    );
    write_room_directly(&mut conn, &room).await;

    let err = rooms::skip_round(&mut conn, code, "dev2")
        .await
        .expect_err("non-host must not be able to skip");
    assert!(matches!(err, RoomError::NotHost), "got {err:?}");

    cleanup(code).await;
}

#[tokio::test]
#[ignore]
async fn resolve_if_expired_auto_advances_on_poll() {
    setup();
    let mut conn = test_connect().await;
    let code = "VTEAAK";
    let room = make_vote_room_in_redis_state(
        code,
        &[("host", "Host"), ("dev2", "Sam")],
        Duration::seconds(-1),
    );
    let mut planted = room.clone();
    let votes: std::collections::BTreeMap<String, Vec<String>> =
        std::iter::once(("dev2".to_string(), vec!["pid2".to_string()])).collect();
    planted.current_round.as_mut().unwrap().votes = Some(votes);
    write_room_directly(&mut conn, &planted).await;

    // A poll after expiry resolves the round without the host advancing.
    let observed = rooms::resolve_room_if_expired(&mut conn, code)
        .await
        .unwrap();
    assert_eq!(
        observed.pubs.as_ref().map(|p| p.len()),
        Some(1),
        "lazy resolve should commit the winner"
    );
    assert_eq!(
        observed.pubs.as_ref().unwrap()[0].place_id.as_deref(),
        Some("pid2")
    );
    // Only the round closes. The group sees the result before voting again,
    // so the next round waits for an explicit call.
    let round = observed.current_round.as_ref().expect("round kept");
    assert_eq!(round.index, 0);
    assert!(round.resolved.is_some());

    cleanup(code).await;
}

#[tokio::test]
#[ignore]
async fn resolve_if_expired_is_noop_when_not_expired() {
    setup();
    let mut conn = test_connect().await;
    let code = "VTEAAL";
    let room = make_vote_room_in_redis_state(code, &[("host", "Host")], Duration::seconds(60));
    write_room_directly(&mut conn, &room).await;
    let before = rooms::get_room(&mut conn, code).await.unwrap();

    let after = rooms::resolve_room_if_expired(&mut conn, code)
        .await
        .unwrap();
    assert_eq!(after.version, before.version);
    assert!(after.pubs.as_ref().is_none_or(|p| p.is_empty()));

    cleanup(code).await;
}

#[tokio::test]
#[ignore]
async fn advance_rejects_non_host_in_vote_mode() {
    setup();
    let mut conn = test_connect().await;
    let code = "VTEAAM";
    let room = make_vote_room_in_redis_state(
        code,
        &[("host", "Host"), ("dev2", "Sam")],
        Duration::seconds(60),
    );
    write_room_directly(&mut conn, &room).await;

    let err = rooms::advance_room(&mut conn, code, "dev2")
        .await
        .expect_err("only host may advance");
    assert!(matches!(err, RoomError::NotHost), "got {err:?}");

    cleanup(code).await;
}

#[tokio::test]
#[ignore]
async fn vote_then_advance_resets_votes_for_next_round() {
    setup();
    let mut conn = test_connect().await;
    let code = "VTEAAN";
    let room = make_vote_room_in_redis_state(code, &[("host", "Host")], Duration::seconds(60));
    write_room_directly(&mut conn, &room).await;

    let _ = rooms::vote_room(&mut conn, code, "host", &["pid0".to_string()])
        .await
        .unwrap();
    rooms::advance_room(&mut conn, code, "host").await.unwrap();
    let opened = rooms::open_next_round_for_code(&mut conn, code, "host")
        .await
        .unwrap();

    let next = opened.current_round.as_ref().unwrap();
    assert_eq!(next.index, 1);
    assert!(
        next.votes.is_none(),
        "votes from the previous round must not bleed into the next"
    );
    assert!(next.resolved.is_none());

    cleanup(code).await;
}

#[tokio::test]
#[ignore]
async fn vote_persists_max_skips_constant_check() {
    // The public constant must match the skips actually allowed.
    assert_eq!(decision::MAX_SKIPS_PER_CRAWL, 2);
}

// Unlike join, rejoin works on an active room for a device that isn't a
// member, which covers both mid-crawl arrivals and silent token recovery.

#[tokio::test]
#[ignore]
async fn rejoin_lets_new_member_enter_active_room() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    let mut started = room.clone();
    started.status = RoomStatus::Active;
    started.pubs = Some(vec![fake_pub("A"), fake_pub("B"), fake_pub("C")]);
    write_room_directly(&mut conn, &started).await;

    // join would reject a non-member here; rejoin must not.
    let after = rooms::rejoin_room(
        &mut conn,
        &room.code,
        "newcomer".to_string(),
        "Newcomer".to_string(),
    )
    .await
    .expect("rejoin should succeed for active room");

    assert_eq!(after.status, RoomStatus::Active);
    assert_eq!(after.members.len(), 2, "host + newcomer");
    assert!(
        after.members.iter().any(|m| m.device_id == "newcomer"),
        "newcomer must appear in members"
    );

    cleanup(&room.code).await;
}

#[tokio::test]
#[ignore]
async fn rejoin_refreshes_name_for_existing_member_on_active_room() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    rooms::join_room(
        &mut conn,
        &room.code,
        "guest".to_string(),
        "Old Name".to_string(),
    )
    .await
    .expect("initial join");

    let mut started = rooms::get_room(&mut conn, &room.code).await.unwrap();
    started.status = RoomStatus::Active;
    started.pubs = Some(vec![fake_pub("A")]);
    write_room_directly(&mut conn, &started).await;

    let after = rooms::rejoin_room(
        &mut conn,
        &room.code,
        "guest".to_string(),
        "New Name".to_string(),
    )
    .await
    .expect("rejoin existing member");

    assert_eq!(after.members.len(), 2, "no duplicate member created");
    let guest = after
        .members
        .iter()
        .find(|m| m.device_id == "guest")
        .unwrap();
    assert_eq!(guest.name, "New Name", "name must be updated in-place");

    cleanup(&room.code).await;
}

#[tokio::test]
#[ignore]
async fn rejoin_works_for_lobby_room() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    let after = rooms::rejoin_room(
        &mut conn,
        &room.code,
        "guest".to_string(),
        "Guest".to_string(),
    )
    .await
    .expect("rejoin should work for lobby rooms too");

    assert_eq!(after.status, RoomStatus::Lobby);
    assert_eq!(after.members.len(), 2);

    cleanup(&room.code).await;
}

#[tokio::test]
#[ignore]
async fn rejoin_rejects_not_found() {
    setup();
    let mut conn = test_connect().await;

    let err = rooms::rejoin_room(
        &mut conn,
        "ABSENT",
        "anyone".to_string(),
        "Anyone".to_string(),
    )
    .await
    .expect_err("must fail for non-existent room");

    assert!(matches!(err, RoomError::NotFound), "got {err:?}");
}

#[tokio::test]
#[ignore]
async fn rejoin_rejects_room_full() {
    setup();
    let mut conn = test_connect().await;

    let room = rooms::create_room(
        &mut conn,
        "host".to_string(),
        "Host".to_string(),
        settings(),
    )
    .await
    .expect("create");

    // Fill to MAX_MEMBERS through concurrent joins, so the cap is reached
    // atomically.
    const EXTRA: usize = 19;
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..EXTRA {
        let code = room.code.clone();
        tasks.spawn(async move {
            let mut c = test_connect().await;
            rooms::join_room(&mut c, &code, format!("filler-{i}"), format!("F{i}")).await
        });
    }
    while let Some(res) = tasks.join_next().await {
        res.expect("task panic").expect("fill join");
    }

    let full_check = rooms::get_room(&mut conn, &room.code).await.unwrap();
    assert_eq!(
        full_check.members.len(),
        20,
        "room must be at cap before testing rejoin"
    );

    let err = rooms::rejoin_room(
        &mut conn,
        &room.code,
        "latecomer".to_string(),
        "Late".to_string(),
    )
    .await
    .expect_err("rejoin into a full room must be refused");

    assert!(matches!(err, RoomError::RoomFull), "got {err:?}");

    cleanup(&room.code).await;
}
