//! Multiplayer pub crawl rooms.
//!
//! A host creates a room and shares the short code; friends join with the code
//! and the host kicks off a shared crawl. State lives in Redis with a 24h TTL.
//! There's no real-time channel: clients poll `GET /api/rooms/{code}` for state
//! changes. Authority is host-only: only the creator can start or advance the
//! crawl.

use crate::decision::{
    self, DecisionMode, Round, effective_candidates_per_round, effective_round_seconds,
    now_rfc3339_millis,
};
use crate::house_rules::{
    CustomRulesError, HouseRulesDeck, MAX_CUSTOM_RULES, roll_one, roll_rules, validate_custom_rules,
};
use crate::places::{GeoapifyConfig, Mode};
use crate::roulette::{
    PlaceFilters, PubResponse, fetch_pool, get_random_crawl, sample_weighted_pubs,
};
use chrono::{DateTime, Utc};
use rand::Rng;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tokio::sync::OnceCell;
use tracing::{info, warn};
use validator::Validate;
use vercel_runtime::StatusCode as VercelStatusCode;

/// Matches the frontend's own session lifetime.
const ROOM_TTL_SECONDS: u64 = 24 * 60 * 60;

/// No `0/O/I/1`, so a code read aloud can't be mistyped.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 6;

/// Bigger rooms mean more JSON on every poll.
const MAX_MEMBERS: usize = 20;

/// Approvals one vote may carry. A round only ever has
/// [`decision::MAX_CANDIDATES_PER_ROUND`] candidates, so this is pure abuse
/// defence: a huge approvals array would make every later write in the room
/// round-trip it through cjson.
const MAX_APPROVALS_PER_VOTE: usize = 16;

/// Real `place_id` values are 30-60 characters. Keeps `vote.lua` cheap.
const MAX_APPROVAL_STRING_LEN: usize = 128;

/// Size of one `custom_pool` entry. The count is capped by the struct's
/// `length(max = 50)`; this stops one huge entry riding through every cjson
/// decode for the life of the room. Around 8x the largest real entry.
const MAX_CUSTOM_POOL_ENTRY_BYTES: usize = 2048;

/// Code attempts before giving up. The keyspace is ~1B against a 24h TTL, so
/// collisions are already vanishingly rare.
const MAX_CREATE_ATTEMPTS: u32 = 8;

/// CAS retries for `start_room`, in case a join or leave bumps the version
/// between the upstream fetch and the write.
const START_ROOM_CAS_RETRIES: u32 = 3;

/// CAS retries for every other room write. Bounded, so concurrent traffic
/// can't spin forever.
const CAS_RETRIES: u32 = 3;

/// Lifetime of the start lock if nothing releases it. Upstream calls land in
/// seconds, so anything near this means the function itself died. This TTL is
/// the only guard against a room stuck forever.
const START_LOCK_TTL_SECONDS: u64 = 30;

// Each script runs atomically, so nothing can interleave between its GET and
// SET. Sources live in src/lua/ and are embedded at compile time.
//
// Scripts that decode, mutate and re-encode the room (join, rejoin, leave,
// advance, vote) push every float through lua-cjson, which keeps 14
// significant figures against f64's ~16. Harmless for lat/lon, but don't add
// a `PubResponse` field that needs full precision without checking this. The
// CAS scripts take a Rust-serialised blob and avoid the problem.

const LUA_JOIN: &str = include_str!("lua/join.lua");
const LUA_REJOIN: &str = include_str!("lua/rejoin.lua");
const LUA_LEAVE: &str = include_str!("lua/leave.lua");
const LUA_ADVANCE: &str = include_str!("lua/advance.lua");
const LUA_START_CAS: &str = include_str!("lua/start_cas.lua");
const LUA_VOTE: &str = include_str!("lua/vote.lua");
const LUA_CAS_WRITE: &str = include_str!("lua/cas_write.lua");

