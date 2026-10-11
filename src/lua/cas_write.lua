-- Generic version-checked write.
--
-- Rust builds the new room, which needs an rng and so can't happen in a
-- script, and hands over the serialised blob. `VERSION_CONFLICT` tells the
-- caller to re-read and retry.
--
-- KEYS[1] = room key
-- ARGV[1] = expected_version (the version Rust read before computing)
-- ARGV[2] = pre-serialised new room JSON
-- ARGV[3] = ttl_seconds
--
-- Returns: the new room JSON (echoed back).
-- Errors:
--   NOT_FOUND         - room no longer exists
--   VERSION_CONFLICT  - room was modified between our read and this write

local raw = redis.call('GET', KEYS[1])
if not raw then return redis.error_reply('NOT_FOUND') end
local room = cjson.decode(raw)
if room.version ~= tonumber(ARGV[1]) then
    return redis.error_reply('VERSION_CONFLICT')
end
redis.call('SET', KEYS[1], ARGV[2], 'EX', tonumber(ARGV[3]))
return ARGV[2]
