-- Atomically upsert a member's approval vote in the current round.
--
-- A vote is a device's full approval set, so `[]` clears it. An emptied votes
-- map goes back to null, because cjson can't tell `{}` from `[]`.
--
-- KEYS[1] = room key
-- ARGV[1] = device_id
-- ARGV[2] = approvals (JSON array of place_id strings; may be `[]`)
-- ARGV[3] = now (RFC3339)
-- ARGV[4] = ttl_seconds
--
-- Returns: the new room JSON on success.
-- Errors:
--   NOT_FOUND          - room no longer exists (expired or deleted)
--   NOT_MEMBER         - caller's device_id is not in the room
--   NO_OPEN_ROUND      - room has no current_round, or it's already resolved
--   ROUND_EXPIRED      - `now` is at or past current_round.expires_at
--   INVALID_CANDIDATE  - at least one approval references a place_id not on
--                        the current candidate list

local raw = redis.call('GET', KEYS[1])
if not raw then return redis.error_reply('NOT_FOUND') end
local room = cjson.decode(raw)
local device_id, now = ARGV[1], ARGV[3]
local ttl = tonumber(ARGV[4])

-- The host counts as a member here.
local is_member = false
for _, m in ipairs(room.members) do
    if m.device_id == device_id then is_member = true; break end
end
if not is_member then return redis.error_reply('NOT_MEMBER') end

-- `current_round` is null with no round open, which decodes as userdata.
if type(room.current_round) ~= 'table' then
    return redis.error_reply('NO_OPEN_ROUND')
end
-- Resolved rounds are read-only.
if type(room.current_round.resolved) == 'table' then
    return redis.error_reply('NO_OPEN_ROUND')
end

-- RFC3339 with a trailing Z compares lexically, which is enough here.
if now >= room.current_round.expires_at then
    return redis.error_reply('ROUND_EXPIRED')
end

-- Reject place_ids that aren't candidates. Rust checks this too.
local approvals = cjson.decode(ARGV[2])
local valid = {}
for _, c in ipairs(room.current_round.candidates) do
    if c.place_id then valid[c.place_id] = true end
end
for _, pid in ipairs(approvals) do
    if not valid[pid] then return redis.error_reply('INVALID_CANDIDATE') end
end

-- Stays null until the first real vote, for the cjson reason above.
if type(room.current_round.votes) ~= 'table' then
    room.current_round.votes = {}
end

if #approvals == 0 then
    -- Empty approvals clear the vote instead of storing an empty set.
    room.current_round.votes[device_id] = nil
    if not next(room.current_round.votes) then
        room.current_round.votes = cjson.null
    end
else
    room.current_round.votes[device_id] = approvals
end

room.version = room.version + 1
room.updated_at = now
local json = cjson.encode(room)
redis.call('SET', KEYS[1], json, 'EX', ttl)
return json