fn room_key(code: &str) -> String {
    format!("room:{code}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub device_id: String,
    pub name: String,
    pub joined_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct RoomSettings {
    #[validate(range(min = -90.0, max = 90.0))]
    pub latitude: f64,
    #[validate(range(min = -180.0, max = 180.0))]
    pub longitude: f64,
    #[validate(range(min = 1, max = 5000))]
    pub radius: i16,
    #[validate(range(min = 0.0, max = 360.0))]
    pub bearing: Option<f64>,
    #[validate(range(min = 1, max = 5))]
    pub num_pubs: i16,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub beer_garden: bool,
    /// Deck for a house-rules game. Dictator mode rolls every rule at
    /// `start_room`; vote mode rolls one per resolved round. Either way the
    /// result is persisted, so every member sees the same rule per pub.
    #[serde(default)]
    pub house_rules_deck: Option<HouseRulesDeck>,
    /// The host's own rules, required for a `Custom` deck. Normalised at
    /// create time by [`validate_custom_rules`], so this is exactly what gets
    /// dealt. Bounded like `custom_pool`, because the room JSON round-trips
    /// through Lua on every poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_rules: Option<Vec<String>>,
    /// How the next pub gets picked.
    #[serde(default)]
    pub decision_mode: DecisionMode,
    /// Candidates per round. Clamped on read by
    /// [`effective_candidates_per_round`].
    #[validate(range(min = 2, max = 5))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidates_per_round: Option<u8>,
    /// Voting window per round. `Some(0)` means the host closes manually.
    #[validate(range(min = 0, max = 300))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_seconds: Option<u32>,
    /// Pool to use instead of fetching, for "from my favourites" rooms.
    /// Bounded, because an oversized pool would ride through every Lua
    /// decode and every poll response.
    #[validate(length(max = 50))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_pool: Option<Vec<PubResponse>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RoomStatus {
    Lobby,
    Active,
    Finished,
}

/// Why a room finished, so the client can tell a full crawl from one cut
/// short by an empty pool.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// All `num_pubs` rounds resolved.
    Completed,
    /// The pool ran out, so the crawl ended early rather than opening an
    /// empty round.
    PoolExhausted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    pub code: String,
    pub host_device_id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub status: RoomStatus,
    pub settings: RoomSettings,
    pub members: Vec<Member>,
    /// Locked-in pub list once the host starts the crawl.
    #[serde(default)]
    pub pubs: Option<Vec<PubResponse>>,
    /// Where the group is now. Only meaningful while `Active`.
    #[serde(default)]
    pub current_index: usize,
    /// One rule per pub: `house_rules[i]` pairs with `pubs[i]`. `None` with
    /// no deck configured.
    #[serde(default)]
    pub house_rules: Option<Vec<String>>,
    /// Every candidate fetched at `start_room`, drained as rounds resolve.
    /// `None` in dictator mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<Vec<PubResponse>>,
    /// The open or just-resolved round. `None` in dictator mode, when
    /// finished, or briefly between rounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_round: Option<Round>,
    /// Skips used, capped at [`decision::MAX_SKIPS_PER_CRAWL`] per crawl.
    #[serde(default)]
    pub skips_used: u8,
    /// Set once the room is `Finished`. Skipped when serialising, so older
    /// room blobs without it still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
    /// Bumped on every write, so a poll can skip unchanged state.
    pub version: u64,
}

#[derive(Debug)]
pub enum RoomError {
    NotFound,
    NotHost,
    NotMember,
    RoomFull,
    AlreadyStarted,
    NotStarted,
    AlreadyAtEnd,
    NameTooLong,
    NameRequired,
    InvalidCode,
    /// A place_id that isn't in the current candidate set.
    InvalidCandidate,
    /// Too many approvals, or one of them too long. The sizes go to the logs,
    /// not the client.
    ApprovalsTooLong {
        count: usize,
        max_len: usize,
    },
    /// No open round to vote on.
    NoOpenRound,
    /// The current round has to be resolved before the next one opens.
    RoundStillOpen,
    /// The voting window has elapsed.
    RoundExpired,
    /// A `custom_pool` entry has an out-of-range coordinate. The index is for
    /// the logs.
    InvalidCustomPool {
        index: usize,
    },
    /// A `custom_pool` entry is over `MAX_CUSTOM_POOL_ENTRY_BYTES`. The
    /// index and size are for the logs.
    CustomPoolEntryTooLarge {
        index: usize,
        bytes: usize,
    },
    /// The `custom_rules` list was missing, too long, or had an oversized
    /// entry.
    CustomRules(CustomRulesError),
    /// The host is out of skips.
    SkipBudgetExhausted,
    /// Vote-mode call against a dictator-mode room, or the reverse.
    ModeMismatch,
    Validation(validator::ValidationErrors),
    /// Redis failed. The message is for logs only.
    Storage(String),
    /// CAS retries ran out. Redis is healthy, so the answer is to try again.
    Contention(String),
    /// A `start_room` is already running for this room. Needs a longer wait
    /// than `Contention`.
    StartInProgress,
    /// An invariant was violated, such as a vote-mode room with no pool.
    /// A bug, so retrying won't help.
    Invariant(String),
    /// An error from the crawl pipeline, such as no pubs nearby.
    Crawl(crate::roulette::AppError),
}

impl RoomError {
    pub fn to_response(&self) -> (VercelStatusCode, String) {
        match self {
            RoomError::NotFound => (
                VercelStatusCode::NOT_FOUND,
                "Room not found. Check the code or ask the host.".to_string(),
            ),
            RoomError::NotHost => (
                VercelStatusCode::FORBIDDEN,
                "Only the host can do that.".to_string(),
            ),
            RoomError::NotMember => (
                VercelStatusCode::FORBIDDEN,
                "You must be a member of this room to do that.".to_string(),
            ),
            RoomError::RoomFull => (
                VercelStatusCode::CONFLICT,
                format!("Room is full (max {MAX_MEMBERS} members)."),
            ),
            RoomError::AlreadyStarted => (
                VercelStatusCode::CONFLICT,
                "The crawl has already started.".to_string(),
            ),
            RoomError::NotStarted => (
                VercelStatusCode::CONFLICT,
                "The crawl hasn't started yet.".to_string(),
            ),
            RoomError::AlreadyAtEnd => (
                VercelStatusCode::CONFLICT,
                "Already at the last pub.".to_string(),
            ),
            RoomError::InvalidCandidate => (
                VercelStatusCode::BAD_REQUEST,
                "Vote includes a pub that isn't on the candidate list.".to_string(),
            ),
            // Generic, so the response doesn't reveal where the cap sits.
            RoomError::ApprovalsTooLong { .. } => (
                VercelStatusCode::BAD_REQUEST,
                "Invalid request parameters.".to_string(),
            ),
            RoomError::NoOpenRound => (
                VercelStatusCode::CONFLICT,
                "There's no round open to vote on right now.".to_string(),
            ),
            RoomError::RoundStillOpen => (
                VercelStatusCode::CONFLICT,
                "Lock in the current round before opening the next.".to_string(),
            ),
            RoomError::RoundExpired => (
                VercelStatusCode::CONFLICT,
                "The voting window has closed.".to_string(),
            ),
            RoomError::InvalidCustomPool { .. } => (
                VercelStatusCode::BAD_REQUEST,
                "Invalid request parameters.".to_string(),
            ),
            RoomError::CustomPoolEntryTooLarge { .. } => (
                VercelStatusCode::BAD_REQUEST,
                "Invalid request parameters.".to_string(),
            ),
            // The host typed this text, so name the constraint they broke.
            RoomError::CustomRules(CustomRulesError::Empty) => (
                VercelStatusCode::BAD_REQUEST,
                "Add at least one house rule, or pick a different deck.".to_string(),
            ),
            RoomError::CustomRules(_) => (
                VercelStatusCode::BAD_REQUEST,
                format!(
                    "House rules must be {MAX_CUSTOM_RULES} rules or fewer, each under 140 characters."
                ),
            ),
            RoomError::SkipBudgetExhausted => (
                VercelStatusCode::CONFLICT,
                format!(
                    "Out of skips. Each crawl gets {} reshuffles.",
                    decision::MAX_SKIPS_PER_CRAWL
                ),
            ),
            RoomError::ModeMismatch => (
                VercelStatusCode::CONFLICT,
                "That action isn't available in this room's decision mode.".to_string(),
            ),
            RoomError::NameRequired => (
                VercelStatusCode::BAD_REQUEST,
                "Display name is required.".to_string(),
            ),
            RoomError::NameTooLong => (
                VercelStatusCode::BAD_REQUEST,
                "Display name is too long (max 32 chars).".to_string(),
            ),
            RoomError::InvalidCode => (
                VercelStatusCode::BAD_REQUEST,
                "Room code is invalid.".to_string(),
            ),
            // Generic: the full validator output is logged, never returned,
            // so the API can't be used to probe the schema.
            RoomError::Validation(_) => (
                VercelStatusCode::BAD_REQUEST,
                "Invalid request parameters.".to_string(),
            ),
            // A dependency outage, not our bug, so 503 and retry.
            RoomError::Storage(_) => (
                VercelStatusCode::SERVICE_UNAVAILABLE,
                "Service temporarily unavailable. Please try again in a moment.".to_string(),
            ),
            // Redis is fine, the write just lost a race.
            RoomError::Contention(_) => (
                VercelStatusCode::CONFLICT,
                "Another change landed first. Please try again.".to_string(),
            ),
            // Host double-tap or a second tab. Short wait.
            RoomError::StartInProgress => (
                VercelStatusCode::CONFLICT,
                "The host is already starting this crawl. Hang tight.".to_string(),
            ),
            // Nothing the client can do, so no retry hint.
            RoomError::Invariant(_) => (
                VercelStatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong on our end.".to_string(),
            ),
            RoomError::Crawl(inner) => inner.to_response(),
        }
    }

    /// `Retry-After` in seconds, where one makes sense. Any value beats
    /// letting the client retry immediately.
    pub fn retry_after_seconds(&self) -> Option<u64> {
        match self {
            RoomError::Storage(_) => Some(5),
            // Self-healing, so a 1s nudge is plenty.
            RoomError::Contention(_) => Some(1),
            // Long enough for an in-flight upstream call to land, short
            // enough to beat `START_LOCK_TTL_SECONDS`.
            RoomError::StartInProgress => Some(5),
            _ => None,
        }
    }
}

impl From<redis::RedisError> for RoomError {
    fn from(err: redis::RedisError) -> Self {
        RoomError::Storage(err.to_string())
    }
}

impl From<CustomRulesError> for RoomError {
    fn from(err: CustomRulesError) -> Self {
        RoomError::CustomRules(err)
    }
}

impl From<validator::ValidationErrors> for RoomError {
    fn from(err: validator::ValidationErrors) -> Self {
        RoomError::Validation(err)
    }
}

/// One connection per warm instance, shared by every request that lands on
/// it. Cloning is cheap and the connection pipelines internally.
static REDIS_CONN: OnceCell<redis::aio::MultiplexedConnection> = OnceCell::const_new();

/// The cached Redis connection, opened on first call.
pub async fn connect() -> Result<redis::aio::MultiplexedConnection, RoomError> {
    let conn = REDIS_CONN
        .get_or_try_init(|| async {
            let url = std::env::var("REDIS_URL")
                .map_err(|_| RoomError::Storage("REDIS_URL not set".to_string()))?;
            let client = redis::Client::open(url)?;
            let conn = client.get_multiplexed_async_connection().await?;
            Ok::<_, RoomError>(conn)
        })
        .await?;
    Ok(conn.clone())
}

fn generate_code<R: Rng>(rng: &mut R) -> String {
    (0..CODE_LEN)
        .map(|_| {
            let i = rng.gen_range(0..CODE_ALPHABET.len());
            CODE_ALPHABET[i] as char
        })
        .collect()
}

/// Uppercase, 6 characters, all from our alphabet.
pub fn is_valid_code(code: &str) -> bool {
    code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b))
}

/// Check each `custom_pool` entry: coordinates in range, and JSON size under
/// [`MAX_CUSTOM_POOL_ENTRY_BYTES`]. The struct's `length(max = 50)` caps the
/// count, but without a per-entry cap one huge entry would ride through every
/// Lua decode the pool sees. `start_room_vote` re-checks coordinates anyway.
fn validate_custom_pool(pool: &[crate::roulette::PubResponse]) -> Result<(), RoomError> {
    for (idx, entry) in pool.iter().enumerate() {
        if !(-90.0..=90.0).contains(&entry.latitude) || !(-180.0..=180.0).contains(&entry.longitude)
        {
            return Err(RoomError::InvalidCustomPool { index: idx });
        }
        // Paying this once at create time bounds the cost of every later
        // round-trip through cjson.
        let bytes = serde_json::to_vec(entry)
            .map_err(|e| RoomError::Storage(format!("serialize custom_pool entry: {e}")))?
            .len();
        if bytes > MAX_CUSTOM_POOL_ENTRY_BYTES {
            return Err(RoomError::CustomPoolEntryTooLarge { index: idx, bytes });
        }
    }
    Ok(())
}

fn validate_name(raw: &str) -> Result<String, RoomError> {
    let name = raw.trim().to_string();
    if name.is_empty() {
        return Err(RoomError::NameRequired);
    }
    if name.chars().count() > 32 {
        return Err(RoomError::NameTooLong);
    }
    Ok(name)
}

/// Store the room only if the code is free. `Ok(false)` means it was taken.
async fn write_room_nx(
    conn: &mut redis::aio::MultiplexedConnection,
    room: &Room,
) -> Result<bool, RoomError> {
    let key = room_key(&room.code);
    let json = serde_json::to_string(room)
        .map_err(|e| RoomError::Storage(format!("serialize room: {e}")))?;
    let result: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg(json)
        .arg("NX")
        .arg("EX")
        .arg(ROOM_TTL_SECONDS)
        .query_async(conn)
        .await?;
    Ok(result.is_some())
}

pub async fn get_room(
    conn: &mut redis::aio::MultiplexedConnection,
    code: &str,
) -> Result<Room, RoomError> {
    if !is_valid_code(code) {
        return Err(RoomError::InvalidCode);
    }
    let key = room_key(code);
    let raw: Option<String> = conn.get(&key).await?;
    let raw = raw.ok_or(RoomError::NotFound)?;
    let room: Room = serde_json::from_str(&raw)
        .map_err(|e| RoomError::Storage(format!("deserialize room: {e}")))?;
    Ok(room)
}

fn parse_room_json(json: &str) -> Result<Room, RoomError> {
    serde_json::from_str(json).map_err(|e| RoomError::Storage(format!("deserialize room: {e}")))
}

/// Map a Lua script's `redis.error_reply('CODE')` back to a typed error.
/// Matching on `contains` tolerates whatever prefix Redis adds.
fn map_lua_err(err: redis::RedisError) -> RoomError {
    let s = err.to_string();
    if s.contains("NOT_FOUND") {
        RoomError::NotFound
    } else if s.contains("NOT_HOST") {
        RoomError::NotHost
    } else if s.contains("NOT_MEMBER") {
        RoomError::NotMember
    } else if s.contains("ROOM_FULL") {
        RoomError::RoomFull
    } else if s.contains("ALREADY_STARTED") {
        RoomError::AlreadyStarted
    } else if s.contains("NOT_STARTED") {
        RoomError::NotStarted
    } else if s.contains("INVALID_CANDIDATE") {
        RoomError::InvalidCandidate
    } else if s.contains("NO_OPEN_ROUND") {
        RoomError::NoOpenRound
    } else if s.contains("ROUND_EXPIRED") {
        RoomError::RoundExpired
    } else {
        RoomError::Storage(s)
    }
}

