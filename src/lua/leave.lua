-- Atomically remove a member from the room.
--
-- If the leaver is the last member, the room key is deleted and we signal
-- ROOM_DELETED so the caller can return Ok(None). If the host leaves, the
-- next member in join order is promoted.
--
-- KEYS[1] = room key
-- ARGV[1] = device_id
-- ARGV[2] = now (RFC3339)
-- ARGV[3] = ttl_seconds
--
-- Returns: the new room JSON when a member leaves but others remain, or the
--          unchanged room JSON when the device_id wasn't a member.
-- Errors:
--   NOT_FOUND     - room no longer exists
--   ROOM_DELETED  - signalled when the leaver was the last member (room key
--                   has been DELed by this script)

local raw = redis.call('GET', KEYS[1])
if not raw then return redis.error_reply('NOT_FOUND') end
local room = cjson.decode(raw)
local device_id, now, ttl = ARGV[1], ARGV[2], tonumber(ARGV[3])

local kept = {}
local was_member = false
for _, m in ipairs(room.members) do
    if m.device_id == device_id then
        was_member = true
    else
        table.insert(kept, m)
    end
end

-- Leaving a room you weren't in is a no-op.
if not was_member then
    return cjson.encode(room)
end

if #kept == 0 then
    redis.call('DEL', KEYS[1])
    return redis.error_reply('ROOM_DELETED')
end

-- Members are in join order, so kept[1] is the longest-serving survivor.
if room.host_device_id == device_id then
    room.host_device_id = kept[1].device_id
end

room.members = kept
room.version = room.version + 1
room.updated_at = now
local json = cjson.encode(room)
redis.call('SET', KEYS[1], json, 'EX', ttl)
return json
