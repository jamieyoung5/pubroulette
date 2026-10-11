# pubroulette

The serverless Rust backend behind [pubroulette.com](https://pubroulette.com). A person or group
cannot decide which pub to go to, so the app picks one for them.

This is a public, read-only mirror of the private API repo, published as a code sample.
Deployment configuration and secrets are not included.

## What it does

- **Solo / duo play.** `POST /api/play` returns one random nearby pub; `POST /api/crawl` returns N pubs
  ordered for walking.
- **Multiplayer rooms.** A host creates a room, friends join with a 6-character code, and the group does a
  shared crawl. Each stop is decided either host-dictator style or by per-stop approval voting, with optional
  "house rules" cards dealt to players.

Supporting endpoints: a Nominatim search proxy (`/api/search`), a health check, per-IP rate limiting, and
HMAC bearer tokens scoping every room action.

```
api/health.rs                  -> GET  /api/health
api/play.rs                    -> POST /api/play
api/crawl.rs                   -> POST /api/crawl
api/search.rs                  -> GET  /api/search
api/rooms.rs                   -> POST /api/rooms            (create)
api/rooms/[code].rs            -> GET  /api/rooms/{code}     (poll)
api/rooms/[code]/join.rs       -> POST /api/rooms/{code}/join
api/rooms/[code]/rejoin.rs     -> POST /api/rooms/{code}/rejoin
api/rooms/[code]/leave.rs      -> POST /api/rooms/{code}/leave
api/rooms/[code]/start.rs      -> POST /api/rooms/{code}/start
api/rooms/[code]/advance.rs    -> POST /api/rooms/{code}/advance
api/rooms/[code]/vote.rs       -> POST /api/rooms/{code}/vote
api/rooms/[code]/skip-round.rs -> POST /api/rooms/{code}/skip-round
api/rooms/[code]/next-round.rs -> POST /api/rooms/{code}/next-round
```

## Running locally

Requires a stable Rust toolchain, Docker (for Redis) and a free
[Geoapify](https://www.geoapify.com/) API key.

```sh
cp .env.example .env     # then fill in GEOAPIFY_API_KEY and ROOM_TOKEN_SECRET
make redis-up            # start Redis in Docker
make dev                 # start the local dev server on :3000
make play                # smoke-test POST /api/play
```

`examples/devserver.rs` is an axum router that mounts every handler at the same paths the serverless platform
would, so the whole API runs locally as one process.

## Tests

```sh
make test        # unit + integration tests
make lint        # clippy, warnings as errors
make fmt-check   # formatting
make ci          # everything CI runs
```

221 tests run with no external services. Suites that need a live Redis or a real Geoapify key are marked
`#[ignore]`:

```sh
make redis-up
REDIS_URL=redis://localhost:6379 cargo test -- --ignored   # 55 Redis-backed tests
make test-smoke                                            # hits the real Geoapify API
```

Set `PLACES_CACHE_DISABLED=1` to bypass the cache and go straight upstream.

## License

[AGPL-3.0](LICENSE).