pub async fn create_room(
    conn: &mut redis::aio::MultiplexedConnection,
    host_device_id: String,
    host_name: String,
    settings: RoomSettings,
) -> Result<Room, RoomError> {
    settings.validate()?;
    // Checks `validator` can't express over an `Option<Vec<PubResponse>>`.
    // Failing here keeps a bad pool from ever being persisted.
    if let Some(pool) = settings.custom_pool.as_ref() {
        validate_custom_pool(pool)?;
    }
    // Normalise up front so the room holds exactly what gets dealt. Checked
    // whenever the field is present, so an odd deck/rules pair can't smuggle
    // an unbounded list through.
    let mut settings = settings;
    settings.custom_rules = match settings.custom_rules.as_deref() {
        Some(rules) => Some(validate_custom_rules(rules)?),
        None => None,
    };
    // A `Custom` deck with no rules would silently deal nothing.
    if settings
        .house_rules_deck
        .is_some_and(|d| d.needs_custom_rules())
        && settings.custom_rules.is_none()
    {
        return Err(RoomError::CustomRules(CustomRulesError::Empty));
    }
    let settings = settings;
    let host_name = validate_name(&host_name)?;

    let now = Utc::now();
    let host = Member {
        device_id: host_device_id.clone(),
        name: host_name,
        joined_at: now,
    };

    for attempt in 0..MAX_CREATE_ATTEMPTS {
        // ThreadRng is !Send, so it must not cross the await below.
        let code = {
            let mut rng = rand::thread_rng();
            generate_code(&mut rng)
        };
        let room = Room {
            code: code.clone(),
            host_device_id: host_device_id.clone(),
            created_at: now,
            updated_at: now,
            status: RoomStatus::Lobby,
            settings: settings.clone(),
            members: vec![host.clone()],
            pubs: None,
            current_index: 0,
            house_rules: None,
            pool: None,
            current_round: None,
            skips_used: 0,
            finish_reason: None,
            version: 1,
        };

        if write_room_nx(conn, &room).await? {
            info!(code = %code, host = %host_device_id, "room_created");
            return Ok(room);
        }

        warn!(code = %code, attempt, "room_code_collision");
    }

    Err(RoomError::Contention(
        "exhausted room code attempts".to_string(),
    ))
}

pub async fn join_room(
    conn: &mut redis::aio::MultiplexedConnection,
    code: &str,
    device_id: String,
    name: String,
) -> Result<Room, RoomError> {
    let name = validate_name(&name)?;
    let now = now_rfc3339_millis(Utc::now());
    let json: String = redis::Script::new(LUA_JOIN)
        .key(room_key(code))
        .arg(&device_id)
        .arg(&name)
        .arg(&now)
        .arg(MAX_MEMBERS.to_string())
        .arg(ROOM_TTL_SECONDS.to_string())
        .invoke_async(conn)
        .await
        .map_err(map_lua_err)?;
    let room = parse_room_json(&json)?;
    info!(code = %code, device = %device_id, members = room.members.len(), "room_joined");
    Ok(room)
}

/// [`join_room`] for lobby and active rooms alike, so a member can re-enter
/// mid-crawl. Also the path the client's silent token refresh uses.
pub async fn rejoin_room(
    conn: &mut redis::aio::MultiplexedConnection,
    code: &str,
    device_id: String,
    name: String,
) -> Result<Room, RoomError> {
    let name = validate_name(&name)?;
    let now = now_rfc3339_millis(Utc::now());
    let json: String = redis::Script::new(LUA_REJOIN)
        .key(room_key(code))
        .arg(&device_id)
        .arg(&name)
        .arg(&now)
        .arg(MAX_MEMBERS.to_string())
        .arg(ROOM_TTL_SECONDS.to_string())
        .invoke_async(conn)
        .await
        .map_err(map_lua_err)?;
    let room = parse_room_json(&json)?;
    info!(code = %code, device = %device_id, status = ?room.status, members = room.members.len(), "room_rejoined");
    Ok(room)
}

pub async fn leave_room(
    conn: &mut redis::aio::MultiplexedConnection,
    code: &str,
    device_id: &str,
) -> Result<Option<Room>, RoomError> {
    let now = now_rfc3339_millis(Utc::now());
    let result: Result<String, redis::RedisError> = redis::Script::new(LUA_LEAVE)
        .key(room_key(code))
        .arg(device_id)
        .arg(&now)
        .arg(ROOM_TTL_SECONDS.to_string())
        .invoke_async(conn)
        .await;
    match result {
        Ok(json) => {
            let room = parse_room_json(&json)?;
            info!(code = %code, device = %device_id, members = room.members.len(), "room_left");
            Ok(Some(room))
        }
        Err(e) if e.to_string().contains("ROOM_DELETED") => {
            info!(code = %code, "room_emptied_and_deleted");
            Ok(None)
        }
        Err(e) => Err(map_lua_err(e)),
    }
}

pub async fn start_room(
    conn: &mut redis::aio::MultiplexedConnection,
    geo_config: &GeoapifyConfig,
    code: &str,
    device_id: &str,
) -> Result<Room, RoomError> {
    // Check before paying for the upstream call.
    let initial = get_room(conn, code).await?;
    if initial.host_device_id != device_id {
        return Err(RoomError::NotHost);
    }
    if initial.status != RoomStatus::Lobby {
        return Err(RoomError::AlreadyStarted);
    }

    // Single-flight lock. Without it a double-tap fires two paid upstream
    // calls and only one wins the CAS. Every exit path below releases it; the
    // TTL only covers a panic or timeout.
    if !acquire_start_lock(conn, code).await? {
        warn!(code = %code, "start_lock_contended");
        return Err(RoomError::StartInProgress);
    }

    let result = match initial.settings.decision_mode {
        DecisionMode::Dictator => {
            start_room_dictator(conn, geo_config, code, device_id, initial).await
        }
        DecisionMode::Vote => start_room_vote(conn, geo_config, code, device_id, initial).await,
    };

    // Release either way: on failure it lets a real retry happen straight
    // away. Errors are ignored so they can't mask the start result, and the
    // TTL still cleans up.
    release_start_lock(conn, code).await;

    result
}

fn start_lock_key(code: &str) -> String {
    format!("rooms:start_lock:{code}")
}

/// Take the start lock. `Ok(false)` means someone else holds it. A Redis
/// error surfaces rather than soft-failing, so an outage can't quietly
/// double-charge the upstream budget.
async fn acquire_start_lock(
    conn: &mut redis::aio::MultiplexedConnection,
    code: &str,
) -> Result<bool, RoomError> {
    let key = start_lock_key(code);
    let result: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(START_LOCK_TTL_SECONDS)
        .query_async(conn)
        .await?;
    Ok(result.is_some())
}

/// Drop the start lock. Failures are logged; the TTL is the safety net.
async fn release_start_lock(conn: &mut redis::aio::MultiplexedConnection, code: &str) {
    let key = start_lock_key(code);
    let res: redis::RedisResult<i64> = conn.del(&key).await;
    if let Err(e) = res {
        warn!(code = %code, error = %e, "start_lock_release_failed");
    }
}

/// Dictator start: the server picks every pub up front.
async fn start_room_dictator(
    conn: &mut redis::aio::MultiplexedConnection,
    geo_config: &GeoapifyConfig,
    code: &str,
    device_id: &str,
    initial: Room,
) -> Result<Room, RoomError> {
    let filters = PlaceFilters {
        beer_garden: initial.settings.beer_garden,
    };
    let pubs = get_random_crawl(
        geo_config,
        initial.settings.num_pubs,
        initial.settings.latitude,
        initial.settings.longitude,
        initial.settings.radius,
        initial.settings.bearing,
        initial.settings.mode,
        &[],
        &filters,
    )
    .await
    .map_err(RoomError::Crawl)?;

    // Roll before the CAS loop so a retry can't reshuffle what the host
    // already saw. One rule per pub actually returned, which may be fewer
    // than asked for. ThreadRng is !Send, so keep its scope tight.
    let house_rules = initial.settings.house_rules_deck.map(|deck| {
        let custom = initial.settings.custom_rules.as_deref().unwrap_or(&[]);
        let mut rng = rand::thread_rng();
        roll_rules(deck, custom, pubs.len(), &mut rng)
    });

    // If a join or leave bumped the version, re-read and retry without
    // re-fetching pubs. Serialising in Rust keeps cjson away from the floats.
    let mut current = initial;
    for attempt in 0..START_ROOM_CAS_RETRIES {
        if attempt > 0 {
            current = get_room(conn, code).await?;
            if current.host_device_id != device_id {
                return Err(RoomError::NotHost);
            }
            if current.status != RoomStatus::Lobby {
                return Err(RoomError::AlreadyStarted);
            }
        }

        let mut new_room = current.clone();
        new_room.pubs = Some(pubs.clone());
        new_room.house_rules = house_rules.clone();
        new_room.current_index = 0;
        new_room.status = RoomStatus::Active;
        bump(&mut new_room);

        let new_json = serde_json::to_string(&new_room)
            .map_err(|e| RoomError::Storage(format!("serialize room: {e}")))?;

        let result: Result<String, redis::RedisError> = redis::Script::new(LUA_START_CAS)
            .key(room_key(code))
            .arg(device_id)
            .arg(current.version.to_string())
            .arg(&new_json)
            .arg(ROOM_TTL_SECONDS.to_string())
            .invoke_async(conn)
            .await;

        match result {
            Ok(_) => {
                info!(code = %code, mode = "dictator", num_pubs = new_room.settings.num_pubs, "room_started");
                return Ok(new_room);
            }
            Err(e) if e.to_string().contains("VERSION_CONFLICT") => {
                warn!(code = %code, attempt, "start_room_cas_conflict_retry");
                continue;
            }
            Err(e) => return Err(map_lua_err(e)),
        }
    }

    Err(RoomError::Contention(
        "start_room: too many concurrent modifications, try again".to_string(),
    ))
}

