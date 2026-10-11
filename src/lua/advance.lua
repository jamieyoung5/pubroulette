-- Atomically advance the crawl to the next pub. If we're already on the last
-- pub, the room transitions to Finished.
--
-- KEYS[1] = room key
-- ARGV[1] = device_id (must equal room.host_device_id)
-- ARGV[2] = now (RFC3339)
-- ARGV[3] = ttl_seconds
--
-- Returns: the new room JSON.
-- Errors:
--   NOT_FOUND    - room no longer exists
--   NOT_HOST     - caller is not the host
--   NOT_STARTED  - room isn't Active, or has no pubs to walk through

local raw = redis.call('GET', KEYS[1])
if not raw then return redis.error_reply('NOT_FOUND') end
local room = cjson.decode(raw)
local device_id, now, ttl = ARGV[1], ARGV[2], tonumber(ARGV[3])

if room.host_device_id ~= device_id then return redis.error_reply('NOT_HOST') end
if room.status ~= 'active' then return redis.error_reply('NOT_STARTED') end

-- room.pubs is null before start, which cjson decodes as userdata.
local total = 0
if type(room.pubs) == 'table' then total = #room.pubs end
if total == 0 then return redis.error_reply('NOT_STARTED') end

if room.current_index + 1 >= total then
    room.current_index = total - 1
    room.status = 'finished'
    -- In dictator mode the last pub is the only way to finish. Vote mode
    -- sets this in Rust.
    room.finish_reason = 'completed'
else
    room.current_index = room.current_index + 1
end

room.version = room.version + 1
room.updated_at = now
local json = cjson.encode(room)
redis.call('SET', KEYS[1], json, 'EX', ttl)
return json
