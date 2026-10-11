-- Compare-and-swap for start_room.
--
-- A script can't call Geoapify, so Rust reads the room, fetches the pubs,
-- serialises the new room, and hands the blob here. That also keeps cjson
-- away from the pub coordinates, which its number formatter would round.
--
-- KEYS[1] = room key
-- ARGV[1] = device_id (must equal room.host_device_id)
-- ARGV[2] = expected_version (the version Rust observed before serialising)
-- ARGV[3] = pre-serialised new room JSON
-- ARGV[4] = ttl_seconds
--
-- Returns: the new room JSON (echoed back).
-- Errors:
--   NOT_FOUND         - room no longer exists
--   NOT_HOST          - caller is not the host
--   ALREADY_STARTED   - room left lobby state since we read it
--   VERSION_CONFLICT  - room was modified between our read and this write;
--                       the Rust caller re-reads and retries

local raw = redis.call('GET', KEYS[1])
if not raw then return redis.error_reply('NOT_FOUND') end
local room = cjson.decode(raw)

if room.host_device_id ~= ARGV[1] then return redis.error_reply('NOT_HOST') end
if room.status ~= 'lobby' then return redis.error_reply('ALREADY_STARTED') end
if room.version ~= tonumber(ARGV[2]) then return redis.error_reply('VERSION_CONFLICT') end

redis.call('SET', KEYS[1], ARGV[3], 'EX', tonumber(ARGV[4]))
return ARGV[3]