/// Vote-mode start: fetch the pool, open round 0, go active.
async fn start_room_vote(
    conn: &mut redis::aio::MultiplexedConnection,
    geo_config: &GeoapifyConfig,
    code: &str,
    device_id: &str,
    initial: Room,
) -> Result<Room, RoomError> {
    let filters = PlaceFilters {
        beer_garden: initial.settings.beer_garden,
    };

    let pool = if let Some(custom) = initial
        .settings
        .custom_pool
        .as_ref()
        .filter(|v| !v.is_empty())
    {
        // A junk coordinate here would break distances and map rendering
        // later.
        for (idx, entry) in custom.iter().enumerate() {
            if !(-90.0..=90.0).contains(&entry.latitude)
                || !(-180.0..=180.0).contains(&entry.longitude)
            {
                return Err(RoomError::InvalidCustomPool { index: idx });
            }
        }
        custom.clone()
    } else {
        fetch_pool(
            geo_config,
            initial.settings.latitude,
            initial.settings.longitude,
            initial.settings.radius,
            initial.settings.bearing,
            initial.settings.mode,
            &filters,
        )
        .await
        .map_err(RoomError::Crawl)?
    };

    let candidates_per_round =
        effective_candidates_per_round(initial.settings.candidates_per_round) as usize;
    let num_pubs = initial.settings.num_pubs as usize;

    // The pool has to cover `num_pubs` rounds. A sparse radius can trip this.
    if pool.len() < num_pubs {
        return Err(RoomError::Crawl(
            crate::roulette::AppError::NotEnoughFound {
                requested: initial.settings.num_pubs,
                found: pool.len(),
                mode: initial.settings.mode,
            },
        ));
    }

    // Rolled outside the CAS loop so a retry can't reshuffle the candidates
    // the host already saw.
    let (candidates, opened_at, expires_at) = {
        let mut rng = rand::thread_rng();
        let picks = sample_weighted_pubs(&pool, candidates_per_round, &HashSet::new(), &mut rng);
        let candidates: Vec<PubResponse> = picks.into_iter().cloned().collect();
        let opened_at = Utc::now();
        let round_secs = effective_round_seconds(initial.settings.round_seconds);
        let expires_at = round_expires_at(opened_at, round_secs);
        (candidates, opened_at, expires_at)
    };

    if candidates.is_empty() {
        return Err(RoomError::Crawl(crate::roulette::AppError::NoneFound {
            mode: initial.settings.mode,
        }));
    }

    let round = Round {
        index: 0,
        candidates,
        opened_at,
        expires_at,
        votes: None,
        resolved: None,
    };

    let mut current = initial;
    for attempt in 0..START_ROOM_CAS_RETRIES {
        if attempt > 0 {
            current = get_room(conn, code).await?;
            if current.host_device_id != device_id {
                return Err(RoomError::NotHost);
            }
            if current.status != RoomStatus::Lobby {
                return Err(RoomError::AlreadyStarted);
            }
        }

        let mut new_room = current.clone();
        // `None`, not `Some([])`: lua-cjson writes an empty table as `{}`,
        // which Rust can't read back as a Vec.
        new_room.pubs = None;
        new_room.house_rules = None;
        new_room.pool = Some(pool.clone());
        new_room.current_round = Some(round.clone());
        new_room.current_index = 0;
        new_room.status = RoomStatus::Active;
        bump(&mut new_room);

        let new_json = serde_json::to_string(&new_room)
            .map_err(|e| RoomError::Storage(format!("serialize room: {e}")))?;

        let result: Result<String, redis::RedisError> = redis::Script::new(LUA_START_CAS)
            .key(room_key(code))
            .arg(device_id)
            .arg(current.version.to_string())
            .arg(&new_json)
            .arg(ROOM_TTL_SECONDS.to_string())
            .invoke_async(conn)
            .await;

        match result {
            Ok(_) => {
                info!(
                    code = %code,
                    mode = "vote",
                    num_pubs = new_room.settings.num_pubs,
                    pool_size = pool.len(),
                    candidates_per_round,
                    "room_started"
                );
                return Ok(new_room);
            }
            Err(e) if e.to_string().contains("VERSION_CONFLICT") => {
                warn!(code = %code, attempt, "start_room_cas_conflict_retry");
                continue;
            }
            Err(e) => return Err(map_lua_err(e)),
        }
    }

    Err(RoomError::Contention(
        "start_room: too many concurrent modifications, try again".to_string(),
    ))
}

pub async fn advance_room(
    conn: &mut redis::aio::MultiplexedConnection,
    code: &str,
    device_id: &str,
) -> Result<Room, RoomError> {
    // Costs one extra GET, which beats maintaining per-mode Lua scripts.
    let room = get_room(conn, code).await?;
    if room.host_device_id != device_id {
        return Err(RoomError::NotHost);
    }
    match room.settings.decision_mode {
        DecisionMode::Dictator => advance_room_dictator(conn, code, device_id).await,
        DecisionMode::Vote => advance_room_vote(conn, code, room).await,
    }
}

async fn advance_room_dictator(
    conn: &mut redis::aio::MultiplexedConnection,
    code: &str,
    device_id: &str,
) -> Result<Room, RoomError> {
    let now = now_rfc3339_millis(Utc::now());
    let json: String = redis::Script::new(LUA_ADVANCE)
        .key(room_key(code))
        .arg(device_id)
        .arg(&now)
        .arg(ROOM_TTL_SECONDS.to_string())
        .invoke_async(conn)
        .await
        .map_err(map_lua_err)?;
    let room = parse_room_json(&json)?;
    info!(
        code = %code,
        index = room.current_index,
        status = ?room.status,
        "room_advanced"
    );
    Ok(room)
}

/// Vote-mode advance: close the round, commit the winner, then open the next
/// one or finish. One CAS write, so concurrent votes aren't lost.
async fn advance_room_vote(
    conn: &mut redis::aio::MultiplexedConnection,
    code: &str,
    initial: Room,
) -> Result<Room, RoomError> {
    if initial.status != RoomStatus::Active {
        return Err(RoomError::NotStarted);
    }
    if initial.current_round.is_none() {
        // Vote-mode rooms drop `current_round` once the last one resolves.
        return Err(RoomError::AlreadyAtEnd);
    }

    let mut current = initial;
    for attempt in 0..CAS_RETRIES {
        if attempt > 0 {
            current = get_room(conn, code).await?;
            if current.settings.decision_mode != DecisionMode::Vote {
                return Err(RoomError::ModeMismatch);
            }
            if current.status != RoomStatus::Active {
                return Err(RoomError::NotStarted);
            }
            if current.current_round.is_none() {
                return Err(RoomError::AlreadyAtEnd);
            }
        }

        let new_room = match resolve_round(&current, Utc::now()) {
            Ok(r) => r,
            Err(e) => return Err(e),
        };

        match write_cas(conn, code, current.version, &new_room).await {
            Ok(()) => {
                info!(
                    code = %code,
                    index = new_room.current_index,
                    status = ?new_room.status,
                    pubs = new_room.pubs.as_ref().map(|p| p.len()).unwrap_or(0),
                    "room_round_resolved"
                );
                return Ok(new_room);
            }
            Err(CasError::Conflict) => {
                warn!(code = %code, attempt, "advance_vote_cas_conflict_retry");
                continue;
            }
            Err(CasError::Storage(e)) => return Err(e),
        }
    }

    Err(RoomError::Contention(
        "advance_room: too many concurrent modifications, try again".to_string(),
    ))
}

/// Host-only: open the next round on a room whose previous one is resolved.
/// Same CAS-with-retry shape as `advance_room_vote`.
pub async fn open_next_round_for_code(
    conn: &mut redis::aio::MultiplexedConnection,
    code: &str,
    device_id: &str,
) -> Result<Room, RoomError> {
    let mut current = get_room(conn, code).await?;
    if current.host_device_id != device_id {
        return Err(RoomError::NotHost);
    }
    if current.settings.decision_mode != DecisionMode::Vote {
        return Err(RoomError::ModeMismatch);
    }

    for attempt in 0..CAS_RETRIES {
        if attempt > 0 {
            current = get_room(conn, code).await?;
            if current.host_device_id != device_id {
                return Err(RoomError::NotHost);
            }
            if current.settings.decision_mode != DecisionMode::Vote {
                return Err(RoomError::ModeMismatch);
            }
        }

        let new_room = match open_next_round(&current, Utc::now()) {
            Ok(r) => r,
            Err(e) => return Err(e),
        };

        match write_cas(conn, code, current.version, &new_room).await {
            Ok(()) => {
                info!(
                    code = %code,
                    index = new_room
                        .current_round
                        .as_ref()
                        .map(|r| r.index)
                        .unwrap_or(0),
                    status = ?new_room.status,
                    "room_next_round_opened"
                );
                return Ok(new_room);
            }
            Err(CasError::Conflict) => {
                warn!(code = %code, attempt, "open_next_round_cas_conflict_retry");
                continue;
            }
            Err(CasError::Storage(e)) => return Err(e),
        }
    }

    Err(RoomError::Contention(
        "open_next_round: too many concurrent modifications, try again".to_string(),
    ))
}

