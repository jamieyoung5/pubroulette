-- Atomically add or update a member, then bump the room's version.
--
-- KEYS[1] = room key (e.g. "room:ABCDEF")
-- ARGV[1] = device_id
-- ARGV[2] = display name (already trimmed/validated in Rust)
-- ARGV[3] = now (RFC3339 string; used for joined_at + updated_at)
-- ARGV[4] = max_members
-- ARGV[5] = ttl_seconds
--
-- Returns: the new room JSON on success.
-- Errors:
--   NOT_FOUND        - room no longer exists (expired or deleted)
--   ALREADY_STARTED  - crawl has started and caller is not an existing member
--   ROOM_FULL        - member cap reached

local raw = redis.call('GET', KEYS[1])
if not raw then return redis.error_reply('NOT_FOUND') end
local room = cjson.decode(raw)
local device_id, name, now = ARGV[1], ARGV[2], ARGV[3]
local max_members = tonumber(ARGV[4])
local ttl = tonumber(ARGV[5])

-- Once active the member list is locked, but an existing member re-joining
-- is just a name refresh.
if room.status ~= 'lobby' then
    local found = false
    for _, m in ipairs(room.members) do
        if m.device_id == device_id then found = true; break end
    end
    if not found then return redis.error_reply('ALREADY_STARTED') end
end

local updated = false
for i, m in ipairs(room.members) do
    if m.device_id == device_id then
        room.members[i].name = name
        updated = true
        break
    end
end

if not updated then
    if #room.members >= max_members then return redis.error_reply('ROOM_FULL') end
    table.insert(room.members, {device_id=device_id, name=name, joined_at=now})
end

room.version = room.version + 1
room.updated_at = now
local json = cjson.encode(room)
redis.call('SET', KEYS[1], json, 'EX', ttl)
return json