/// Cast an approval vote. `vote.lua` checks membership, expiry and candidate
/// validity atomically; this just parses what comes back.
pub async fn vote_room(
    conn: &mut redis::aio::MultiplexedConnection,
    code: &str,
    device_id: &str,
    approvals: &[String],
) -> Result<Room, RoomError> {
    if !is_valid_code(code) {
        return Err(RoomError::InvalidCode);
    }
    // Reject before serialising, so an oversized payload never reaches
    // `cjson.decode` and slows the room down for everyone.
    if approvals.len() > MAX_APPROVALS_PER_VOTE
        || approvals.iter().any(|s| s.len() > MAX_APPROVAL_STRING_LEN)
    {
        let longest = approvals.iter().map(|s| s.len()).max().unwrap_or(0);
        warn!(
            code = %code,
            device = %device_id,
            count = approvals.len(),
            longest,
            "vote_approvals_oversized"
        );
        return Err(RoomError::ApprovalsTooLong {
            count: approvals.len(),
            max_len: longest,
        });
    }
    // Canonical form, so two identical votes store identical bytes.
    let mut cleaned: Vec<String> = approvals.to_vec();
    cleaned.sort();
    cleaned.dedup();

    let approvals_json = serde_json::to_string(&cleaned)
        .map_err(|e| RoomError::Storage(format!("serialize approvals: {e}")))?;
    let now = now_rfc3339_millis(Utc::now());

    let json: String = redis::Script::new(LUA_VOTE)
        .key(room_key(code))
        .arg(device_id)
        .arg(&approvals_json)
        .arg(&now)
        .arg(ROOM_TTL_SECONDS.to_string())
        .invoke_async(conn)
        .await
        .map_err(map_lua_err)?;
    let room = parse_room_json(&json)?;
    info!(
        code = %code,
        device = %device_id,
        approvals = cleaned.len(),
        "room_voted"
    );
    Ok(room)
}

/// Host-only: resample the current round, excluding visited pubs and the
/// candidates just rejected. Budgeted per crawl.
pub async fn skip_round(
    conn: &mut redis::aio::MultiplexedConnection,
    code: &str,
    device_id: &str,
) -> Result<Room, RoomError> {
    let mut current = get_room(conn, code).await?;
    if current.host_device_id != device_id {
        return Err(RoomError::NotHost);
    }
    if current.settings.decision_mode != DecisionMode::Vote {
        return Err(RoomError::ModeMismatch);
    }
    if current.status != RoomStatus::Active {
        return Err(RoomError::NotStarted);
    }

    for attempt in 0..CAS_RETRIES {
        if attempt > 0 {
            current = get_room(conn, code).await?;
            if current.host_device_id != device_id {
                return Err(RoomError::NotHost);
            }
            if current.settings.decision_mode != DecisionMode::Vote {
                return Err(RoomError::ModeMismatch);
            }
        }

        if current.skips_used >= decision::MAX_SKIPS_PER_CRAWL {
            return Err(RoomError::SkipBudgetExhausted);
        }
        let Some(round) = current.current_round.as_ref() else {
            return Err(RoomError::NoOpenRound);
        };
        if round.resolved.is_some() {
            return Err(RoomError::NoOpenRound);
        }

        let Some(pool) = current.pool.as_ref() else {
            // A stale wire shape, not a transient failure.
            return Err(RoomError::Invariant(
                "skip_round: vote-mode room has no pool".to_string(),
            ));
        };

        let exclude = build_exclude_set(&current, Some(round));
        let candidates_per_round =
            effective_candidates_per_round(current.settings.candidates_per_round) as usize;

        let (new_candidates, opened_at, expires_at) = {
            let mut rng = rand::thread_rng();
            let picks = sample_weighted_pubs(pool, candidates_per_round, &exclude, &mut rng);
            if picks.is_empty() {
                // Nothing left to resample, so skipping again won't help.
                return Err(RoomError::Crawl(crate::roulette::AppError::NoneFound {
                    mode: current.settings.mode,
                }));
            }
            let candidates: Vec<PubResponse> = picks.into_iter().cloned().collect();
            let opened_at = Utc::now();
            let round_secs = effective_round_seconds(current.settings.round_seconds);
            let expires_at = round_expires_at(opened_at, round_secs);
            (candidates, opened_at, expires_at)
        };

        let mut new_room = current.clone();
        let next_round = Round {
            index: round.index,
            candidates: new_candidates,
            opened_at,
            expires_at,
            votes: None,
            resolved: None,
        };
        new_room.current_round = Some(next_round);
        new_room.skips_used += 1;
        bump(&mut new_room);

        match write_cas(conn, code, current.version, &new_room).await {
            Ok(()) => {
                info!(
                    code = %code,
                    skips_used = new_room.skips_used,
                    "round_skipped"
                );
                return Ok(new_room);
            }
            Err(CasError::Conflict) => {
                warn!(code = %code, attempt, "skip_round_cas_conflict_retry");
                continue;
            }
            Err(CasError::Storage(e)) => return Err(e),
        }
    }

    Err(RoomError::Contention(
        "skip_round: too many concurrent modifications, try again".to_string(),
    ))
}

/// Resolve an expired round that nobody advanced, so the group isn't stranded
/// when the host loses signal. A no-op, with no write, when nothing expired.
pub async fn resolve_room_if_expired(
    conn: &mut redis::aio::MultiplexedConnection,
    code: &str,
) -> Result<Room, RoomError> {
    let initial = get_room(conn, code).await?;
    if initial.settings.decision_mode != DecisionMode::Vote {
        return Ok(initial);
    }
    if initial.status != RoomStatus::Active {
        return Ok(initial);
    }
    let now = Utc::now();
    let needs_resolve = initial
        .current_round
        .as_ref()
        .map(|r| r.is_expired(now))
        .unwrap_or(false);
    if !needs_resolve {
        return Ok(initial);
    }

    let mut current = initial;
    for attempt in 0..CAS_RETRIES {
        if attempt > 0 {
            current = get_room(conn, code).await?;
            // Someone else resolved it first; keep their winner.
            let still_expired = current
                .current_round
                .as_ref()
                .map(|r| r.is_expired(Utc::now()))
                .unwrap_or(false);
            if !still_expired {
                return Ok(current);
            }
        }

        // Only close the round. Opening the next one stays manual so the
        // group still sees who won before voting again.
        let new_room = match resolve_round(&current, Utc::now()) {
            Ok(r) => r,
            Err(e) => return Err(e),
        };

        match write_cas(conn, code, current.version, &new_room).await {
            Ok(()) => {
                info!(
                    code = %code,
                    index = new_room.current_index,
                    status = ?new_room.status,
                    "round_auto_resolved"
                );
                return Ok(new_room);
            }
            Err(CasError::Conflict) => {
                warn!(code = %code, attempt, "auto_resolve_cas_conflict_retry");
                continue;
            }
            Err(CasError::Storage(e)) => return Err(e),
        }
    }

    // Out of retries: return what we read rather than failing a read-only
    // endpoint. The next caller will resolve it.
    get_room(conn, code).await
}

/// Close the open round and commit its winner to `pubs[]`. Opening the next
/// round is [`open_next_round`]'s job, so the client can show the result
/// first. On the last pub the room finishes, keeping the resolved round for
/// the client to render.
fn resolve_round(room: &Room, now: DateTime<Utc>) -> Result<Room, RoomError> {
    let Some(round) = room.current_round.as_ref() else {
        return Err(RoomError::AlreadyAtEnd);
    };
    if round.resolved.is_some() {
        // Re-resolving would re-roll the rng and could pick a different
        // winner, so keep what's already there.
        return Err(RoomError::NoOpenRound);
    }

    let mut rng = rand::thread_rng();

    let resolved = decision::resolve(round, now, &mut rng).ok_or_else(|| {
        // An open round always has candidates, so this room is corrupt.
        RoomError::Invariant("resolve: round has no candidates".to_string())
    })?;

    let mut new_room = room.clone();
    let mut pubs = new_room.pubs.unwrap_or_default();
    pubs.push(resolved.winner.clone());
    let total_pubs = new_room.settings.num_pubs as usize;
    let is_last = pubs.len() >= total_pubs;
    new_room.pubs = Some(pubs);

    if let Some(deck) = new_room.settings.house_rules_deck {
        let custom = new_room.settings.custom_rules.clone().unwrap_or_default();
        let mut history = new_room.house_rules.unwrap_or_default();
        // Ordered decks index into the deal for the finished crawl, so pass
        // the target length, not the committed one.
        history.push(roll_one(deck, &custom, &history, total_pubs, &mut rng));
        new_room.house_rules = Some(history);
    }

    // Keep the resolved round so clients can render the outcome.
    let mut finished_round = round.clone();
    finished_round.resolved = Some(resolved);
    new_room.current_round = Some(finished_round);
    new_room.current_index = new_room
        .pubs
        .as_ref()
        .map(|p| p.len().saturating_sub(1))
        .unwrap_or(0);
    if is_last {
        new_room.status = RoomStatus::Finished;
        new_room.finish_reason = Some(FinishReason::Completed);
    }

    bump(&mut new_room);
    Ok(new_room)
}

/// Open the next round on a room that's between rounds: active, with a
/// resolved round, and short of `num_pubs`. Anything else belongs to
/// `start_room`, an open vote, or a finished crawl.
fn open_next_round(room: &Room, now: DateTime<Utc>) -> Result<Room, RoomError> {
    if room.status != RoomStatus::Active {
        return Err(RoomError::NotStarted);
    }
    let Some(prev) = room.current_round.as_ref() else {
        return Err(RoomError::NoOpenRound);
    };
    if prev.resolved.is_none() {
        return Err(RoomError::RoundStillOpen);
    }
    let committed = room.pubs.as_ref().map(|p| p.len()).unwrap_or(0);
    let total = room.settings.num_pubs as usize;
    if committed >= total {
        return Err(RoomError::AlreadyAtEnd);
    }

    let pool = room
        .pool
        .as_ref()
        .ok_or_else(|| RoomError::Invariant("open_next: vote-mode room has no pool".to_string()))?;
    let exclude = build_exclude_set(room, None);
    let candidates_per_round =
        effective_candidates_per_round(room.settings.candidates_per_round) as usize;

    let mut rng = rand::thread_rng();
    let picks = sample_weighted_pubs(pool, candidates_per_round, &exclude, &mut rng);
    let mut new_room = room.clone();

    if picks.is_empty() {
        // Finish early rather than open an empty round. `finish_reason` is
        // what tells the client to suggest a wider radius.
        new_room.status = RoomStatus::Finished;
        new_room.finish_reason = Some(FinishReason::PoolExhausted);
        warn!(code = %new_room.code, "pool_exhausted_finishing_early");
        bump(&mut new_room);
        return Ok(new_room);
    }

    let candidates: Vec<PubResponse> = picks.into_iter().cloned().collect();
    let opened_at = now;
    let round_secs = effective_round_seconds(new_room.settings.round_seconds);
    let expires_at = round_expires_at(opened_at, round_secs);
    let next_index = prev.index.saturating_add(1);
    new_room.current_round = Some(Round {
        index: next_index,
        candidates,
        opened_at,
        expires_at,
        votes: None,
        resolved: None,
    });
    bump(&mut new_room);
    Ok(new_room)
}

/// place_ids to keep out of the next round: everything visited, plus the
/// candidates of a round being replaced by a skip.
fn build_exclude_set(room: &Room, also_exclude_round: Option<&Round>) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(pubs) = room.pubs.as_ref() {
        for p in pubs {
            if let Some(pid) = p.place_id.as_ref() {
                set.insert(pid.clone());
            }
        }
    }
    if let Some(round) = also_exclude_round {
        for c in &round.candidates {
            if let Some(pid) = c.place_id.as_ref() {
                set.insert(pid.clone());
            }
        }
    }
    set
}

fn round_expires_at(opened_at: DateTime<Utc>, round_seconds: u32) -> DateTime<Utc> {
    if round_seconds == 0 {
        // Manual close: park the deadline at the room's TTL so Lua always
        // reads the round as open.
        opened_at + chrono::Duration::seconds(ROOM_TTL_SECONDS as i64)
    } else {
        opened_at + chrono::Duration::seconds(round_seconds as i64)
    }
}

#[derive(Debug)]
enum CasError {
    Conflict,
    Storage(RoomError),
}

async fn write_cas(
    conn: &mut redis::aio::MultiplexedConnection,
    code: &str,
    expected_version: u64,
    new_room: &Room,
) -> Result<(), CasError> {
    let new_json = serde_json::to_string(new_room)
        .map_err(|e| CasError::Storage(RoomError::Storage(format!("serialize room: {e}"))))?;
    let result: Result<String, redis::RedisError> = redis::Script::new(LUA_CAS_WRITE)
        .key(room_key(code))
        .arg(expected_version.to_string())
        .arg(&new_json)
        .arg(ROOM_TTL_SECONDS.to_string())
        .invoke_async(conn)
        .await;
    match result {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("VERSION_CONFLICT") => Err(CasError::Conflict),
        Err(e) => Err(CasError::Storage(map_lua_err(e))),
    }
}

fn bump(room: &mut Room) {
    room.version += 1;
    room.updated_at = Utc::now();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::ResolvedRound;
    use rand::SeedableRng;
    use std::collections::BTreeMap;

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

    #[test]
    fn generated_code_is_correct_length_and_alphabet() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        for _ in 0..100 {
            let code = generate_code(&mut rng);
            assert_eq!(code.len(), CODE_LEN);
            assert!(code.bytes().all(|b| CODE_ALPHABET.contains(&b)));
        }
    }

    #[test]
    fn generated_codes_avoid_ambiguous_chars() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        for _ in 0..1000 {
            let code = generate_code(&mut rng);
            for c in code.chars() {
                assert!(!"01OI".contains(c), "ambiguous char in {code}");
            }
        }
    }

    #[test]
    fn is_valid_code_accepts_real_codes() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        for _ in 0..50 {
            let code = generate_code(&mut rng);
            assert!(is_valid_code(&code), "rejected own output: {code}");
        }
    }

    #[test]
    fn is_valid_code_rejects_garbage() {
        assert!(!is_valid_code(""));
        assert!(!is_valid_code("ABC"));
        assert!(!is_valid_code("ABCDEFG"));
        assert!(!is_valid_code("abcdef")); // lowercase
        assert!(!is_valid_code("ABCDE0")); // forbidden 0
        assert!(!is_valid_code("ABCDE1")); // forbidden 1
        assert!(!is_valid_code("ABCDEO")); // forbidden O
        assert!(!is_valid_code("ABCDEI")); // forbidden I
        assert!(!is_valid_code("ABCD!2"));
    }

    #[test]
    fn validate_name_trims_and_rejects_empty() {
        assert_eq!(validate_name("  Jamie  ").unwrap(), "Jamie");
        assert!(matches!(validate_name(""), Err(RoomError::NameRequired)));
        assert!(matches!(validate_name("   "), Err(RoomError::NameRequired)));
    }

    #[test]
    fn validate_name_enforces_max_length() {
        let ok = "a".repeat(32);
        let too_long = "a".repeat(33);
        assert!(validate_name(&ok).is_ok());
        assert!(matches!(
            validate_name(&too_long),
            Err(RoomError::NameTooLong)
        ));
    }

    #[test]
    fn settings_validation_catches_bad_input() {
        let mut s = settings();
        s.latitude = 200.0;
        assert!(s.validate().is_err());

        let mut s = settings();
        s.num_pubs = 99;
        assert!(s.validate().is_err());

        let mut s = settings();
        s.radius = 0;
        assert!(s.validate().is_err());
    }

    #[test]
    fn room_error_status_codes() {
        assert_eq!(RoomError::NotFound.to_response().0.as_u16(), 404);
        assert_eq!(RoomError::NotHost.to_response().0.as_u16(), 403);
        assert_eq!(RoomError::RoomFull.to_response().0.as_u16(), 409);
        assert_eq!(RoomError::AlreadyStarted.to_response().0.as_u16(), 409);
        assert_eq!(RoomError::NotStarted.to_response().0.as_u16(), 409);
        assert_eq!(RoomError::InvalidCode.to_response().0.as_u16(), 400);
        assert_eq!(RoomError::NameRequired.to_response().0.as_u16(), 400);
        // A dependency outage is 503, and the details stay server-side.
        let (status, msg) = RoomError::Storage("connection refused".to_string()).to_response();
        assert_eq!(status.as_u16(), 503);
        assert!(!msg.contains("connection refused"));
        let (status, msg) = RoomError::Contention("retry exhausted".to_string()).to_response();
        assert_eq!(status.as_u16(), 409);
        assert!(!msg.contains("retry exhausted"));
        let (status, msg) = RoomError::Invariant("no pool".to_string()).to_response();
        assert_eq!(status.as_u16(), 500);
        assert!(!msg.contains("no pool"));
    }

    #[test]
    fn start_in_progress_is_409_with_meaningful_retry_after() {
        // Long enough for the in-flight call to land, short enough that a
        // stuck start doesn't hold the host for the whole lock TTL.
        let (status, msg) = RoomError::StartInProgress.to_response();
        assert_eq!(status.as_u16(), 409);
        assert!(!msg.is_empty(), "should carry a user-facing message");
        assert_eq!(RoomError::StartInProgress.retry_after_seconds(), Some(5));
    }

    #[test]
    fn transient_errors_advertise_retry_after() {
        assert_eq!(
            RoomError::Storage("redis down".to_string()).retry_after_seconds(),
            Some(5)
        );
        assert_eq!(
            RoomError::Contention("retry".to_string()).retry_after_seconds(),
            Some(1)
        );
        assert_eq!(
            RoomError::Invariant("bug".to_string()).retry_after_seconds(),
            None
        );
        assert_eq!(RoomError::NotFound.retry_after_seconds(), None);
        assert_eq!(RoomError::NotHost.retry_after_seconds(), None);
        assert_eq!(RoomError::AlreadyStarted.retry_after_seconds(), None);
        assert_eq!(RoomError::InvalidCode.retry_after_seconds(), None);
    }

    #[test]
    fn validation_error_message_does_not_leak_field_names() {
        // A real validator error, so the actual Display impl runs.
        let mut bad = settings();
        bad.latitude = 999.0;
        bad.num_pubs = 99;
        let raw = bad.validate().unwrap_err();
        let raw_display = format!("{raw}");
        assert!(raw_display.contains("latitude") || raw_display.contains("num_pubs"));

        let (status, msg) = RoomError::Validation(raw).to_response();
        assert_eq!(status.as_u16(), 400);
        // The client message must not name fields or constraints.
        assert!(!msg.contains("latitude"));
        assert!(!msg.contains("num_pubs"));
        assert!(!msg.contains("range"));
        assert_eq!(msg, "Invalid request parameters.");
    }

    fn pub_response(place_id: &str, distance: Option<f64>) -> PubResponse {
        PubResponse {
            name: format!("Pub {place_id}"),
            latitude: 51.5,
            longitude: -0.1,
            place_id: Some(place_id.to_string()),
            address_line1: None,
            address_line2: None,
            phone: None,
            website: None,
            opening_hours: None,
            wikidata_id: None,
            distance_meters: distance,
        }
    }

    fn vote_settings() -> RoomSettings {
        let mut s = settings();
        s.decision_mode = DecisionMode::Vote;
        s
    }

    fn make_vote_room(num_pubs: i16) -> Room {
        let now = Utc::now();
        let mut s = vote_settings();
        s.num_pubs = num_pubs;
        let pool: Vec<PubResponse> = (0..(num_pubs as usize + 4))
            .map(|i| pub_response(&format!("pid{i}"), Some(100.0 * (i + 1) as f64)))
            .collect();
        let candidates = pool[..3].to_vec();
        Room {
            code: "ABCDEF".to_string(),
            host_device_id: "h".to_string(),
            created_at: now,
            updated_at: now,
            status: RoomStatus::Active,
            settings: s,
            members: vec![Member {
                device_id: "h".to_string(),
                name: "Host".to_string(),
                joined_at: now,
            }],
            // `None`, for the same lua-cjson reason as `start_room_vote`.
            pubs: None,
            current_index: 0,
            house_rules: None,
            pool: Some(pool),
            current_round: Some(Round {
                index: 0,
                candidates,
                opened_at: now,
                expires_at: now + chrono::Duration::seconds(60),
                votes: None,
                resolved: None,
            }),
            skips_used: 0,
            finish_reason: None,
            version: 1,
        }
    }

    #[test]
    fn room_serializes_round_trip() {
        let now = Utc::now();
        let room = Room {
            code: "ABCDEF".to_string(),
            host_device_id: "h".to_string(),
            created_at: now,
            updated_at: now,
            status: RoomStatus::Lobby,
            settings: settings(),
            members: vec![Member {
                device_id: "h".to_string(),
                name: "Host".to_string(),
                joined_at: now,
            }],
            pubs: None,
            current_index: 0,
            house_rules: None,
            pool: None,
            current_round: None,
            skips_used: 0,
            finish_reason: None,
            version: 1,
        };
        let json = serde_json::to_string(&room).unwrap();
        let parsed: Room = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.code, "ABCDEF");
        assert_eq!(parsed.members.len(), 1);
        assert_eq!(parsed.status, RoomStatus::Lobby);
        assert!(matches!(
            parsed.settings.decision_mode,
            DecisionMode::Dictator
        ));
        assert!(parsed.pool.is_none());
        assert!(parsed.current_round.is_none());
        assert_eq!(parsed.skips_used, 0);
        assert!(parsed.finish_reason.is_none());
        // Blobs written before `finish_reason` existed must still parse.
    }

    #[test]
    fn vote_room_serializes_round_trip() {
        let room = make_vote_room(3);
        let json = serde_json::to_string(&room).unwrap();
        let parsed: Room = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed.settings.decision_mode, DecisionMode::Vote));
        assert_eq!(parsed.pool.as_ref().map(|p| p.len()), Some(7));
        let round = parsed.current_round.expect("round persisted");
        assert_eq!(round.candidates.len(), 3);
        assert!(round.votes.is_none());
        assert!(round.resolved.is_none());
    }

    #[test]
    fn vote_room_start_state_has_no_empty_arrays() {
        // An empty Vec would come back from Lua as `{}` and fail to parse, so
        // the room written at start must contain no empty arrays.
        let room = make_vote_room(3);
        let json = serde_json::to_string(&room).unwrap();
        // `pubs` and `house_rules` are the only ones that could be empty.
        assert!(
            !json.contains("\"pubs\":[]"),
            "pubs must not be serialised as [], use null to avoid lua-cjson ambiguity"
        );
        assert!(
            !json.contains("\"house_rules\":[]"),
            "house_rules must not be serialised as [], use null to avoid lua-cjson ambiguity"
        );
    }

    #[test]
    fn legacy_room_without_engine_fields_deserialises_as_dictator() {
        // A room stored without any of the vote-mode fields.
        let now_str = now_rfc3339_millis(Utc::now());
        let legacy = serde_json::json!({
            "code": "LEGACY",
            "host_device_id": "h",
            "created_at": now_str,
            "updated_at": now_str,
            "status": "lobby",
            "settings": {
                "latitude": 55.94,
                "longitude": -3.18,
                "radius": 500,
                "bearing": null,
                "num_pubs": 3,
                "mode": "pubs",
                "beer_garden": false,
                "house_rules_deck": null,
            },
            "members": [{"device_id": "h", "name": "Host", "joined_at": now_str}],
            "pubs": null,
            "current_index": 0,
            "house_rules": null,
            "version": 1,
        });
        let parsed: Room = serde_json::from_value(legacy).expect("legacy parse");
        assert!(matches!(
            parsed.settings.decision_mode,
            DecisionMode::Dictator
        ));
        assert_eq!(parsed.skips_used, 0);
        assert!(parsed.pool.is_none());
        assert!(parsed.current_round.is_none());
    }

    #[test]
    fn resolve_round_commits_winner_but_does_not_open_next() {
        let mut room = make_vote_room(3);
        let mut votes = BTreeMap::new();
        votes.insert("dev1".to_string(), vec!["pid0".to_string()]);
        votes.insert("dev2".to_string(), vec!["pid0".to_string()]);
        room.current_round.as_mut().unwrap().votes = Some(votes);

        let next = resolve_round(&room, Utc::now()).expect("resolves");
        assert_eq!(next.pubs.as_ref().unwrap().len(), 1);
        assert_eq!(
            next.pubs.as_ref().unwrap()[0].place_id.as_deref(),
            Some("pid0")
        );
        // The resolved round stays; opening the next one is separate.
        let same_round = next.current_round.as_ref().expect("round kept for display");
        assert_eq!(same_round.index, 0);
        assert!(same_round.resolved.is_some());
        assert_eq!(next.status, RoomStatus::Active);
    }

    #[test]
    fn resolve_round_rejects_already_resolved() {
        // Resolving twice must not re-roll and pick a new winner.
        let mut room = make_vote_room(3);
        room.current_round.as_mut().unwrap().resolved = Some(ResolvedRound {
            winner: pub_response("pid0", Some(100.0)),
            tally: vec![],
            tiebroken: false,
            resolved_at: Utc::now(),
        });
        let err = resolve_round(&room, Utc::now()).expect_err("must reject");
        assert!(matches!(err, RoomError::NoOpenRound), "got {err:?}");
    }

    #[test]
    fn open_next_round_opens_after_resolve() {
        let mut room = make_vote_room(3);
        room.current_round.as_mut().unwrap().votes = Some(
            [("dev1".to_string(), vec!["pid0".to_string()])]
                .into_iter()
                .collect(),
        );
        let resolved = resolve_round(&room, Utc::now()).expect("resolves");
        let opened = open_next_round(&resolved, Utc::now()).expect("opens");
        let new_round = opened.current_round.as_ref().expect("next round opened");
        assert_eq!(new_round.index, 1);
        assert!(new_round.resolved.is_none());
        assert!(new_round.votes.is_none());
    }

    #[test]
    fn open_next_round_rejects_when_round_unresolved() {
        // Opening before the current round resolves must error cleanly.
        let room = make_vote_room(3);
        let err = open_next_round(&room, Utc::now()).expect_err("must reject");
        assert!(matches!(err, RoomError::RoundStillOpen), "got {err:?}");
    }

    #[test]
    fn open_next_round_rejects_when_at_end() {
        // A one-pub crawl finishes on resolve, so there's no next round.
        let mut room = make_vote_room(1);
        room.current_round.as_mut().unwrap().votes = Some(
            [("dev1".to_string(), vec!["pid0".to_string()])]
                .into_iter()
                .collect(),
        );
        let resolved = resolve_round(&room, Utc::now()).expect("resolves");
        assert_eq!(resolved.status, RoomStatus::Finished);
        let err = open_next_round(&resolved, Utc::now()).expect_err("must reject");
        // Anything but Active reports as not started.
        assert!(matches!(err, RoomError::NotStarted), "got {err:?}");
    }

    #[test]
    fn resolve_finishes_room_on_last_pub() {
        let mut room = make_vote_room(1);
        room.current_round.as_mut().unwrap().votes = Some(
            [("dev1".to_string(), vec!["pid0".to_string()])]
                .into_iter()
                .collect(),
        );
        let next = resolve_round(&room, Utc::now()).expect("resolves");
        assert_eq!(next.status, RoomStatus::Finished);
        // A natural finish carries `Completed`.
        assert_eq!(next.finish_reason, Some(FinishReason::Completed));
        let last_round = next.current_round.as_ref().expect("kept for display");
        assert!(last_round.resolved.is_some());
    }

    #[test]
    fn open_next_round_excludes_visited_from_candidates() {
        // Round 1 must not re-show the pub round 0 just committed.
        let mut room = make_vote_room(3);
        let already = pub_response("pid1", Some(200.0));
        room.pubs = Some(vec![already]);
        room.current_round.as_mut().unwrap().index = 1;

        room.current_round.as_mut().unwrap().votes = Some(
            [("dev1".to_string(), vec!["pid0".to_string()])]
                .into_iter()
                .collect(),
        );

        let resolved = resolve_round(&room, Utc::now()).expect("resolves");
        let opened = open_next_round(&resolved, Utc::now()).expect("opens");
        let new_round = opened.current_round.as_ref().expect("next round opened");
        // Neither the new winner nor the already-visited pub may appear.
        for c in &new_round.candidates {
            let pid = c.place_id.as_deref().unwrap();
            assert!(pid != "pid0" && pid != "pid1", "excluded pid leaked: {pid}");
        }
    }

    #[test]
    fn open_next_round_finishes_with_pool_exhausted_when_resampling_empty() {
        // A 5-pub crawl with a 1-entry pool: after round 0 the exclude set
        // covers everything, so it should finish early as `PoolExhausted`
        // rather than report `AlreadyAtEnd`.
        let mut room = make_vote_room(5);
        let only_entry = pub_response("pid0", Some(100.0));
        room.pool = Some(vec![only_entry.clone()]);
        room.current_round.as_mut().unwrap().candidates = vec![only_entry];
        room.current_round.as_mut().unwrap().votes = Some(
            [("dev1".to_string(), vec!["pid0".to_string()])]
                .into_iter()
                .collect(),
        );
        let resolved = resolve_round(&room, Utc::now()).expect("resolves");
        assert_eq!(resolved.pubs.as_ref().unwrap().len(), 1);
        assert_eq!(resolved.status, RoomStatus::Active);

        let opened = open_next_round(&resolved, Utc::now()).expect("opens");
        assert_eq!(opened.status, RoomStatus::Finished);
        assert_eq!(opened.finish_reason, Some(FinishReason::PoolExhausted));
    }

    #[test]
    fn round_expires_at_zero_seconds_yields_far_future() {
        let now = Utc::now();
        let exp = round_expires_at(now, 0);
        // The deadline parks a day out, so only the host can close it.
        assert!(exp > now + chrono::Duration::hours(23));
    }

    #[test]
    fn round_expires_at_normal_seconds() {
        let now = Utc::now();
        let exp = round_expires_at(now, 60);
        let delta = (exp - now).num_seconds();
        assert!((59..=61).contains(&delta), "got {delta}");
    }

    #[test]
    fn validate_custom_pool_rejects_oversized_entry() {
        // The count limit would pass this, so it pins the per-entry cap.
        let bloated = PubResponse {
            name: "x".repeat(MAX_CUSTOM_POOL_ENTRY_BYTES * 2),
            latitude: 51.5,
            longitude: -0.1,
            place_id: Some("p".to_string()),
            address_line1: None,
            address_line2: None,
            phone: None,
            website: None,
            opening_hours: None,
            wikidata_id: None,
            distance_meters: None,
        };
        let err = validate_custom_pool(std::slice::from_ref(&bloated)).unwrap_err();
        assert!(
            matches!(err, RoomError::CustomPoolEntryTooLarge { index: 0, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn custom_rules_settings_round_trip_over_the_wire() {
        let mut s = settings();
        s.house_rules_deck = Some(HouseRulesDeck::Custom);
        s.custom_rules = Some(vec!["The Hat Pub: hats on.".to_string()]);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"house_rules_deck\":\"custom\""), "{json}");
        let back: RoomSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.custom_rules, s.custom_rules);
    }

    #[test]
    fn custom_rules_omitted_from_wire_when_unset() {
        // Same reasoning as `custom_pool`: keep the poll payload small.
        let json = serde_json::to_string(&settings()).unwrap();
        assert!(
            !json.contains("custom_rules"),
            "custom_rules must be skipped when None: {json}"
        );
    }

    #[test]
    fn custom_rules_errors_are_actionable_400s() {
        // These name what the host typed wrong, so they can fix it.
        let (status, body) = RoomError::CustomRules(CustomRulesError::Empty).to_response();
        assert_eq!(status, VercelStatusCode::BAD_REQUEST);
        assert!(body.contains("at least one"), "{body}");

        let (status, body) =
            RoomError::CustomRules(CustomRulesError::TooMany { count: 99 }).to_response();
        assert_eq!(status, VercelStatusCode::BAD_REQUEST);
        assert!(body.contains(&MAX_CUSTOM_RULES.to_string()), "{body}");
        assert!(
            !body.contains("99"),
            "must not echo the caller's count: {body}"
        );

        let (status, _) = RoomError::CustomRules(CustomRulesError::RuleTooLong {
            index: 0,
            chars: 5000,
        })
        .to_response();
        assert_eq!(status, VercelStatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_custom_pool_rejects_out_of_range_coords() {
        let bad = PubResponse {
            name: "ok".to_string(),
            latitude: 999.0,
            longitude: 0.0,
            place_id: Some("p".to_string()),
            address_line1: None,
            address_line2: None,
            phone: None,
            website: None,
            opening_hours: None,
            wikidata_id: None,
            distance_meters: None,
        };
        let err = validate_custom_pool(std::slice::from_ref(&bad)).unwrap_err();
        assert!(
            matches!(err, RoomError::InvalidCustomPool { index: 0 }),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_custom_pool_accepts_realistic_entry() {
        // A realistic entry must pass comfortably.
        let realistic = PubResponse {
            name: "The Ship Inn at the End of the Universe".to_string(),
            latitude: 55.94,
            longitude: -3.18,
            place_id: Some("place_abc123".to_string()),
            address_line1: Some("123 Long Road Name".to_string()),
            address_line2: Some("Edinburgh, EH1 1AA, United Kingdom".to_string()),
            phone: Some("+44 131 555 5555".to_string()),
            website: Some("https://example.co.uk/the-ship-inn".to_string()),
            opening_hours: Some("Mo-Fr 11:00-23:00; Sa-Su 10:00-01:00".to_string()),
            wikidata_id: Some("Q12345".to_string()),
            distance_meters: Some(284.3),
        };
        validate_custom_pool(std::slice::from_ref(&realistic)).expect("must pass");
    }

    #[test]
    fn custom_pool_error_codes_are_generic_400() {
        let (status, msg) = RoomError::CustomPoolEntryTooLarge {
            index: 7,
            bytes: 999_999,
        }
        .to_response();
        assert_eq!(status.as_u16(), 400);
        // The response must not reveal the threshold.
        assert!(!msg.contains("999"));
        assert!(!msg.contains("7"));
    }

    #[test]
    fn settings_validation_clamps_candidates_per_round() {
        let mut s = vote_settings();
        s.candidates_per_round = Some(99);
        assert!(s.validate().is_err());
        s.candidates_per_round = Some(0);
        assert!(s.validate().is_err());
        s.candidates_per_round = Some(3);
        assert!(s.validate().is_ok());
    }

    #[test]
    fn settings_validation_clamps_round_seconds() {
        let mut s = vote_settings();
        s.round_seconds = Some(999);
        assert!(s.validate().is_err());
        s.round_seconds = Some(0); // manual close is allowed
        assert!(s.validate().is_ok());
        s.round_seconds = Some(60);
        assert!(s.validate().is_ok());
    }

    #[test]
    fn vote_mode_room_error_status_codes() {
        assert_eq!(RoomError::NotMember.to_response().0.as_u16(), 403);
        assert_eq!(RoomError::InvalidCandidate.to_response().0.as_u16(), 400);
        assert_eq!(RoomError::NoOpenRound.to_response().0.as_u16(), 409);
        assert_eq!(RoomError::RoundExpired.to_response().0.as_u16(), 409);
        assert_eq!(RoomError::SkipBudgetExhausted.to_response().0.as_u16(), 409);
        assert_eq!(RoomError::ModeMismatch.to_response().0.as_u16(), 409);
        // Generic 400: the sizes go to the logs, not the client.
        let (status, msg) = RoomError::ApprovalsTooLong {
            count: 1024,
            max_len: 1_000_000,
        }
        .to_response();
        assert_eq!(status.as_u16(), 400);
        assert_eq!(msg, "Invalid request parameters.");
        assert!(!msg.contains("1024"));
        assert!(!msg.contains("1000000"));
    }

    // The script itself is covered end-to-end by the integration suite. These
    // simulate its member-list mutation on a plain `Room`.

    fn simulate_rejoin(room: &Room, device_id: &str, name: &str) -> Room {
        let mut r = room.clone();
        let already = r.members.iter_mut().find(|m| m.device_id == device_id);
        match already {
            Some(m) => m.name = name.to_string(),
            None => r.members.push(Member {
                device_id: device_id.to_string(),
                name: name.to_string(),
                joined_at: Utc::now(),
            }),
        }
        r
    }

    #[test]
    fn rejoin_adds_new_member_to_active_room() {
        let mut room = make_vote_room(3);
        room.status = RoomStatus::Active;
        let updated = simulate_rejoin(&room, "late_joiner", "Alex");
        assert_eq!(updated.members.len(), 2);
        assert!(
            updated
                .members
                .iter()
                .any(|m| m.device_id == "late_joiner" && m.name == "Alex")
        );
    }

    #[test]
    fn rejoin_refreshes_name_for_existing_member() {
        let room = make_vote_room(3);
        let updated = simulate_rejoin(&room, "h", "HostRenamed");
        assert_eq!(updated.members.len(), 1, "no duplicate added");
        assert_eq!(updated.members[0].name, "HostRenamed");
    }

    #[test]
    fn rejoin_works_for_lobby_room() {
        let mut room = make_vote_room(3);
        room.status = RoomStatus::Lobby;
        let updated = simulate_rejoin(&room, "friend", "Bob");
        assert!(updated.members.iter().any(|m| m.name == "Bob"));
    }

    #[test]
    fn rejoin_validate_name_still_applies() {
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name("Valid Name").is_ok());
    }
}
